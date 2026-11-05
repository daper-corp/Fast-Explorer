//! Native Win32 popup menus: Fast Explorer's own commands plus the shell items from the host.

use fex_core::menu::MenuItem;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MENU_ITEM_FLAGS, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, SetMenuDefaultItem, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
};
use windows::core::{HSTRING, PCWSTR};

/// One of our own commands. Ids sit above the shell's range (1..=0x7FFF); id 0 is a separator.
pub struct Own {
    pub id: u32,
    /// Text with an optional "\t" shortcut hint, e.g. "복사\tCtrl+C".
    pub label: &'static str,
    pub default: bool,
    pub disabled: bool,
}

pub const SEP: Own = Own { id: 0, label: "", default: false, disabled: false };

pub const fn own(id: u32, label: &'static str) -> Own {
    Own { id, label, default: false, disabled: false }
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
        let mut root = Level { menu, pending_sep: false, count: 0 };
        root.add_own(top);
        root.pending_sep = true;
        let seen = top.iter().chain(bottom).filter(|o| o.id != 0).map(|o| plain(o.label)).collect();
        root = add_shell(root, items, seen);
        root.pending_sep = true;
        root.add_own(bottom);
        let flags = (TPM_RETURNCMD | TPM_RIGHTBUTTON).0;
        let id = TrackPopupMenuEx(menu, flags, at.0, at.1, HWND(hwnd as _), None).0 as u32;
        let _ = DestroyMenu(menu); // also destroys the submenus
        id
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

    unsafe fn add_own(&mut self, items: &[Own]) {
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
            }
        }
    }
}

/// Rebuilds the host's flattened item list (depth-first) as nested menus under `root`.
/// Top-level items whose label is already in `seen` (ours, or an earlier shell item) are dropped.
unsafe fn add_shell(root: Level, items: &[MenuItem], mut seen: std::collections::HashSet<String>) -> Level {
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
                stack.push(Level { menu: sub, pending_sep: false, count: 0 });
            } else {
                stack[d].append(flags, it.id as usize, &it.label);
                if it.default {
                    let _ = SetMenuDefaultItem(stack[d].menu, it.id, 0);
                }
            }
        }
    }
    stack[0]
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
