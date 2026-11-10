#![windows_subsystem = "windows"]
//! fex-shell-host: builds and runs shell context menus for Fast Explorer in its own process,
//! so a shell extension that hangs or crashes can't take the main window with it.
//! Requests arrive on stdin, replies go to stdout (see `fex_core::menu`).

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fex_core::menu::{MenuItem, Request};
use windows::Win32::Foundation::{ERROR_CANCELLED, E_FAIL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW, HBITMAP, ReleaseDC,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_NORMAL, CMINVOKECOMMANDINFOEX, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, ILFindLastID, ILFree,
    IShellFolder, SHBindToParent, SHGetDesktopFolder, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetMenuItemCount,
    GetMenuItemInfoW, GetMessageW, HMENU, MENUITEMINFOW, MFS_DEFAULT, MFS_DISABLED, MFT_SEPARATOR, MIIM_BITMAP, MIIM_FTYPE,
    MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SW_SHOWNORMAL, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_INITMENUPOPUP, WNDCLASSW, WS_POPUP,
};
use windows::core::{Error, HSTRING, Interface, PCSTR, PCWSTR, PSTR, PWSTR, Result, w};

const WM_REQUEST: u32 = WM_APP;
const WM_STDIN_CLOSED: u32 = WM_APP + 1;
const CMIC_MASK_UNICODE: u32 = 0x4000;
const LAST_ID: u32 = 0x7FFF;

/// Lines read from stdin, waiting for the window thread.
static QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The last menu built, kept so a later `Invoke` can run one of its commands.
struct Built {
    seq: u64,
    cm: IContextMenu,
    menu: HMENU,
    /// Folder of a background menu; static verbs expand "%V" from it.
    dir: Option<PathBuf>,
}

impl Drop for Built {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.menu);
        }
    }
}

thread_local! {
    static LAST: RefCell<Option<Built>> = const { RefCell::new(None) };
}

fn main() -> Result<()> {
    unsafe {
        OleInitialize(None)?; // STA + OLE: what shell extensions expect
        let instance = GetModuleHandleW(None)?;
        let class = w!("FexShellHost");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        });
        // Hidden top-level window: receives requests (also inside a command's modal loop)
        // and owns any dialog a command opens.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Fast Explorer shell host"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        // The first menu built in a process lacks the newer (IExplorerCommand / packaged-app)
        // items such as "터미널에서 열기"; build and discard one so every real reply is complete.
        let _ = build(hwnd, 0, &Request::Background { seq: 0, folder: std::env::temp_dir() });

        let target = hwnd.0 as isize;
        std::thread::spawn(move || {
            let post = |msg| {
                let _ = PostMessageW(Some(HWND(target as _)), msg, WPARAM(0), LPARAM(0));
            };
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                QUEUE.lock().unwrap().push(line);
                post(WM_REQUEST);
            }
            post(WM_STDIN_CLOSED); // Fast Explorer is gone
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_REQUEST => {
            handle_queue(hwnd);
            LRESULT(0)
        }
        WM_STDIN_CLOSED => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn handle_queue(hwnd: HWND) {
    let lines = std::mem::take(&mut *QUEUE.lock().unwrap());
    let reqs: Vec<Request> = lines.iter().filter_map(|l| Request::decode(l)).collect();
    for (i, req) in reqs.iter().enumerate() {
        // Only the newest query matters: the selection has moved on.
        if req.is_query() && reqs[i + 1..].iter().any(Request::is_query) {
            continue;
        }
        handle(hwnd, req);
    }
}

fn handle(hwnd: HWND, req: &Request) {
    match req {
        Request::Items { seq, .. } | Request::Background { seq, .. } => match build(hwnd, *seq, req) {
            Ok(built) => {
                let mut items = Vec::new();
                unsafe { read_items(&built.cm, built.menu, 0, &mut items) };
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "M\t{seq}\t{}", items.len());
                for item in &items {
                    let _ = writeln!(out, "{}", item.encode());
                }
                let _ = out.flush();
                LAST.with(|l| *l.borrow_mut() = Some(built));
            }
            Err(e) => reply(&format!("X\t{seq}\t{}", one_line(&e.message()))),
        },
        Request::Invoke { seq, id } => {
            // Clone out of LAST: the command may run a modal loop that handles more requests.
            let cm = LAST.with(|l| l.borrow().as_ref().filter(|b| b.seq == *seq).map(|b| (b.cm.clone(), b.dir.clone())));
            let r = match cm {
                Some((cm, dir)) => unsafe { invoke(hwnd, &cm, dir.as_deref(), Verb::Id(*id)) },
                None => Err(Error::new(E_FAIL, "메뉴가 바뀌었습니다. 다시 열어 주세요.")),
            };
            reply_done(*seq, r);
        }
        Request::Verb { seq, verb, .. } => {
            let r = build(hwnd, *seq, req).and_then(|b| unsafe { invoke(hwnd, &b.cm, b.dir.as_deref(), Verb::Name(verb)) });
            reply_done(*seq, r);
        }
    }
}

fn reply(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn reply_done(seq: u64, r: Result<()>) {
    match r {
        Err(e) if e.code() != ERROR_CANCELLED.to_hresult() => reply(&format!("R\t{seq}\t{}", one_line(&e.message()))),
        _ => reply(&format!("R\t{seq}\t")),
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

fn build(hwnd: HWND, seq: u64, req: &Request) -> Result<Built> {
    unsafe {
        let cm = match req {
            Request::Background { folder, .. } => background_menu(hwnd, folder)?,
            Request::Items { paths, .. } | Request::Verb { paths, .. } => item_menu(hwnd, paths)?,
            Request::Invoke { .. } => return Err(E_FAIL.into()),
        };
        let dir = match req {
            Request::Background { folder, .. } => Some(folder.clone()),
            _ => None,
        };
        let built = Built { seq, cm, menu: CreatePopupMenu()?, dir };
        built.cm.QueryContextMenu(built.menu, 0, 1, LAST_ID, CMF_NORMAL).ok()?;
        Ok(built)
    }
}

/// Owned PIDLs, freed on drop.
struct Pidls(Vec<*mut ITEMIDLIST>);

impl Drop for Pidls {
    fn drop(&mut self) {
        for &p in &self.0 {
            unsafe { ILFree(Some(p)) };
        }
    }
}

unsafe fn parse(pidls: &mut Pidls, path: &Path) -> Result<*mut ITEMIDLIST> {
    let mut pidl = std::ptr::null_mut();
    unsafe { SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut pidl, 0, None)? };
    pidls.0.push(pidl);
    Ok(pidl)
}

/// The menu Explorer shows for these items (all in one folder), extensions included.
unsafe fn item_menu(hwnd: HWND, paths: &[PathBuf]) -> Result<IContextMenu> {
    let mut pidls = Pidls(Vec::new());
    for p in paths {
        unsafe { parse(&mut pidls, p)? };
    }
    let first = *pidls.0.first().ok_or_else(|| Error::from(E_FAIL))?;
    unsafe {
        let folder: IShellFolder = SHBindToParent(first, None)?;
        let children: Vec<*const ITEMIDLIST> = pidls.0.iter().map(|&p| ILFindLastID(p) as *const _).collect();
        folder.GetUIObjectOf(hwnd, &children, None)
    }
}

/// The menu for a folder's empty space ("새로 만들기", ...).
unsafe fn background_menu(hwnd: HWND, folder: &Path) -> Result<IContextMenu> {
    let mut pidls = Pidls(Vec::new());
    unsafe {
        let pidl = parse(&mut pidls, folder)?;
        let sf: IShellFolder = SHGetDesktopFolder()?.BindToObject(pidl, None)?;
        sf.CreateViewObject(hwnd)
    }
}

/// Flattens the menu (submenus depth-first) into protocol items.
unsafe fn read_items(cm: &IContextMenu, menu: HMENU, depth: u8, out: &mut Vec<MenuItem>) {
    let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0) as u32;
    for pos in 0..count {
        let mut mii = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_BITMAP | MIIM_FTYPE | MIIM_STATE | MIIM_SUBMENU | MIIM_STRING,
            ..Default::default()
        };
        if unsafe { GetMenuItemInfoW(menu, pos, true, &mut mii) }.is_err() {
            continue;
        }
        let mut buf = vec![0u16; mii.cch as usize + 1];
        if mii.cch > 0 {
            mii.dwTypeData = PWSTR(buf.as_mut_ptr());
            mii.cch += 1;
            let _ = unsafe { GetMenuItemInfoW(menu, pos, true, &mut mii) };
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        // "Paste\tCtrl+V": keep the label, drop the shortcut hint.
        let label = String::from_utf16_lossy(&buf[..end]).split('\t').next().unwrap_or("").to_string();
        let separator = mii.fType.0 & MFT_SEPARATOR.0 != 0;
        let submenu = !mii.hSubMenu.is_invalid();
        if !separator && label.is_empty() {
            continue; // owner-drawn or bitmap-only
        }
        if submenu {
            unsafe { init_popup(cm, mii.hSubMenu, pos) };
        }
        let verb = if !separator && !submenu && (1..=LAST_ID).contains(&mii.wID) {
            unsafe { verb_of(cm, mii.wID - 1) }
        } else {
            String::new()
        };
        out.push(MenuItem {
            depth,
            id: mii.wID,
            separator,
            disabled: mii.fState.0 & MFS_DISABLED.0 != 0,
            default: mii.fState.0 & MFS_DEFAULT.0 != 0,
            submenu,
            verb,
            icon: if separator { String::new() } else { unsafe { bitmap_icon(mii.hbmpItem) } },
            label,
        });
        if submenu && depth < 4 {
            unsafe { read_items(cm, mii.hSubMenu, depth + 1, out) };
        }
    }
}

/// An item bitmap (what Explorer draws left of the text) as [`MenuItem::icon`]; empty if the item
/// has none, or only one of the HBMMENU_* pseudo handles.
unsafe fn bitmap_icon(bmp: HBITMAP) -> String {
    if (bmp.0 as usize) <= 16 {
        return String::new();
    }
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(bmp.into(), size_of::<BITMAP>() as i32, Some((&raw mut bm).cast())) == 0 {
            return String::new();
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 || w > 64 || h > 64 {
            return String::new();
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, bmp, 0, h as u32, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        if lines != h {
            return String::new();
        }
        // Bitmaps without an alpha channel are drawn opaque.
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        }
        MenuItem::pack_icon(w as u32, h as u32, &px)
    }
}

/// Lets handlers fill submenus they populate lazily ("보내기", "새로 만들기", ...).
unsafe fn init_popup(cm: &IContextMenu, sub: HMENU, pos: u32) {
    let (w, l) = (WPARAM(sub.0 as usize), LPARAM(pos as isize));
    unsafe {
        if let Ok(cm3) = cm.cast::<IContextMenu3>() {
            let _ = cm3.HandleMenuMsg2(WM_INITMENUPOPUP, w, l, None);
        } else if let Ok(cm2) = cm.cast::<IContextMenu2>() {
            let _ = cm2.HandleMenuMsg(WM_INITMENUPOPUP, w, l);
        }
    }
}

unsafe fn verb_of(cm: &IContextMenu, offset: u32) -> String {
    let mut buf = [0u16; 64];
    let r = unsafe { cm.GetCommandString(offset as usize, GCS_VERBW, None, PSTR(buf.as_mut_ptr().cast()), buf.len() as u32) };
    match r {
        Ok(()) => String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]),
        Err(_) => String::new(),
    }
}

enum Verb<'a> {
    Id(u32),
    Name(&'a str),
}

unsafe fn invoke(hwnd: HWND, cm: &IContextMenu, dir: Option<&Path>, verb: Verb) -> Result<()> {
    let (ansi, wide);
    let dir = dir.map(|d| HSTRING::from(d.as_os_str()));
    let mut info = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE,
        hwnd,
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    if let Some(d) = &dir {
        info.lpDirectoryW = PCWSTR(d.as_ptr());
        info.lpDirectory = PCSTR(std::ptr::null()); // unicode field wins with CMIC_MASK_UNICODE
    }
    match verb {
        Verb::Id(id) if (1..=LAST_ID).contains(&id) => {
            // MAKEINTRESOURCE(offset): the command's offset from idCmdFirst.
            let offset = (id - 1) as usize;
            info.lpVerb = PCSTR(offset as *const u8);
            info.lpVerbW = PCWSTR(offset as *const u16);
        }
        Verb::Id(_) => return Err(E_FAIL.into()),
        Verb::Name(v) => {
            ansi = format!("{v}\0");
            wide = HSTRING::from(v);
            info.lpVerb = PCSTR(ansi.as_ptr());
            info.lpVerbW = PCWSTR(wide.as_ptr());
        }
    }
    unsafe { cm.InvokeCommand((&raw const info).cast()) }
}
