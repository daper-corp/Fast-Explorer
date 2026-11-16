//! Dragging a tab out of the strip: a click-through ghost that follows the pointer while the
//! tab is outside the strip, and handing the tab's folder to another fex window.

use std::cell::{Cell, RefCell};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

/// `dwData` tag of the WM_COPYDATA that carries a tab's folder to another fex window.
const MAGIC: usize = 0x4645_5854; // "FEXT"
const GHOST_W: f32 = 200.0;
const GHOST_H: f32 = 32.0;

thread_local! {
    static GHOST: Cell<isize> = const { Cell::new(0) };
    static TITLE: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Tags a main window so other fex processes can tell it from dialogs and foreign windows.
pub fn mark(hwnd: isize) {
    let _ = unsafe { SetPropW(HWND(hwnd as _), w!("FexMain"), Some(HANDLE(1 as _))) };
}

/// Shows the ghost of a tab titled `title` with its top-left at screen (x, y), or hides it on `None`.
/// Slint cannot draw outside its window, so the ghost is a native layered window.
pub fn ghost(at: Option<(&str, i32, i32, f32)>) {
    unsafe {
        let mut h = GHOST.get();
        let Some((title, x, y, scale)) = at else {
            if h != 0 {
                let _ = ShowWindow(HWND(h as _), SW_HIDE);
            }
            return;
        };
        if h == 0 {
            let class = w!("FexTabGhost");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(ghost_proc),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
            let Ok(win) = CreateWindowExW(ex, class, PCWSTR::null(), WS_POPUP, 0, 0, 0, 0, None, None, None, None) else {
                return;
            };
            let _ = SetLayeredWindowAttributes(win, COLORREF(0), 225, LWA_ALPHA);
            h = win.0 as isize;
            GHOST.set(h);
        }
        let hwnd = HWND(h as _);
        if TITLE.with_borrow(|t| t.as_str() != title) {
            TITLE.with_borrow_mut(|t| *t = title.to_string());
            let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
            let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        let (cx, cy) = ((GHOST_W * scale) as i32, (GHOST_H * scale) as i32);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, cx, cy, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

unsafe extern "system" fn ghost_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if m == WM_PAINT {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(h, &mut ps);
            let mut r = RECT::default();
            let _ = GetClientRect(h, &mut r);
            let s = r.bottom as f32 / GHOST_H;
            FillRect(dc, &r, HBRUSH(GetStockObject(WHITE_BRUSH).0));
            FrameRect(dc, &r, HBRUSH(GetStockObject(GRAY_BRUSH).0));
            let accent = CreateSolidBrush(COLORREF(0x00D7_7800)); // accent blue (BGR)
            let line = RECT { bottom: r.top + (3.0 * s) as i32, ..r };
            FillRect(dc, &line, accent);
            let _ = DeleteObject(accent.into());
            let font = CreateFontW(
                -(14.0 * s) as i32, 0, 0, 0, 600, 0, 0, 0,
                DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
            );
            let old = SelectObject(dc, font.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x0020_2020));
            let mut buf = [0u16; 260];
            let n = GetWindowTextW(h, &mut buf).max(0) as usize;
            let pad = (10.0 * s) as i32;
            let mut text = RECT { left: r.left + pad, right: r.right - pad, ..r };
            DrawTextW(dc, &mut buf[..n], &mut text, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            SelectObject(dc, old);
            let _ = DeleteObject(font.into());
            let _ = EndPaint(h, &ps);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(h, m, wp, lp) }
}

/// The main window of another fex process and its tab strip (screen px) if the point is on the strip.
pub fn other_fex_at(own: HWND, x: i32, y: i32, scale: f32) -> Option<(HWND, RECT)> {
    struct Query {
        x: i32,
        y: i32,
        hit: HWND,
    }
    // Windows come top to bottom; skip our click-through ghost and highlight, which sit on top.
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> windows::core::BOOL {
        let q = unsafe { &mut *(l.0 as *mut Query) };
        let mut r = RECT::default();
        let see_through = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32 & WS_EX_TRANSPARENT.0 != 0;
        let hit = unsafe { IsWindowVisible(h) }.as_bool()
            && !see_through
            && unsafe { GetWindowRect(h, &mut r) }.is_ok()
            && (r.left..r.right).contains(&q.x)
            && (r.top..r.bottom).contains(&q.y);
        if hit {
            q.hit = h;
        }
        (!hit).into()
    }
    let mut q = Query { x, y, hit: HWND::default() };
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut q as *mut Query as isize)) };
    let root = q.hit;
    if root.0.is_null() || root == own || unsafe { GetPropW(root, w!("FexMain")) }.0.is_null() {
        return None;
    }
    let mut r = RECT::default();
    unsafe { GetWindowRect(root, &mut r) }.ok()?;
    let strip = RECT { bottom: r.top + (46.0 * scale) as i32, ..r };
    (y < strip.bottom).then_some((root, strip))
}

/// Asks another fex window to open `path` in a new tab and brings it to the front.
pub fn send_tab(target: HWND, from: HWND, path: &Path) -> bool {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let cds = COPYDATASTRUCT { dwData: MAGIC, cbData: (wide.len() * 2) as u32, lpData: wide.as_ptr() as *mut _ };
    let mut result = 0usize;
    let sent = unsafe {
        SendMessageTimeoutW(
            target,
            WM_COPYDATA,
            WPARAM(from.0 as usize),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG,
            2000,
            Some(&mut result as *mut usize),
        )
    };
    let ok = sent.0 != 0 && result == 1;
    if ok {
        let _ = unsafe { SetForegroundWindow(target) };
    }
    ok
}

/// Handles a WM_COPYDATA sent by `send_tab`: opens the folder in a new tab.
pub fn received(lparam: LPARAM) -> LRESULT {
    let cds = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
    if cds.dwData != MAGIC || cds.lpData.is_null() || cds.cbData % 2 != 0 || cds.cbData > 65_536 {
        return LRESULT(0);
    }
    let wide = unsafe { std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2) };
    let path = PathBuf::from(String::from_utf16_lossy(wide.split(|&c| c == 0).next().unwrap_or_default()));
    // Only folders (or "This PC"): the sender is not trusted to open anything else.
    if !(path.is_dir() || crate::is_this_pc(&path)) {
        return LRESULT(0);
    }
    crate::post(move |a| a.new_tab(path));
    LRESULT(1)
}
