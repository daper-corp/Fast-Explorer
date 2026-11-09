//! Native Win32 popup menus: Fast Explorer's own commands plus the shell items from the host.

use std::path::PathBuf;

use fex_core::menu::MenuItem;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection,
    CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_NOCLIP, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject,
    DrawTextW, FW_NORMAL, GdiFlush, HBITMAP, HGDIOBJ, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGSI_ICON, SHGSI_SMALLICON, SHGetFileInfoW, SHGetStockIconInfo,
    SHSTOCKICONID, SHSTOCKICONINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetIconInfo, GetMenuItemCount, GetSystemMetrics, HMENU, ICONINFO,
    MENU_ITEM_FLAGS, MENUITEMINFOW, MF_BYCOMMAND, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_BITMAP,
    SM_CXSMICON, SetMenuDefaultItem, SetMenuItemBitmaps, SetMenuItemInfoW, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenuEx,
};
use windows::core::{HSTRING, PCWSTR, w};

/// An icon for one of our menu items: a shell stock icon, the icon of a file/folder, or a glyph
/// of the Segoe icon font (the same symbols Windows 11 menus use for cut/copy/paste).
pub enum Icon {
    Stock(SHSTOCKICONID),
    Path(PathBuf),
    Glyph(char),
}

/// Segoe MDL2 Assets code points.
pub mod glyph {
    pub const CUT: char = '\u{E8C6}';
    pub const COPY: char = '\u{E8C8}';
    pub const PASTE: char = '\u{E77F}';
    pub const REFRESH: char = '\u{E72C}';
    pub const SEARCH: char = '\u{E721}';
    pub const NEW_TAB: char = '\u{E8A7}';
    pub const LINK: char = '\u{E71B}';
    pub const STAR: char = '\u{E734}';
    pub const MORE: char = '\u{E712}';
    pub const RENAME_MANY: char = '\u{E8AC}';
    pub const LOCK: char = '\u{E72E}';
    pub const TERMINAL: char = '\u{E756}';
}

/// One of our own commands. Ids sit above the shell's range (1..=0x7FFF); id 0 is a separator.
pub struct Own {
    pub id: u32,
    /// Text with an optional "\t" shortcut hint, e.g. "복사\tCtrl+C".
    pub label: &'static str,
    pub default: bool,
    pub disabled: bool,
    pub icon: Option<Icon>,
}

pub const SEP: Own = Own { id: 0, label: "", default: false, disabled: false, icon: None };

pub const fn own(id: u32, label: &'static str) -> Own {
    Own { id, label, default: false, disabled: false, icon: None }
}

/// Verbs we already offer ourselves; hidden from the shell part to avoid duplicates.
const OWN_VERBS: [&str; 8] = ["open", "cut", "copy", "copyaspath", "paste", "delete", "rename", "properties"];

/// "새 폴더(&N)" -> "새 폴더": what a menu label reads as, for spotting duplicates.
fn plain(label: &str) -> String {
    let s = label.split('\t').next().unwrap_or("").replace('&', "");
    let s = s.trim();
    // A trailing "(X)" accelerator, as Korean/Japanese/Chinese menus write it.
    match s.char_indices().rev().nth(2) {
        Some((i, '(')) if s.ends_with(')') => s[..i].trim_end().to_string(),
        _ => s.to_string(),
    }
}

/// Shows `top`, the shell `items`, then `bottom` at screen point `at`; returns the chosen id or 0.
/// Runs a modal loop until the menu closes.
pub fn show(hwnd: isize, at: (i32, i32), top: &[Own], items: &[MenuItem], bottom: &[Own]) -> u32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let mut bitmaps = Vec::new();
        let mut root = Level { menu, pending_sep: false, count: 0 };
        root.add_own(top, &mut bitmaps);
        root.pending_sep = true;
        let seen = top.iter().chain(bottom).filter(|o| o.id != 0).map(|o| plain(o.label)).collect();
        root = add_shell(root, items, seen, &mut bitmaps);
        root.pending_sep = true;
        root.add_own(bottom, &mut bitmaps);
        let flags = (TPM_RETURNCMD | TPM_RIGHTBUTTON).0;
        let id = TrackPopupMenuEx(menu, flags, at.0, at.1, HWND(hwnd as _), None).0 as u32;
        let _ = DestroyMenu(menu); // also destroys the submenus
        // The menu only referenced these; it did not own them.
        for (color, mask) in bitmaps {
            let _ = DeleteObject(HGDIOBJ(color.0));
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        id
    }
}

/// Loads a menu icon and returns the color+mask bitmaps the menu can draw with.
unsafe fn icon_bitmaps(icon: &Icon) -> Option<(HBITMAP, HBITMAP)> {
    unsafe {
        let hicon = match icon {
            Icon::Glyph(_) => return None, // drawn as an alpha bitmap; see `glyph_bitmap`
            Icon::Stock(id) => {
                let mut info =
                    SHSTOCKICONINFO { cbSize: size_of::<SHSTOCKICONINFO>() as u32, ..Default::default() };
                SHGetStockIconInfo(*id, SHGSI_ICON | SHGSI_SMALLICON, &mut info).ok()?;
                info.hIcon
            }
            Icon::Path(path) => {
                let mut info = SHFILEINFOW::default();
                let path = HSTRING::from(path.as_os_str());
                let got = SHGetFileInfoW(
                    PCWSTR(path.as_ptr()),
                    FILE_ATTRIBUTE_NORMAL,
                    Some(&mut info),
                    size_of::<SHFILEINFOW>() as u32,
                    SHGFI_ICON | SHGFI_SMALLICON,
                );
                if got == 0 {
                    return None;
                }
                info.hIcon
            }
        };
        if hicon.is_invalid() {
            return None;
        }
        let mut info = ICONINFO { fIcon: true.into(), ..Default::default() };
        let ok = GetIconInfo(hicon, &mut info).is_ok();
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(hicon);
        if !ok || info.hbmColor.is_invalid() {
            if !info.hbmMask.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
            }
            return None;
        }
        Some((info.hbmColor, info.hbmMask))
    }
}

/// A menu being filled. Separators are deferred so none end up leading, trailing or doubled.
#[derive(Clone, Copy)]
struct Level {
    menu: HMENU,
    pending_sep: bool,
    count: u32,
}

impl Level {
    unsafe fn append(&mut self, flags: MENU_ITEM_FLAGS, id: usize, label: &str) {
        unsafe {
            if self.pending_sep && self.count > 0 {
                let _ = AppendMenuW(self.menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
            let _ = AppendMenuW(self.menu, flags, id, &HSTRING::from(label));
        }
        self.pending_sep = false;
        self.count += 1;
    }

    /// Shows `bmp` (premultiplied-alpha) left of the item appended last.
    unsafe fn set_last_bitmap(&self, bmp: Option<HBITMAP>, bitmaps: &mut Vec<(HBITMAP, HBITMAP)>) {
        let Some(bmp) = bmp else { return };
        unsafe {
            let pos = (GetMenuItemCount(Some(self.menu)) - 1).max(0) as u32;
            let mii = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_BITMAP,
                hbmpItem: bmp,
                ..Default::default()
            };
            let _ = SetMenuItemInfoW(self.menu, pos, true, &mii);
        }
        // Freed with the other bitmaps; the mask slot is unused.
        bitmaps.push((bmp, HBITMAP::default()));
    }

    unsafe fn add_own(&mut self, items: &[Own], bitmaps: &mut Vec<(HBITMAP, HBITMAP)>) {
        for o in items {
            if o.id == 0 {
                self.pending_sep = true;
                continue;
            }
            unsafe {
                self.append(if o.disabled { MF_GRAYED } else { MF_STRING }, o.id as usize, o.label);
                if o.default {
                    let _ = SetMenuDefaultItem(self.menu, o.id, 0);
                }
                if let Some(Icon::Glyph(g)) = &o.icon {
                    self.set_last_bitmap(glyph_bitmap(*g), bitmaps);
                } else if let Some(icon) = &o.icon
                    && let Some((color, mask)) = icon_bitmaps(icon)
                {
                    let usable = (!mask.is_invalid()).then_some(mask);
                    let _ = SetMenuItemBitmaps(self.menu, o.id, MF_BYCOMMAND, Some(color), usable);
                    bitmaps.push((color, mask));
                }
            }
        }
    }
}

/// Rebuilds the host's flattened item list (depth-first) as nested menus under `root`.
/// Top-level items whose label is already in `seen` (ours, or an earlier shell item) are dropped.
unsafe fn add_shell(
    root: Level,
    items: &[MenuItem],
    mut seen: std::collections::HashSet<String>,
    bitmaps: &mut Vec<(HBITMAP, HBITMAP)>,
) -> Level {
    // ponytail: label-based dedup is a heuristic (same text = same command); good enough for
    // menus, revisit if an extension legitimately repeats a label.
    let mut stack = vec![root];
    let mut skip_below: Option<u8> = None;
    for it in items {
        if let Some(d) = skip_below {
            if it.depth > d {
                continue;
            }
            skip_below = None;
        }
        let d = it.depth as usize;
        if d >= stack.len() {
            continue;
        }
        stack.truncate(d + 1);
        if it.separator {
            stack[d].pending_sep = true;
            continue;
        }
        if d == 0 && (OWN_VERBS.contains(&it.verb.as_str()) || !seen.insert(plain(&it.label))) {
            skip_below = Some(it.depth); // drops a duplicate submenu's children too
            continue;
        }
        let flags = if it.disabled { MF_GRAYED } else { MF_STRING };
        unsafe {
            if it.submenu {
                let Ok(sub) = CreatePopupMenu() else {
                    skip_below = Some(it.depth);
                    continue;
                };
                stack[d].append(flags | MF_POPUP, sub.0 as usize, &it.label);
                stack[d].set_last_bitmap(shell_item_bitmap(it), bitmaps);
                stack.push(Level { menu: sub, pending_sep: false, count: 0 });
            } else {
                stack[d].append(flags, it.id as usize, &it.label);
                stack[d].set_last_bitmap(shell_item_bitmap(it), bitmaps);
                if it.default {
                    let _ = SetMenuDefaultItem(stack[d].menu, it.id, 0);
                }
            }
        }
    }
    stack[0]
}

/// The bitmap the shell extension supplied for `it`; failing that, a symbol for well-known items.
unsafe fn shell_item_bitmap(it: &MenuItem) -> Option<HBITMAP> {
    if let Some((w, h, px)) = it.icon_pixels() {
        return unsafe { dib(w as i32, h as i32, &px) };
    }
    let label = it.label.to_lowercase();
    if it.verb.contains("terminal") || label.contains("터미널") || label.contains("terminal") {
        return unsafe { glyph_bitmap(glyph::TERMINAL) };
    }
    None
}

/// A 32-bit top-down DIB holding premultiplied BGRA `px`.
unsafe fn dib(w: i32, h: i32, px: &[u8]) -> Option<HBITMAP> {
    unsafe {
        let info = BITMAPINFO {
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
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() || px.len() != (w * h * 4) as usize {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            return None;
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits.cast::<u8>(), px.len());
        Some(bmp)
    }
}

/// Renders a Segoe icon-font glyph as a small dark alpha bitmap.
unsafe fn glyph_bitmap(ch: char) -> Option<HBITMAP> {
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).clamp(16, 64);
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
        // White on black: the coverage ends up in every channel. Segoe MDL2 Assets ships with
        // Windows 10 and 11.
        let font = CreateFontW(
            -(size * 7 / 8), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, 0, w!("Segoe MDL2 Assets"),
        );
        let prev = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FF_FFFF));
        let mut text: Vec<u16> = ch.to_string().encode_utf16().collect();
        let mut rc = RECT { left: 0, top: 0, right: size, bottom: size };
        let drawn = DrawTextW(dc, &mut text, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP) > 0;
        SelectObject(dc, prev);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = GdiFlush();
        let px = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (size * size * 4) as usize);
        const FG: u32 = 0x3C; // dark gray, like the menu text
        for p in px.chunks_exact_mut(4) {
            let a = p[1] as u32; // coverage (green channel)
            let c = (FG * a / 255) as u8; // premultiplied
            p.copy_from_slice(&[c, c, c, a as u8]);
        }
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        if !drawn {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            return None;
        }
        Some(bmp)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plain_strips_accelerators() {
        assert_eq!(super::plain("새 폴더(&N)"), "새 폴더");
        assert_eq!(super::plain("새 폴더\tCtrl+Shift+N"), "새 폴더");
        assert_eq!(super::plain("&Notepad++로 편집"), "Notepad++로 편집");
        assert_eq!(super::plain("반디집으로 압축하기(&L)..."), "반디집으로 압축하기(L)...");
    }
}
