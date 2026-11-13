//! OLE drag and drop: drag the selection out to any app, and accept files dropped on the
//! list or on a folder row. The callbacks run on the UI thread.

use std::cell::RefCell;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use windows::core::{implement, BOOL, HRESULT, PCWSTR, Ref, Result as WinResult};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, POINTL, S_OK,
};
use windows::Win32::System::Com::{CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_LBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{
    BHID_DataObject, DragQueryFileW, HDROP, ILCreateFromPathW, IShellItemArray, SHCreateShellItemArrayFromIDLists,
};

/// What the app does with a drag: where it may land, and what to do once it does.
pub struct Hooks {
    /// Screen point -> (folder to drop into, folder row to highlight).
    pub target_at: fn(i32, i32) -> Option<(PathBuf, Option<usize>)>,
    /// Runs the copy/move; `move_` is true when the drop effect was MOVE.
    pub dropped: fn(Vec<PathBuf>, PathBuf, bool),
    /// Highlights a folder row, or clears the highlight.
    pub hover: fn(Option<usize>),
    /// Called on every DragOver with the screen point, to scroll the list near its edges.
    pub scroll: fn(i32, i32),
    /// The mouse wheel turned during a drag we started; the delta is in wheel units (120 per notch).
    pub wheel: fn(i32),
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

fn hooks() -> Option<&'static Hooks> {
    HOOKS.get()
}

/// Registers the window as a drop target. Call once, on the UI thread, after the window exists.
pub fn install(hwnd: isize, hooks: Hooks) {
    let _ = HOOKS.set(hooks);
    unsafe {
        // Drag and drop needs OLE on this thread; an earlier CoInitializeEx is harmless.
        let _ = OleInitialize(None);
    }
    let target: IDropTarget = DropTarget { state: RefCell::default() }.into();
    unsafe {
        // winit registers its own (unused by Slint) drop target on this window; take it over.
        let _ = RevokeDragDrop(HWND(hwnd as _));
        // RegisterDragDrop takes its own reference (kept until the window goes away), so the
        // local one is released when it goes out of scope.
        let _ = RegisterDragDrop(HWND(hwnd as _), &target);
    }
}

struct Job {
    paths: Vec<PathBuf>,
    done: Box<dyn FnOnce(Option<bool>)>,
}

thread_local! {
    static DRAG_WINDOW: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// Starts an OLE drag of `paths` and calls `done` once the drop finishes: `None` means the
/// drag could not start, otherwise whether the effect was MOVE (the caller then refreshes).
/// The drag runs from a plain Win32 message, not from inside a Slint/winit callback: winit
/// buffers repaints while its handler is running, so the list could not scroll during a drag.
pub fn start_drag(paths: Vec<PathBuf>, done: impl FnOnce(Option<bool>) + 'static) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let mut h = DRAG_WINDOW.get();
        if h == 0 {
            let class = windows::core::w!("FexDragStarter");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(drag_proc),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassExW(&wc);
            if let Ok(w) = CreateWindowExW(Default::default(), class, PCWSTR::null(), WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None) {
                h = w.0 as isize;
                DRAG_WINDOW.set(h);
            }
        }
        let job = Box::into_raw(Box::new(Job { paths, done: Box::new(done) }));
        if h == 0 || PostMessageW(Some(HWND(h as _)), WM_APP, WPARAM(0), LPARAM(job as isize)).is_err() {
            drop(Box::from_raw(job)); // the drag never started
        }
    }
}

unsafe extern "system" fn drag_proc(
    h: HWND,
    m: u32,
    w: windows::Win32::Foundation::WPARAM,
    l: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::*;
    if m == WM_APP {
        let job = unsafe { Box::from_raw(l.0 as *mut Job) };
        let Job { paths, done } = *job;
        done(run_drag(paths));
        return windows::Win32::Foundation::LRESULT(0);
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn run_drag(paths: Vec<PathBuf>) -> Option<bool> {
    if paths.is_empty() {
        return None;
    }
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, UnhookWindowsHookEx, WH_GETMESSAGE};
    unsafe {
        let data = shell_data_object(&paths)?;
        let source: IDropSource = DropSource(()).into();
        let mut effect = DROPEFFECT_NONE;
        // DoDragDrop's loop eats the wheel; a message hook still sees it as it is read.
        let hook = SetWindowsHookExW(WH_GETMESSAGE, Some(wheel_hook), None, GetCurrentThreadId()).ok();
        let r = DoDragDrop(&data, &source, DROPEFFECT_COPY | DROPEFFECT_MOVE, &mut effect);
        if let Some(hook) = hook {
            let _ = UnhookWindowsHookEx(hook);
        }
        Some(r == DRAGDROP_S_DROP && effect.0 & DROPEFFECT_MOVE.0 != 0)
    }
}

unsafe extern "system" fn wheel_hook(
    code: i32,
    w: windows::Win32::Foundation::WPARAM,
    l: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, MSG, PM_REMOVE, WM_MOUSEWHEEL};
    if code >= 0 && w.0 as u32 == PM_REMOVE.0 {
        let msg = unsafe { &*(l.0 as *const MSG) };
        if msg.message == WM_MOUSEWHEEL
            && let Some(h) = hooks()
        {
            (h.wheel)(((msg.wParam.0 >> 16) as i16).into());
        }
    }
    unsafe { CallNextHookEx(None, code, w, l) }
}

#[implement(IDropSource)]
struct DropSource(());

#[allow(non_snake_case)]
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if (keys & MK_LBUTTON) == MODIFIERKEYS_FLAGS(0) {
            DRAGDROP_S_DROP // the left button is up
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// The IDataObject the shell builds for a list of items: it carries CF_HDROP and the other
/// formats Explorer publishes, so every drop target understands the drag.
unsafe fn shell_data_object(paths: &[PathBuf]) -> Option<IDataObject> {
    unsafe {
        let mut ids: Vec<*const windows::Win32::UI::Shell::Common::ITEMIDLIST> = Vec::with_capacity(paths.len());
        for p in paths {
            let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
            let id = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
            if id.is_null() {
                continue;
            }
            ids.push(id.cast_const());
        }
        let array = SHCreateShellItemArrayFromIDLists(&ids).ok();
        // The array copied the ids; the originals are ours to free.
        for id in ids {
            CoTaskMemFree(Some(id.cast()));
        }
        let array: IShellItemArray = array?;
        let data: IDataObject = array.BindToHandler(None, &BHID_DataObject).ok()?;
        Some(data)
    }
}

#[derive(Default)]
struct State {
    files: Vec<PathBuf>,
    row: Option<usize>,
    /// Effects the drag source allows, from DragEnter.
    allowed: DROPEFFECT,
}

#[implement(IDropTarget)]
struct DropTarget {
    state: RefCell<State>,
}

impl DropTarget {
    /// The folder under `pt` if the drop would be meaningful, and the effect to show.
    fn locate(
        &self,
        files: &[PathBuf],
        pt: &POINTL,
        keys: MODIFIERKEYS_FLAGS,
        allowed: DROPEFFECT,
    ) -> Option<(PathBuf, Option<usize>, DROPEFFECT)> {
        let (target, row) = (hooks()?.target_at)(pt.x, pt.y)?;
        if files.is_empty() || files.iter().any(|f| target == *f || target.starts_with(f)) {
            return None;
        }
        // Dropping back into the source folder (blank space) cancels; Ctrl still duplicates.
        if keys & MK_CONTROL == MODIFIERKEYS_FLAGS(0) && files.iter().all(|f| f.parent() == Some(&*target)) {
            return None;
        }
        let want = choose_effect(files, &target, keys);
        let effect = if allowed == DROPEFFECT_NONE || want.0 & allowed.0 != 0 {
            want
        } else if allowed.0 & DROPEFFECT_COPY.0 != 0 {
            DROPEFFECT_COPY
        } else if allowed.0 & DROPEFFECT_MOVE.0 != 0 {
            DROPEFFECT_MOVE
        } else {
            DROPEFFECT_NONE
        };
        (effect != DROPEFFECT_NONE).then_some((target, row, effect))
    }

    fn enter(&self, pt: &POINTL, keys: MODIFIERKEYS_FLAGS, effect: *mut DROPEFFECT) -> WinResult<()> {
        let located = {
            let state = self.state.borrow();
            self.locate(&state.files, pt, keys, state.allowed)
        };
        match located {
            Some((_target, row, chosen)) => {
                let mut state = self.state.borrow_mut();
                let changed = state.row != row;
                state.row = row;
                drop(state);
                if changed && let Some(h) = hooks() {
                    (h.hover)(row);
                }
                unsafe { *effect = chosen };
            }
            None => {
                let old = self.state.borrow_mut().row.take();
                if old.is_some() && let Some(h) = hooks() {
                    (h.hover)(None);
                }
                unsafe { *effect = DROPEFFECT_NONE };
            }
        }
        Ok(())
    }

    fn over(&self, pt: &POINTL, keys: MODIFIERKEYS_FLAGS, effect: *mut DROPEFFECT) -> WinResult<()> {
        unsafe { *effect = DROPEFFECT_NONE };
        let located = {
            let state = self.state.borrow();
            if state.files.is_empty() {
                None
            } else {
                self.locate(&state.files, pt, keys, state.allowed)
            }
        };
        match located {
            Some((_target, row, chosen)) => {
                let mut state = self.state.borrow_mut();
                let changed = state.row != row;
                state.row = row;
                drop(state);
                if changed && let Some(h) = hooks() {
                    (h.hover)(row);
                }
                unsafe { *effect = chosen };
            }
            None => {
                let old = self.state.borrow_mut().row.take();
                if old.is_some() && let Some(h) = hooks() {
                    (h.hover)(None);
                }
            }
        }
        Ok(())
    }

    fn finish(&self, data: Option<&IDataObject>, pt: &POINTL, keys: MODIFIERKEYS_FLAGS, effect: *mut DROPEFFECT) -> WinResult<()> {
        let mut state = std::mem::take(&mut *self.state.borrow_mut());
        if let Some(data) = data
            && let Some(files) = unsafe { hdrop_files(data) }
        {
            state.files = files;
        }
        if let Some(h) = hooks() {
            (h.hover)(None);
        }
        let Some((target, _, chosen)) = self.locate(&state.files, pt, keys, state.allowed) else {
            unsafe { *effect = DROPEFFECT_NONE };
            return Ok(());
        };
        unsafe { *effect = chosen };
        if let Some(h) = hooks() {
            (h.dropped)(state.files, target, chosen.contains(DROPEFFECT_MOVE));
        }
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        // Keep the files on the first enter: DragOver has no data object.
        let files = data.as_ref().and_then(|data| unsafe { hdrop_files(data) }).unwrap_or_default();
        let allowed = unsafe { *effect };
        {
            let mut state = self.state.borrow_mut();
            state.files = files;
            state.allowed = allowed;
        }
        self.enter(pt, keys, effect)
    }

    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        let result = self.over(pt, keys, effect);
        if let Some(h) = hooks() {
            (h.scroll)(pt.x, pt.y);
        }
        result
    }

    fn DragLeave(&self) -> WinResult<()> {
        *self.state.borrow_mut() = State::default();
        if let Some(h) = hooks() {
            (h.hover)(None);
        }
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        self.finish(data.as_ref(), pt, keys, effect)
    }
}

/// Explorer's default: move inside one volume, copy across volumes; Ctrl/Shift override.
fn choose_effect(files: &[PathBuf], target: &Path, keys: MODIFIERKEYS_FLAGS) -> DROPEFFECT {
    if keys & MK_CONTROL != MODIFIERKEYS_FLAGS(0) {
        return DROPEFFECT_COPY;
    }
    if keys & MK_SHIFT != MODIFIERKEYS_FLAGS(0) {
        return DROPEFFECT_MOVE;
    }
    let same_volume = files.iter().all(|f| volume(f) == volume(target));
    if same_volume { DROPEFFECT_MOVE } else { DROPEFFECT_COPY }
}

fn volume(p: &Path) -> Option<&std::ffi::OsStr> {
    p.ancestors().last().map(|r| r.as_os_str())
}

/// Files in an IDataObject's CF_HDROP, releasing the medium.
unsafe fn hdrop_files(data: &IDataObject) -> Option<Vec<PathBuf>> {
    unsafe {
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let mut medium = data.GetData(&format).ok()?;
        let hdrop = HDROP(medium.u.hGlobal.0 as _);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            if got > 0 {
                paths.push(PathBuf::from(OsString::from_wide(&buf[..got])));
            }
        }
        ReleaseStgMedium(&mut medium);
        Some(paths)
    }
}

thread_local! {
    static OVERLAY: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// Tints `rect` (screen px) with a click-through translucent window, or hides it on `None`.
/// Slint cannot repaint inside DoDragDrop's modal loop, so the drop-target highlight is native.
pub fn highlight(rect: Option<windows::Win32::Foundation::RECT>) {
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let mut h = OVERLAY.get();
        let Some(r) = rect else {
            if h != 0 {
                let _ = ShowWindow(HWND(h as _), SW_HIDE);
            }
            return;
        };
        if h == 0 {
            let class = windows::core::w!("FexDropHighlight");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(overlay_proc),
                hbrBackground: CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00D7_7800)), // accent blue (BGR)
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
            let Ok(w) = CreateWindowExW(ex, class, PCWSTR::null(), WS_POPUP, 0, 0, 0, 0, None, None, None, None) else {
                return;
            };
            let _ = SetLayeredWindowAttributes(w, windows::Win32::Foundation::COLORREF(0), 70, LWA_ALPHA);
            h = w.0 as isize;
            OVERLAY.set(h);
        }
        let _ = SetWindowPos(
            HWND(h as _),
            Some(HWND_TOPMOST),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

unsafe extern "system" fn overlay_proc(
    h: HWND,
    m: u32,
    w: windows::Win32::Foundation::WPARAM,
    l: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(h, m, w, l) }
}
