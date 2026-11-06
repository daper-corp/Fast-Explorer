//! Shell icons as RGBA pixels. Loading may read files (.exe, .lnk) or registry handlers, so
//! call it from a worker thread that has COM initialized ([`init_thread`]).

use std::path::PathBuf;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP,
    ReleaseDC,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::HSTRING;

/// What an icon depends on. Most files share their extension's icon; a few types carry their
/// own (programs, shortcuts, icon files), which needs the actual file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IconKey {
    Folder,
    /// Lowercase extension without the dot ("" for none).
    Ext(String),
    File(PathBuf),
}

/// Extensions whose icon comes from the file itself.
const OWN_ICON: [&str; 8] = ["exe", "lnk", "ico", "url", "scr", "cpl", "msc", "appref-ms"];

impl IconKey {
    pub fn for_entry(dir: bool, ext: &str, path: impl FnOnce() -> PathBuf) -> Self {
        let ext = ext.to_lowercase();
        match () {
            _ if dir => IconKey::Folder,
            _ if OWN_ICON.contains(&ext.as_str()) => IconKey::File(path()),
            _ => IconKey::Ext(ext),
        }
    }
}

pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn init_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// The shell's 32 px icon for `key`.
pub fn load(key: &IconKey) -> Option<Pixels> {
    let (path, attrs, flags) = match key {
        IconKey::Folder => (HSTRING::from("folder"), FILE_ATTRIBUTE_DIRECTORY, SHGFI_USEFILEATTRIBUTES),
        IconKey::Ext(e) if e.is_empty() => (HSTRING::from("file"), FILE_ATTRIBUTE_NORMAL, SHGFI_USEFILEATTRIBUTES),
        IconKey::Ext(e) => (HSTRING::from(format!("file.{e}")), FILE_ATTRIBUTE_NORMAL, SHGFI_USEFILEATTRIBUTES),
        IconKey::File(p) => (HSTRING::from(p.as_os_str()), FILE_FLAGS_AND_ATTRIBUTES(0), Default::default()),
    };
    unsafe {
        let mut info = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(&path, attrs, Some(&mut info), size_of::<SHFILEINFOW>() as u32, flags | SHGFI_ICON | SHGFI_LARGEICON);
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let px = to_rgba(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        px
    }
}

unsafe fn to_rgba(icon: HICON) -> Option<Pixels> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(icon, &mut ii).ok()?;
        let px = (!ii.hbmColor.is_invalid()).then(|| read_icon(&ii)).flatten();
        let _ = DeleteObject(ii.hbmColor.into());
        let _ = DeleteObject(ii.hbmMask.into());
        px
    }
}

unsafe fn read_icon(ii: &ICONINFO) -> Option<Pixels> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(ii.hbmColor.into(), size_of::<BITMAP>() as i32, Some((&raw mut bm).cast())) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut color = bits(ii.hbmColor, w, h)?;
        // Old icons have no alpha channel: take transparency from the mask (white = transparent).
        if color.chunks_exact(4).all(|p| p[3] == 0) {
            let mask = bits(ii.hbmMask, w, h)?;
            for (c, m) in color.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                c[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        for p in color.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA -> RGBA
        }
        Some(Pixels { width: w as u32, height: h as u32, rgba: color })
    }
}

/// A bitmap's pixels as top-down 32-bit BGRA.
unsafe fn bits(bmp: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    unsafe {
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
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        (lines == h).then_some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_icons_with_transparency() {
        init_thread();
        for key in [IconKey::Folder, IconKey::Ext("txt".into()), IconKey::File(r"C:\Windows\explorer.exe".into())] {
            let px = load(&key).unwrap_or_else(|| panic!("{key:?}"));
            assert_eq!(px.rgba.len(), (px.width * px.height * 4) as usize);
            assert!(px.width >= 16, "{key:?}");
            let alpha: Vec<u8> = px.rgba.chunks(4).map(|p| p[3]).collect();
            assert!(alpha.contains(&0) && alpha.contains(&255), "{key:?}: needs see-through and solid pixels");
        }
    }
}
