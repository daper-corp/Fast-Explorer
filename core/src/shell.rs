//! Shell-backed actions: file operations (IFileOperation), opening files, the file clipboard.
//! Everything here may block (dialogs, disk, other apps), so call it off the UI thread,
//! except the clipboard functions, which only touch in-memory data.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SUCCESS, HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{
    DROPFILES, DragQueryFileW, FOF_ALLOWUNDO, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath, FOF_NO_UI, FOF_NOCONFIRMMKDIR, FOF_RENAMEONCOLLISION,
    FOFX_ADDUNDORECORD, FileOperation, HDROP, IFileOperation, IShellItem, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
    SHCreateItemFromParsingName, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONQUESTION, MB_YESNO, MessageBoxW, SW_HIDE, SW_SHOWNORMAL};
use windows::core::{HSTRING, Owned, PCWSTR, w};

pub enum Op {
    Copy(Vec<PathBuf>, PathBuf),
    Move(Vec<PathBuf>, PathBuf),
    /// `permanent: false` sends to the Recycle Bin.
    Delete { paths: Vec<PathBuf>, permanent: bool },
    Rename(PathBuf, String),
}

/// Runs `op` through IFileOperation, so progress, conflicts, elevation and the Recycle Bin
/// behave exactly like Explorer. `owner` parents the shell dialogs; `None` runs silently.
/// Returns `Ok(false)` if the user cancelled part of it.
pub fn run(op: &Op, owner: Option<isize>) -> io::Result<bool> {
    with_com(|| unsafe {
        let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        let mut flags = FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR | FOFX_ADDUNDORECORD;
        match owner {
            Some(h) => fo.SetOwnerWindow(HWND(h as _))?,
            None => flags |= FOF_NO_UI,
        }
        match op {
            Op::Copy(src, dst) | Op::Move(src, dst) => {
                // Pasting into the same folder makes "name - 복사본" like Explorer.
                if src.iter().all(|s| s.parent() == Some(dst.as_path())) {
                    flags |= FOF_RENAMEONCOLLISION;
                }
                let dst = item(dst)?;
                for s in src {
                    if matches!(op, Op::Copy(..)) {
                        fo.CopyItem(&item(s)?, &dst, PCWSTR::null(), None)?;
                    } else {
                        fo.MoveItem(&item(s)?, &dst, PCWSTR::null(), None)?;
                    }
                }
            }
            Op::Delete { paths, permanent } => {
                if *permanent {
                    flags.0 &= !FOF_ALLOWUNDO.0;
                }
                for p in paths {
                    fo.DeleteItem(&item(p)?, None)?;
                }
            }
            Op::Rename(p, name) => fo.RenameItem(&item(p)?, &HSTRING::from(name.as_str()), None)?,
        }
        fo.SetOperationFlags(flags)?;
        fo.PerformOperations()?;
        Ok(!fo.GetAnyOperationsAborted()?.as_bool())
    })
}

/// Renames within the same folder. Tries MoveFileExW first (~1 ms, versus ~150 ms for
/// IFileOperation) and never replaces an existing file; falls back to the shell when access is
/// denied so it can offer elevation.
pub fn rename(path: &Path, new_name: &str, owner: Option<isize>) -> io::Result<bool> {
    const INVALID: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
    if new_name.trim().is_empty() || new_name.contains(INVALID) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "이름에 \\ / : * ? \" < > | 를 쓸 수 없음"));
    }
    let dst = path.with_file_name(new_name);
    let r = unsafe {
        MoveFileExW(&HSTRING::from(path.as_os_str()), &HSTRING::from(dst.as_os_str()), MOVE_FILE_FLAGS(0))
    };
    match r {
        Ok(()) => Ok(true),
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            run(&Op::Rename(path.to_path_buf(), new_name.to_string()), owner)
        }
        Err(e) => Err(e.into()),
    }
}

/// The real Program Files folder. Never taken from environment variables, which the user controls.
pub fn program_files() -> io::Result<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None)?;
        let path = p.to_string().map_err(io::Error::other);
        CoTaskMemFree(Some(p.0 as _));
        Ok(PathBuf::from(path?))
    }
}

/// Where the index service is installed (admin-only writable).
pub fn install_dir() -> io::Result<PathBuf> {
    Ok(program_files()?.join("Fast Explorer"))
}

/// Yes/No question box (default: Yes). Blocks, so call it off the UI thread.
pub fn confirm(owner: isize, text: &str) -> bool {
    let r = unsafe { MessageBoxW(Some(HWND(owner as _)), &HSTRING::from(text), w!("Fast Explorer"), MB_YESNO | MB_ICONQUESTION) };
    r == IDYES
}

/// Runs `exe args` as administrator (UAC prompt) and waits; returns its exit code.
/// Declining the prompt is an error (ERROR_CANCELLED).
pub fn run_elevated(exe: &Path, args: &str, owner: isize) -> io::Result<u32> {
    let (file, params) = (HSTRING::from(exe.as_os_str()), HSTRING::from(args));
    with_com(|| unsafe {
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            hwnd: HWND(owner as _),
            lpVerb: w!("runas"),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        ShellExecuteExW(&mut info)?;
        let process = Owned::new(info.hProcess);
        WaitForSingleObject(*process, INFINITE);
        let mut code = 0;
        GetExitCodeProcess(*process, &mut code)?;
        Ok(code)
    })
}

/// Opens a file with its default handler (or the "Open with" picker).
pub fn open(path: &Path) -> io::Result<()> {
    let file = HSTRING::from(path.as_os_str());
    with_com(|| unsafe {
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC,
            lpFile: PCWSTR(file.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        ShellExecuteExW(&mut info)
    })
}

/// Creates "새 폴더", "새 폴더 (2)", ... in `dir`; returns the name used.
pub fn new_folder(dir: &Path) -> io::Result<String> {
    new_item(dir, "새 폴더", "", true)
}

/// Creates an empty "새 텍스트 문서.txt" (or "(2)", ...) in `dir`; returns the name used.
pub fn new_text_file(dir: &Path) -> io::Result<String> {
    new_item(dir, "새 텍스트 문서", ".txt", false)
}

/// Never touches an existing item: the first free name wins.
fn new_item(dir: &Path, base: &str, ext: &str, folder: bool) -> io::Result<String> {
    for n in 1..1000 {
        let name = if n == 1 { format!("{base}{ext}") } else { format!("{base} ({n}){ext}") };
        let path = dir.join(&name);
        let r = match folder {
            true => std::fs::create_dir(&path),
            false => std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map(drop),
        };
        match r {
            Ok(()) => return Ok(name),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{base} 이름을 만들 수 없음")))
}

/// Explorer's own view settings: (show hidden items, show file name extensions).
pub fn explorer_view() -> (bool, bool) {
    let read = |name: PCWSTR| {
        let (mut value, mut size) = (0u32, 4u32);
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"),
                name,
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut value).cast()),
                Some(&mut size),
            )
        };
        (r == ERROR_SUCCESS).then_some(value)
    };
    // Hidden: 1 = show, 2 = don't. HideFileExt: 0 = show. Missing values mean Windows' defaults.
    (read(w!("Hidden")) == Some(1), read(w!("HideFileExt")) == Some(0))
}

/// Puts `paths` on the clipboard as CF_HDROP + "Preferred DropEffect", the format
/// Explorer and other apps read. `owner` must be a window of this process.
pub fn clipboard_set(paths: &[PathBuf], cut: bool, owner: isize) -> io::Result<()> {
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.as_os_str().encode_wide());
        list.push(0);
    }
    list.push(0);

    let header = size_of::<DROPFILES>();
    let drop = DROPFILES { pFiles: header as u32, pt: POINT::default(), fNC: false.into(), fWide: true.into() };
    let mut files = Vec::with_capacity(header + list.len() * 2);
    files.extend_from_slice(unsafe { std::slice::from_raw_parts((&raw const drop).cast::<u8>(), header) });
    files.extend(list.iter().flat_map(|c| c.to_le_bytes()));
    // DROPEFFECT_MOVE for cut, DROPEFFECT_COPY | DROPEFFECT_LINK for copy (what Explorer writes).
    let effect: u32 = if cut { 2 } else { 5 };

    unsafe {
        let _clip = Clipboard::open(owner)?;
        EmptyClipboard()?;
        SetClipboardData(CF_HDROP.0 as u32, Some(global(&files)?))?;
        SetClipboardData(drop_effect_format(), Some(global(&effect.to_le_bytes())?))?;
    }
    Ok(())
}

/// Files on the clipboard and whether they were cut, if any.
pub fn clipboard_get(owner: isize) -> Option<(Vec<PathBuf>, bool)> {
    unsafe {
        let _clip = Clipboard::open(owner).ok()?;
        let hdrop = HDROP(GetClipboardData(CF_HDROP.0 as u32).ok()?.0);
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(n as usize);
        for i in 0..n {
            let mut buf = vec![0u16; DragQueryFileW(hdrop, i, None) as usize + 1];
            let len = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            paths.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        let cut = GetClipboardData(drop_effect_format()).ok().is_some_and(|h| {
            let p = GlobalLock(HGLOBAL(h.0)).cast::<u32>();
            let effect = if p.is_null() { 0 } else { p.read_unaligned() };
            let _ = GlobalUnlock(HGLOBAL(h.0));
            effect & 2 != 0 && effect & 1 == 0
        });
        Some((paths, cut))
    }
}

pub fn clipboard_has_files() -> bool {
    unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() }
}

/// Puts plain text (e.g. copied paths) on the clipboard.
pub fn clipboard_set_text(text: &str, owner: isize) -> io::Result<()> {
    let bytes: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(|c| c.to_le_bytes()).collect();
    unsafe {
        let _clip = Clipboard::open(owner)?;
        EmptyClipboard()?;
        SetClipboardData(CF_UNICODETEXT.0 as u32, Some(global(&bytes)?))?;
    }
    Ok(())
}

/// Empties the clipboard (Explorer does this after pasting a cut).
pub fn clipboard_clear(owner: isize) {
    unsafe {
        if let Ok(_clip) = Clipboard::open(owner) {
            let _ = EmptyClipboard();
        }
    }
}

struct Clipboard;

impl Clipboard {
    unsafe fn open(owner: isize) -> io::Result<Self> {
        unsafe { OpenClipboard(Some(HWND(owner as _)))? };
        Ok(Clipboard)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn drop_effect_format() -> u32 {
    unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) }
}

/// Copies `bytes` into a movable global block; ownership passes to the clipboard.
unsafe fn global(bytes: &[u8]) -> io::Result<HANDLE> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let p = GlobalLock(h).cast::<u8>();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(HANDLE(h.0))
    }
}

fn item(path: &Path) -> windows::core::Result<IShellItem> {
    unsafe { SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None) }
}

fn with_com<T>(f: impl FnOnce() -> windows::core::Result<T>) -> io::Result<T> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()? };
    let r = f();
    unsafe { CoUninitialize() };
    Ok(r?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn file_ops_round_trip() {
        let root = std::env::temp_dir().join(format!("fex-ops-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("x.txt"), "hi").unwrap();

        assert!(run(&Op::Copy(vec![a.join("x.txt")], b.clone()), None).unwrap());
        assert_eq!(fs::read_to_string(b.join("x.txt")).unwrap(), "hi");

        // Same-folder copy gets a new name instead of a conflict.
        assert!(run(&Op::Copy(vec![b.join("x.txt")], b.clone()), None).unwrap());
        assert_eq!(fs::read_dir(&b).unwrap().count(), 2);

        assert!(run(&Op::Rename(a.join("x.txt"), "w.txt".into()), None).unwrap());
        assert!(rename(&a.join("w.txt"), "y.txt", None).unwrap());
        assert!(a.join("y.txt").exists() && !a.join("w.txt").exists());
        // Never overwrites, never escapes the folder.
        fs::write(a.join("keep.txt"), "keep").unwrap();
        assert!(rename(&a.join("y.txt"), "keep.txt", None).is_err());
        assert_eq!(fs::read_to_string(a.join("keep.txt")).unwrap(), "keep");
        assert!(rename(&a.join("y.txt"), r"..\y.txt", None).is_err());

        assert!(run(&Op::Move(vec![a.join("y.txt")], b.clone()), None).unwrap());
        assert!(b.join("y.txt").exists() && !a.join("y.txt").exists());

        assert_eq!(new_folder(&a).unwrap(), "새 폴더");
        assert_eq!(new_folder(&a).unwrap(), "새 폴더 (2)");
        assert_eq!(new_text_file(&a).unwrap(), "새 텍스트 문서.txt");
        assert_eq!(new_text_file(&a).unwrap(), "새 텍스트 문서 (2).txt");

        assert!(run(&Op::Delete { paths: vec![b.clone()], permanent: true }, None).unwrap());
        assert!(!b.exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
