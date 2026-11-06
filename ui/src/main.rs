#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::thread;
use std::time::{Duration, Instant};

use fex_core::content;
use fex_core::icons::{self, IconKey};
use fex_core::index::{self, QueryError, Results};
use fex_core::menu::{HANG_LIMIT, MenuHost, MenuItem, MenuResult, Request};
use fex_core::shell::{self, Op};
use fex_core::watch;
use fex_core::{Entry, SortKey, format_filetime, format_size, list_dir, sort_order};
use popup::{Own, SEP, own};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTONEAREST, MonitorFromWindow, ScreenToClient,
};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, GetCursorPos, GetWindowRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, PostMessageW, WM_CLOSE, WM_NCHITTEST, WM_SIZE, WM_XBUTTONDOWN,
    WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

mod dnd;
mod popup;

slint::include_modules!();

// Our own context-menu commands (shell items use 1..=0x7FFF).
const CMD_OPEN: u32 = 0x8001;
const CMD_OPEN_TAB: u32 = 0x8002;
const CMD_CUT: u32 = 0x8003;
const CMD_COPY: u32 = 0x8004;
const CMD_COPY_PATH: u32 = 0x8005;
const CMD_RENAME: u32 = 0x8006;
const CMD_DELETE: u32 = 0x8007;
const CMD_PROPERTIES: u32 = 0x8008;
const CMD_PASTE: u32 = 0x8009;
const CMD_NEW_FOLDER: u32 = 0x800A;
const CMD_REFRESH: u32 = 0x800B;
const CMD_MORE: u32 = 0x800C;
const CMD_RETRY: u32 = 0x800D;
const CMD_NEW_FILE: u32 = 0x800E;
const CMD_ADD_FAVORITE: u32 = 0x800F;

/// How long a right-click waits for a shell menu that isn't prefetched yet.
const MENU_WAIT: Duration = Duration::from_millis(150);

/// Most search hits shown (the status bar says how many matched in all).
const SEARCH_LIMIT: usize = 2000;
/// Content search stops after this many matching files.
const CONTENT_LIMIT: usize = 5000;

/// "내용:hello" / "content:hello" -> Some("hello"): search inside files.
fn content_query(q: &str) -> Option<String> {
    let lower = q.to_lowercase();
    ["내용:", "content:"]
        .iter()
        .find(|p| lower.starts_with(**p))
        .map(|p| q[p.len()..].trim().to_string())
        .filter(|t| !t.is_empty())
}

const SORT_KEYS: [SortKey; 4] = [SortKey::Name, SortKey::Modified, SortKey::Type, SortKey::Size];

/// One folder's listing. A virtualized model: Slint asks only for the visible rows.
#[derive(Default)]
struct Rows {
    entries: RefCell<Arc<Vec<Entry>>>,
    /// Display order: row -> index into `entries`.
    order: RefCell<Vec<u32>>,
    /// Selection by entry index, so it survives re-sorting.
    selected: RefCell<Vec<bool>>,
    editing: Cell<Option<u32>>,
    /// Folder of each entry, for search hits (empty when listing a folder).
    locations: RefCell<Vec<String>>,
    /// The folder being listed (for icons that come from the file itself).
    base: RefCell<PathBuf>,
    notify: ModelNotify,
}

/// View options shared by every tab (initially Explorer's own settings).
static SHOW_HIDDEN: AtomicBool = AtomicBool::new(false);
static SHOW_EXT: AtomicBool = AtomicBool::new(true);

impl Model for Rows {
    type Data = Row;

    fn row_count(&self) -> usize {
        self.order.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Row> {
        let i = *self.order.borrow().get(row)?;
        let entries = self.entries.borrow();
        let e = &entries[i as usize];
        let ext = e.extension();
        let name = display_name(e);
        let stem = match name.rfind('.') {
            Some(p) if p > 0 && !e.is_dir() => p,
            _ => name.len(),
        };
        let icon = icon_image(IconKey::for_entry(e.is_dir(), &ext, || match self.locations.borrow().get(i as usize) {
            Some(dir) => PathBuf::from(dir).join(e.os_name()),
            None => self.base.borrow().join(e.os_name()),
        }));
        Some(Row {
            date: format_filetime(e.modified).into(),
            kind: kind_of(e).into(),
            size: if e.is_dir() { SharedString::new() } else { format_size(e.size).into() },
            selected: self.selected.borrow()[i as usize],
            editing: self.editing.get() == Some(i),
            stem: stem as i32,
            name: name.into(),
            location: self.locations.borrow().get(i as usize).map(|s| s.as_str().into()).unwrap_or_default(),
            icon,
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

impl Rows {
    fn len(&self) -> usize {
        self.order.borrow().len()
    }

    fn entry(&self, row: usize) -> Option<u32> {
        self.order.borrow().get(row).copied()
    }

    fn row_of(&self, idx: u32) -> Option<usize> {
        self.order.borrow().iter().position(|&i| i == idx)
    }

    fn name(&self, idx: u32) -> String {
        self.entries.borrow()[idx as usize].name()
    }

    fn is_dir(&self, idx: u32) -> bool {
        self.entries.borrow()[idx as usize].is_dir()
    }

    fn replace(&self, entries: Vec<Entry>, order: Vec<u32>) {
        *self.selected.borrow_mut() = vec![false; entries.len()];
        *self.entries.borrow_mut() = Arc::new(entries);
        *self.order.borrow_mut() = order;
        self.locations.borrow_mut().clear();
        self.editing.set(None);
        self.notify.reset();
    }

    /// Selected entry indices in display order.
    fn selection(&self) -> Vec<u32> {
        let sel = self.selected.borrow();
        self.order.borrow().iter().copied().filter(|&i| sel[i as usize]).collect()
    }

    /// Changes the selection and repaints only the rows that changed. Resetting the whole model
    /// would rebuild the row widgets under the mouse and lose a double-click in progress.
    fn update_selection(&self, change: impl FnOnce(&mut Vec<bool>)) {
        let before = self.selected.borrow().clone();
        change(&mut self.selected.borrow_mut());
        let changed: Vec<usize> = {
            let after = self.selected.borrow();
            (0..after.len()).filter(|&i| before.get(i) != after.get(i)).collect()
        };
        if changed.len() > 2000 {
            return self.notify.reset(); // e.g. select all: cheaper than thousands of updates
        }
        let mut row_of = vec![usize::MAX; before.len()];
        for (r, &i) in self.order.borrow().iter().enumerate() {
            row_of[i as usize] = r;
        }
        for i in changed {
            if let Some(&r) = row_of.get(i).filter(|&&r| r != usize::MAX) {
                self.notify.row_changed(r);
            }
        }
    }

    fn select_only(&self, idx: Option<u32>) {
        self.update_selection(|sel| {
            sel.fill(false);
            if let Some(i) = idx {
                sel[i as usize] = true;
            }
        });
    }

    fn toggle(&self, idx: u32) {
        self.update_selection(|sel| sel[idx as usize] = !sel[idx as usize]);
    }

    fn select_range(&self, a: usize, b: usize, add: bool) {
        let range: Vec<u32> = self.order.borrow()[a.min(b)..=a.max(b)].to_vec();
        self.update_selection(|sel| {
            if !add {
                sel.fill(false);
            }
            for i in range {
                sel[i as usize] = true;
            }
        });
    }
}

#[derive(Clone, Copy)]
enum Hist {
    Push,
    Go(usize),
}

/// Applied once the next listing of a tab is ready.
#[derive(Default)]
struct After {
    select: Vec<String>,
    rename: bool,
    /// The listing came from the folder watcher, not from the user.
    auto: bool,
}

struct Tab {
    id: u64,
    rows: Rc<Rows>,
    cwd: RefCell<PathBuf>,
    history: RefCell<Vec<PathBuf>>,
    hist_pos: Cell<usize>,
    /// Bumped per load; a worker stops, and its results are dropped, once it no longer matches.
    epoch: Arc<AtomicU64>,
    /// Bumped per sort request and whenever entries are replaced; only the newest sort applies.
    sort_gen: Cell<u64>,
    sort: Cell<(SortKey, bool)>,
    cursor: Cell<Option<u32>>,
    anchor: Cell<Option<u32>>,
    /// A navigation whose listing hasn't arrived yet; the old folder stays on screen until it does.
    pending: RefCell<Option<(PathBuf, Hist, Option<After>)>>,
    after: RefCell<Option<After>>,
    list_ms: Cell<f64>,
    note: RefCell<String>,
    scroll: Cell<f32>,
    /// Showing search results instead of the folder: (query, path of each entry).
    search: RefCell<Option<(String, Vec<PathBuf>)>>,
    /// Reports changes made to the folder by other programs.
    watch: RefCell<Option<watch::DirWatch>>,
    /// A watcher-triggered refresh is scheduled; further events join it.
    refresh_due: Cell<bool>,
}

impl Tab {
    fn new(id: u64) -> Self {
        Tab {
            id,
            rows: Rc::default(),
            cwd: RefCell::default(),
            history: RefCell::default(),
            hist_pos: Cell::new(0),
            epoch: Arc::default(),
            sort_gen: Cell::new(0),
            sort: Cell::new((SortKey::Name, false)),
            cursor: Cell::new(None),
            anchor: Cell::new(None),
            pending: RefCell::default(),
            after: RefCell::default(),
            list_ms: Cell::new(0.0),
            note: RefCell::default(),
            scroll: Cell::new(0.0),
            search: RefCell::default(),
            watch: RefCell::new(watch::DirWatch::new(move || post(move |a| a.folder_changed(id))).ok()),
            refresh_due: Cell::new(false),
        }
    }

    /// Exact path (from the raw UTF-16 name, not the lossy display string).
    fn path_of(&self, idx: u32) -> PathBuf {
        if let Some((_, paths)) = &*self.search.borrow() {
            return paths[idx as usize].clone();
        }
        self.cwd.borrow().join(self.rows.entries.borrow()[idx as usize].os_name())
    }

    fn query(&self) -> Option<String> {
        self.search.borrow().as_ref().map(|s| s.0.clone())
    }

    fn title(&self) -> String {
        let cwd = self.cwd.borrow();
        match cwd.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => cwd.display().to_string(),
        }
    }

    fn bump_sort(&self) {
        self.sort_gen.set(self.sort_gen.get() + 1);
    }
}

/// UI-thread state. Workers report back with [`post`].
struct App {
    ui: slint::Weak<AppWindow>,
    tabs: RefCell<Vec<Rc<Tab>>>,
    tab_model: Rc<VecModel<TabInfo>>,
    active: Cell<usize>,
    next_id: Cell<u64>,
    typeahead: RefCell<(String, Option<Instant>)>,
    /// Shell context menus come from the separate fex-shell-host process.
    host: Arc<MenuHost>,
    menu_seq: Cell<u64>,
    /// The menu last requested from the host: (background?, paths, seq).
    prefetched: RefCell<Option<(bool, Vec<PathBuf>, u64)>>,
    /// Waiting for "더 많은 옵션"; no other query may supersede it meanwhile.
    menu_busy: Cell<bool>,
    /// The newest search; workers for older ones stop and their results are dropped.
    search_seq: Arc<AtomicU64>,
    install_offered: Cell<bool>,
    /// Sidebar: user folders, the default locations, and the mounted drives.
    favorites: RefCell<Vec<PathBuf>>,
    desktop: Option<PathBuf>,
    downloads: Option<PathBuf>,
    drives: Vec<shell::Drive>,
    /// A left press may turn into a drag; cleared when the drag starts.
    drag_armed: Cell<bool>,
    /// Pressing an already-selected row keeps the selection until release, so it can be dragged.
    pending_collapse: Cell<Option<u32>>,
    /// Selection to build the rubber-band range on (empty unless Ctrl is held).
    rubber_base: RefCell<Vec<bool>>,
    rubber_last: Cell<(usize, usize)>,
    drop_row: Cell<i32>,
}

thread_local! {
    static APP: OnceCell<App> = const { OnceCell::new() };
}

fn app<R>(f: impl FnOnce(&App) -> R) -> R {
    APP.with(|a| f(a.get().expect("app initialized")))
}

/// Runs `f` on the UI thread.
fn post(f: impl FnOnce(&App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || app(f));
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

impl App {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window alive")
    }

    fn tab(&self) -> Rc<Tab> {
        self.tabs.borrow()[self.active.get()].clone()
    }

    fn find(&self, id: u64) -> Option<Rc<Tab>> {
        self.tabs.borrow().iter().find(|t| t.id == id).cloned()
    }

    /// The tab, if `epoch` is still its latest load.
    fn current(&self, id: u64, epoch: u64) -> Option<Rc<Tab>> {
        self.find(id).filter(|t| t.epoch.load(SeqCst) == epoch)
    }

    fn is_active(&self, t: &Tab) -> bool {
        self.tab().id == t.id
    }

    fn hwnd(&self) -> isize {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match self.ui().window().window_handle().window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        }
    }

    fn minimize_window(&self) {
        self.ui().window().set_minimized(true);
    }

    fn toggle_maximize(&self) {
        let ui = self.ui();
        let window = ui.window();
        window.set_maximized(!window.is_maximized());
    }

    fn close_window(&self) {
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd() as _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }

    // ---------- tabs ----------

    fn new_tab(&self, path: PathBuf) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let tab = Rc::new(Tab::new(id));
        let i = {
            let mut tabs = self.tabs.borrow_mut();
            tabs.push(tab.clone());
            tabs.len() - 1
        };
        self.select_tab(i, i > 0);
        self.navigate(&tab, path, Hist::Push, None);
    }

    fn select_tab(&self, i: usize, save_scroll: bool) {
        if i >= self.tabs.borrow().len() {
            return;
        }
        let ui = self.ui();
        if save_scroll {
            self.tab().scroll.set(ui.get_list_y());
        }
        self.active.set(i);
        let t = self.tab();
        ui.set_rows(ModelRc::from(t.rows.clone()));
        ui.set_active_tab(i as i32);
        self.sync_view(&t);
        self.sync_fit(&t);
        ui.set_list_y(t.scroll.get());
        ui.invoke_focus_list();
    }

    fn close_tab(&self, i: usize) {
        let n = self.tabs.borrow().len();
        if i >= n {
            return;
        }
        if n == 1 {
            let _ = slint::quit_event_loop();
            return;
        }
        let closed = self.tabs.borrow_mut().remove(i);
        closed.epoch.fetch_add(1, SeqCst);
        let active = self.active.get();
        if i == active {
            self.active.set(0);
            self.select_tab(i.min(n - 2), false);
        } else {
            self.active.set(if i < active { active - 1 } else { active });
            self.ui().set_active_tab(self.active.get() as i32);
            self.sync_tabs();
        }
    }

    fn cycle_tab(&self, back: bool) {
        let n = self.tabs.borrow().len();
        let i = self.active.get();
        self.select_tab(if back { (i + n - 1) % n } else { (i + 1) % n }, true);
    }

    fn sync_tabs(&self) {
        let titles: Vec<TabInfo> =
            self.tabs.borrow().iter().map(|t| TabInfo { title: t.title().into() }).collect();
        self.tab_model.set_vec(titles);
    }

    /// Pushes the active tab's path, history, sort and status to the window.
    fn sync_view(&self, t: &Tab) {
        if !self.is_active(t) {
            return self.sync_tabs();
        }
        let ui = self.ui();
        let cwd = t.cwd.borrow().clone();
        ui.set_path(cwd.display().to_string().into());
        ui.set_crumbs(ModelRc::from(Rc::new(VecModel::from(crumbs_for(&cwd)))));
        ui.set_folder(t.title().into());
        let (pos, len) = (t.hist_pos.get(), t.history.borrow().len());
        ui.set_can_back(pos > 0);
        ui.set_can_forward(pos + 1 < len);
        let (key, desc) = t.sort.get();
        ui.set_sort_column(SORT_KEYS.iter().position(|&k| k == key).unwrap_or(0) as i32);
        ui.set_sort_desc(desc);
        let query = t.query();
        ui.set_search_mode(query.is_some());
        ui.set_search_text(query.unwrap_or_default().into());
        self.sync_tabs();
        self.sync_places();
        self.update_status(t);
    }

    // ---------- sidebar ----------

    /// Rebuilds the sidebar models; cheap, and it picks up icons as they load.
    fn sync_places(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let cwd = self.tab().cwd.borrow().clone();
        let favorites: Vec<Place> = self.favorites.borrow().iter().map(|p| self.place(p, &cwd, true)).collect();
        let drives: Vec<Place> =
            self.drives.iter().map(|d| self.place_drive(d, &cwd)).collect();
        ui.set_favorites(ModelRc::from(Rc::new(VecModel::from(favorites))));
        ui.set_drives(ModelRc::from(Rc::new(VecModel::from(drives))));
    }

    fn place(&self, path: &Path, cwd: &Path, removable: bool) -> Place {
        let title = match () {
            _ if self.desktop.as_deref().is_some_and(|d| same_path(d, path)) => "바탕 화면".into(),
            _ if self.downloads.as_deref().is_some_and(|d| same_path(d, path)) => "다운로드".into(),
            _ => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| path.display().to_string()),
        };
        Place {
            title: SharedString::from(title),
            path: path.display().to_string().into(),
            active: same_path(path, cwd),
            removable,
            icon: icon_image(IconKey::File(path.to_path_buf())),
        }
    }

    fn place_drive(&self, drive: &shell::Drive, cwd: &Path) -> Place {
        Place {
            title: drive.title.clone().into(),
            path: drive.path.display().to_string().into(),
            active: same_path(&drive.path, cwd),
            removable: false,
            icon: icon_image(IconKey::File(drive.path.clone())),
        }
    }

    fn open_place(&self, path: &str) {
        let t = self.tab();
        let path = PathBuf::from(path);
        // Clicking the folder already shown is a no-op, unless search results hide it.
        if same_path(&t.cwd.borrow(), &path) && t.search.borrow().is_none() {
            return;
        }
        self.navigate(&t, path, Hist::Push, None);
        self.ui().invoke_focus_list();
    }

    fn add_favorite(&self) {
        let cwd = self.tab().cwd.borrow().clone();
        self.add_favorite_path(cwd);
    }

    fn add_favorite_path(&self, path: PathBuf) {
        let t = self.tab();
        if path.as_os_str().is_empty() {
            return;
        }
        {
            let mut favorites = self.favorites.borrow_mut();
            if favorites.iter().any(|p| same_path(p, &path)) {
                drop(favorites);
                return self.set_note(&t, "이미 즐겨찾기에 있음".into());
            }
            favorites.push(path);
            save_favorites(&favorites);
        }
        self.sync_places();
        self.set_note(&t, "즐겨찾기에 추가함".into());
    }

    fn remove_favorite(&self, path: &str) {
        let path = PathBuf::from(path);
        let mut favorites = self.favorites.borrow_mut();
        let Some(i) = favorites.iter().position(|p| same_path(p, &path)) else { return };
        favorites.remove(i);
        save_favorites(&favorites);
        drop(favorites);
        self.sync_places();
    }

    fn status_text(&self, t: &Tab) -> String {
        let n = t.rows.len();
        let sel = t.rows.selected.borrow().iter().filter(|&&s| s).count();
        let mut s = format!("{n}개 항목");
        if sel > 0 {
            s += &format!(" · {sel}개 선택");
        }
        let note = t.note.borrow();
        if !note.is_empty() {
            s += " · ";
            s += &note;
        }
        s
    }

    fn update_status(&self, t: &Tab) {
        if !self.is_active(t) {
            return;
        }
        self.ui().set_status(self.status_text(t).into());
        self.prefetch_for(t);
    }

    /// Status-only refresh: used while dragging a selection rectangle, where rebuilding the
    /// shell menu for every mouse move would be wasteful.
    fn update_status_text(&self, t: &Tab) {
        if self.is_active(t) {
            self.ui().set_status(self.status_text(t).into());
        }
    }

    /// Tells the UI the widest visible text in each column, for double-click auto-fit.
    fn sync_fit(&self, t: &Tab) {
        if !self.is_active(t) {
            return;
        }
        let fit = fit_samples(t);
        let ui = self.ui();
        ui.set_fit_name(fit.name.into());
        ui.set_fit_location(fit.location.into());
        ui.set_fit_date(fit.date.into());
        ui.set_fit_kind(fit.kind.into());
        ui.set_fit_size(fit.size.into());
    }

    fn set_note(&self, t: &Tab, note: String) {
        *t.note.borrow_mut() = note;
        self.update_status(t);
    }

    // ---------- loading ----------

    /// Lists `path` on a worker and streams it in. The old listing stays until the first batch
    /// arrives, so an unreadable folder (or a path that is now a file) leaves the tab where it was.
    /// Never opens files: only an explicit address-bar entry does that.
    fn navigate(&self, t: &Rc<Tab>, path: PathBuf, hist: Hist, after: Option<After>) {
        let epoch = t.epoch.fetch_add(1, SeqCst) + 1;
        t.refresh_due.set(false);
        *t.pending.borrow_mut() = Some((path.clone(), hist, after));
        self.set_note(t, "불러오는 중…".into());
        let (id, cancel) = (t.id, t.epoch.clone());
        thread::spawn(move || {
            let start = Instant::now();
            let res = list_dir(&path, |batch| {
                if cancel.load(SeqCst) != epoch {
                    return false;
                }
                post(move |a| a.batch(id, epoch, batch));
                true
            });
            let took = start.elapsed();
            post(move |a| a.listed(id, epoch, took, res.err()));
        });
    }

    /// Makes a pending navigation the tab's current folder.
    fn commit(&self, t: &Rc<Tab>) {
        let Some((path, hist, after)) = t.pending.take() else { return };
        t.search.take(); // going to a folder leaves the search results
        t.bump_sort();
        t.rows.replace(Vec::new(), Vec::new());
        t.cursor.set(None);
        t.anchor.set(None);
        *t.after.borrow_mut() = after;
        {
            let mut h = t.history.borrow_mut();
            match hist {
                Hist::Go(i) => t.hist_pos.set(i),
                Hist::Push if h.is_empty() => h.push(path.clone()),
                Hist::Push if h[t.hist_pos.get()] != path => {
                    h.truncate(t.hist_pos.get() + 1);
                    h.push(path.clone());
                    t.hist_pos.set(h.len() - 1);
                }
                Hist::Push => {}
            }
        }
        *t.rows.base.borrow_mut() = path.clone();
        if let Some(w) = t.watch.borrow().as_ref() {
            w.watch(path.clone());
        }
        *t.cwd.borrow_mut() = path;
        if self.is_active(t) {
            self.ui().set_list_y(0.0);
        } else {
            t.scroll.set(0.0);
        }
        self.sync_view(t);
    }

    fn batch(&self, id: u64, epoch: u64, batch: Vec<Entry>) {
        let Some(t) = self.current(id, epoch) else { return };
        self.commit(&t);
        let rows = &t.rows;
        let (start, n) = {
            let mut entries = rows.entries.borrow_mut();
            let start = entries.len();
            Arc::make_mut(&mut entries).extend(batch);
            let end = entries.len();
            let mut order = rows.order.borrow_mut();
            let first = order.len();
            order.extend((start as u32..end as u32).filter(|&i| shown(&entries[i as usize])));
            rows.selected.borrow_mut().resize(end, false);
            (first, order.len() - first)
        };
        rows.notify.row_added(start, n);
        self.update_status(&t);
    }

    fn listed(&self, id: u64, epoch: u64, took: Duration, err: Option<io::Error>) {
        let Some(t) = self.current(id, epoch) else { return };
        if let Some(e) = err {
            if t.pending.take().is_some() {
                self.sync_view(&t); // never shown; put the address bar back
                return self.set_note(&t, format!("열 수 없음: {e}"));
            }
            return self.set_note(&t, format!("일부만 읽음: {e}"));
        }
        self.commit(&t); // an empty folder sends no batches
        t.list_ms.set(ms(took));
        self.sync_fit(&t);
        self.resort(&t);
    }

    /// Sorts on a worker, then swaps in the new order.
    fn resort(&self, t: &Rc<Tab>) {
        t.bump_sort();
        let seq = t.sort_gen.get();
        let entries = t.rows.entries.borrow().clone();
        // Identifies the list being sorted. (The order's length can't: hidden items are left out.)
        let sorting = (Arc::as_ptr(&entries) as usize, entries.len());
        let (key, desc) = t.sort.get();
        let id = t.id;
        thread::spawn(move || {
            let start = Instant::now();
            let order = display_order(&entries, key, desc);
            let took = start.elapsed();
            post(move |a| a.sorted(id, seq, sorting, order, took));
        });
    }

    fn sorted(&self, id: u64, seq: u64, sorting: (usize, usize), order: Vec<u32>, took: Duration) {
        let Some(t) = self.find(id) else { return };
        let current = {
            let entries = t.rows.entries.borrow();
            (Arc::as_ptr(&entries) as usize, entries.len())
        };
        // Stale if the user navigated or sorted again, or entries changed while sorting.
        if seq != t.sort_gen.get() || sorting != current {
            return;
        }
        *t.rows.order.borrow_mut() = order;
        if t.search.borrow().is_none() {
            *t.note.borrow_mut() = format!("목록 {:.1} ms · 정렬 {:.1} ms", t.list_ms.get(), ms(took));
        }
        self.apply_after(&t);
        t.rows.notify.reset();
        self.reveal_cursor(&t);
        self.update_status(&t);
    }

    /// Re-lists the current folder in one piece (no flicker), keeping the selection by name
    /// unless `after` names what to select.
    fn reload(&self, t: &Rc<Tab>, after: After) {
        if let Some(q) = t.query() {
            return self.search_in(t, q); // refresh the results instead
        }
        let epoch = t.epoch.fetch_add(1, SeqCst) + 1;
        *t.after.borrow_mut() = Some(after);
        let path = t.cwd.borrow().clone();
        let (key, desc) = t.sort.get();
        let (id, cancel) = (t.id, t.epoch.clone());
        thread::spawn(move || {
            let start = Instant::now();
            let mut all = Vec::new();
            let res = list_dir(&path, |b| {
                all.extend(b);
                cancel.load(SeqCst) == epoch
            });
            let took = start.elapsed();
            let order = display_order(&all, key, desc);
            post(move |a| a.reloaded(id, epoch, all, order, took, res.err()));
        });
    }

    fn reloaded(&self, id: u64, epoch: u64, all: Vec<Entry>, order: Vec<u32>, took: Duration, err: Option<io::Error>) {
        let Some(t) = self.current(id, epoch) else { return };
        let auto = t.after.borrow().as_ref().is_some_and(|a| a.auto);
        if let Some(e) = err {
            return self.set_note(&t, format!("새로 고칠 수 없음: {e}"));
        }
        if let Some(after) = t.after.borrow_mut().as_mut().filter(|a| a.select.is_empty()) {
            after.select = t.rows.selection().into_iter().map(|i| t.rows.name(i)).collect();
        }
        t.bump_sort();
        t.rows.replace(all, order);
        t.list_ms.set(ms(took));
        t.cursor.set(None);
        t.anchor.set(None);
        self.apply_after(&t);
        t.rows.notify.reset();
        self.reveal_cursor(&t);
        let quiet = !t.note.borrow().is_empty();
        if auto && !quiet {
            *t.note.borrow_mut() = "변경 감지 · 자동 새로 고침".into();
        }
        self.update_status(&t);
        self.sync_fit(&t);
    }

    fn apply_after(&self, t: &Tab) {
        let Some(after) = t.after.take() else { return };
        let names: HashSet<&str> = after.select.iter().map(String::as_str).collect();
        {
            let entries = t.rows.entries.borrow();
            let mut sel = t.rows.selected.borrow_mut();
            for (i, e) in entries.iter().enumerate() {
                sel[i] = names.contains(e.name().as_str());
            }
        }
        let first = t.rows.selection().first().copied();
        t.cursor.set(first);
        t.anchor.set(first);
        if after.rename {
            t.rows.editing.set(first);
        }
    }

    // ---------- selection ----------

    fn cursor_row(&self, t: &Tab) -> Option<usize> {
        t.cursor.get().and_then(|c| t.rows.row_of(c))
    }

    fn reveal_cursor(&self, t: &Tab) {
        if let Some(r) = self.cursor_row(t).filter(|_| self.is_active(t)) {
            self.ensure_visible(r);
        }
    }

    fn ensure_visible(&self, row: usize) {
        let ui = self.ui();
        let h = ui.get_row_height();
        let y = row as f32 * h;
        let top = -ui.get_list_y();
        let visible = ui.get_list_height();
        if y < top {
            ui.set_list_y(-y);
        } else if y + h > top + visible {
            ui.set_list_y(-(y + h - visible));
        }
    }

    fn row_pressed(&self, row: usize, ctrl: bool, shift: bool) {
        let t = self.tab();
        let Some(idx) = t.rows.entry(row) else { return };
        self.drag_armed.set(true);
        self.pending_collapse.set(None);
        let multi = t.rows.selected.borrow().iter().filter(|&&s| s).count() > 1;
        if shift {
            let anchor = t.anchor.get().and_then(|a| t.rows.row_of(a)).unwrap_or(row);
            t.rows.select_range(anchor, row, ctrl);
        } else if ctrl {
            t.rows.toggle(idx);
            t.anchor.set(Some(idx));
        } else if multi && t.rows.selected.borrow()[idx as usize] {
            // Keep the selection so the whole group can be dragged; collapse on release
            // if this turns out to be a plain click.
            self.pending_collapse.set(Some(idx));
            t.anchor.set(Some(idx));
        } else {
            t.rows.select_only(Some(idx));
            t.anchor.set(Some(idx));
        }
        t.cursor.set(Some(idx));
        self.update_status(&t);
    }

    /// A plain click on a row that was part of a multi-selection collapses the selection.
    fn row_clicked(&self, row: usize) {
        self.drag_armed.set(false);
        let t = self.tab();
        let Some(idx) = t.rows.entry(row) else { return };
        if self.pending_collapse.replace(None) == Some(idx) {
            t.rows.select_only(Some(idx));
            t.cursor.set(Some(idx));
            self.update_status(&t);
        }
    }

    /// Wheel (middle) click: open the folder in a new tab, like the context menu command.
    fn row_middle(&self, row: usize) {
        let t = self.tab();
        let Some(i) = t.rows.entry(row) else { return };
        if t.rows.is_dir(i) {
            self.new_tab(t.path_of(i));
        }
    }

    /// Left press moved far enough: hand the selection to OLE as a native drag.
    fn row_drag(&self, row: usize) {
        if !self.drag_armed.replace(false) {
            return;
        }
        // `moved` also fires while another mouse button is held; only a left drag counts.
        if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000 == 0 {
            return;
        }
        let t = self.tab();
        let Some(idx) = t.rows.entry(row) else { return };
        if t.rows.editing.get().is_some() {
            return;
        }
        self.pending_collapse.set(None);
        let sel = t.rows.selection();
        let rows = if sel.contains(&idx) { sel } else { vec![idx] };
        let paths: Vec<PathBuf> = rows.into_iter().map(|i| t.path_of(i)).collect();
        // DoDragDrop runs its own modal loop; start it once the pointer event is done.
        let id = t.id;
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(moved) = dnd::start_drag(paths) {
                // OLE consumes the mouse release that ended the drag; tell the widgets the
                // button is up so the row does not stay stuck in its pressed state.
                app(|a| a.end_drag_pointer());
                if moved {
                    // A MOVE finished: the source folder changed (an external target), or the
                    // drop handler is about to refresh it for an internal one.
                    slint::Timer::single_shot(Duration::from_millis(300), move || post(move |a| a.reload_tab(id)));
                }
            }
        });
    }

    /// Releases the left button in Slint's eyes after DoDragDrop ate the real mouse-up.
    fn end_drag_pointer(&self) {
        let ui = self.ui();
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(HWND(self.hwnd() as _), &mut pt);
        }
        let scale = ui.window().scale_factor();
        let position = slint::LogicalPosition::new(pt.x as f32 / scale, pt.y as f32 / scale);
        ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
    }

    fn rubber_start(&self, ctrl: bool) {
        let t = self.tab();
        *self.rubber_base.borrow_mut() = if ctrl { t.rows.selected.borrow().clone() } else { Vec::new() };
        self.rubber_last.set((usize::MAX, 0));
    }

    /// Selects the rows the rubber band covers. `y1`/`y2` are in list-local pixels.
    fn rubber_move(&self, y1: f32, y2: f32) {
        let t = self.tab();
        let n = t.rows.len();
        if n == 0 {
            return;
        }
        let ui = self.ui();
        let h = ui.get_row_height().max(1.0);
        // y is visible; list-y is the content offset (negative when scrolled down).
        let offset = ui.get_list_y();
        let a = ((y1.min(y2) - offset) / h).floor().max(0.0) as usize;
        let b = (((y1.max(y2) - offset) / h).floor().max(0.0) as usize).min(n - 1);
        let empty = a > b || a >= n;
        let key = if empty { (usize::MAX, usize::MAX) } else { (a, b) };
        if self.rubber_last.replace(key) == key {
            return;
        }
        let base = self.rubber_base.borrow();
        let mut want = vec![false; n];
        if base.len() == n {
            want.copy_from_slice(&base);
        }
        if !empty {
            for r in a..=b {
                if let Some(i) = t.rows.entry(r) {
                    want[i as usize] = true;
                }
            }
        }
        t.rows.update_selection(|sel| *sel = want);
        self.update_status_text(&t);
    }

    fn rubber_end(&self) {
        self.rubber_base.borrow_mut().clear();
        self.rubber_last.set((usize::MAX, 0));
        let t = self.tab();
        self.update_status(&t);
    }

    fn reload_tab(&self, id: u64) {
        if let Some(t) = self.find(id) {
            self.reload(&t, After::default());
        }
    }

    /// F5 or the tool button: re-list the folder (or re-run the search).
    fn refresh(&self) {
        let t = self.tab();
        self.reload(&t, After::default());
    }

    /// The watcher saw a change: let a burst settle, then re-list once.
    fn folder_changed(&self, id: u64) {
        let Some(t) = self.find(id) else { return };
        if t.refresh_due.replace(true) {
            return;
        }
        slint::Timer::single_shot(Duration::from_millis(400), move || post(move |a| a.flush_refresh(id)));
    }

    fn flush_refresh(&self, id: u64) {
        let Some(t) = self.find(id) else { return };
        if !t.refresh_due.replace(false) {
            return;
        }
        // Don't pull the list out from under a rename box; check again shortly.
        if t.rows.editing.get().is_some() {
            t.refresh_due.set(true);
            slint::Timer::single_shot(Duration::from_secs(1), move || post(move |a| a.flush_refresh(id)));
            return;
        }
        // A navigation or search already in flight brings its own contents.
        if t.search.borrow().is_some() || t.pending.borrow().is_some() {
            return;
        }
        self.reload(&t, After { auto: true, ..Default::default() });
    }

    fn move_cursor(&self, t: &Tab, row: usize, shift: bool, ctrl: bool) {
        let n = t.rows.len();
        if n == 0 {
            return;
        }
        let row = row.min(n - 1);
        let idx = t.rows.entry(row).unwrap();
        if shift {
            let anchor = t.anchor.get().and_then(|a| t.rows.row_of(a)).unwrap_or(row);
            t.rows.select_range(anchor, row, false);
        } else if !ctrl {
            t.rows.select_only(Some(idx));
            t.anchor.set(Some(idx));
        }
        t.cursor.set(Some(idx));
        self.ensure_visible(row);
        self.update_status(t);
    }

    fn type_ahead(&self, t: &Tab, text: &str) {
        let now = Instant::now();
        let prefix = {
            let mut ta = self.typeahead.borrow_mut();
            if ta.1.is_none_or(|last| now - last > Duration::from_secs(1)) {
                ta.0.clear();
            }
            ta.0 += &text.to_lowercase();
            ta.1 = Some(now);
            ta.0.clone()
        };
        let n = t.rows.len();
        let cur = self.cursor_row(t).unwrap_or(n.saturating_sub(1));
        // A single repeated letter cycles through matches, like Explorer.
        let start = if prefix.chars().count() == 1 { cur + 1 } else { cur };
        let hit = (0..n)
            .map(|k| (start + k) % n)
            .find(|&r| t.rows.name(t.rows.entry(r).unwrap()).to_lowercase().starts_with(&prefix));
        if let Some(r) = hit {
            self.move_cursor(t, r, false, false);
        }
    }

    // ---------- keyboard ----------

    fn key(&self, text: &str, ctrl: bool, shift: bool, alt: bool, in_list: bool) -> bool {
        let is = |k: Key| text == SharedString::from(k).as_str();
        // Ctrl+letter may arrive as a control character (Ctrl+C = U+0003).
        let ch = match text.chars().next() {
            Some(c @ '\u{1}'..='\u{1a}') if ctrl => char::from(c as u8 + b'a' - 1),
            Some(c) => c.to_ascii_lowercase(),
            None => return false,
        };
        let t = self.tab();

        // Global shortcuts (also while the address bar has focus).
        match () {
            _ if ctrl && ch == 't' => self.new_tab(t.cwd.borrow().clone()),
            _ if ctrl && (ch == 'w' || is(Key::F4)) => self.close_tab(self.active.get()),
            _ if ctrl && (is(Key::Tab) || is(Key::Backtab)) => self.cycle_tab(shift || is(Key::Backtab)),
            _ if alt && is(Key::LeftArrow) => self.back(),
            _ if alt && is(Key::RightArrow) => self.forward(),
            _ if alt && is(Key::UpArrow) => self.up(),
            _ if (ctrl && ch == 'l') || (alt && ch == 'd') || (!ctrl && is(Key::F4)) => self.ui().invoke_focus_address(),
            _ if (ctrl && ch == 'f') || is(Key::F3) => self.ui().invoke_focus_search(),
            _ if is(Key::F5) || (ctrl && ch == 'r') => self.reload(&t, After::default()),
            _ if is(Key::Escape) && t.search.borrow().is_some() => {
                self.exit_search(&t);
                self.ui().invoke_focus_list();
            }
            _ if is(Key::Escape) && !in_list => {
                self.sync_view(&t);
                self.ui().invoke_focus_list();
            }
            _ if !in_list => return false,

            // File list.
            _ if alt && is(Key::Return) => self.properties(),
            _ if is(Key::Return) => self.open_selection(),
            _ if (shift && is(Key::F10)) || is(Key::Menu) => {
                let _ = slint::invoke_from_event_loop(|| app(|a| a.keyboard_menu()));
            }
            _ if ctrl && shift && ch == 'c' => self.copy_path(),
            _ if is(Key::Backspace) => self.back(),
            _ if ctrl && shift && ch == 'n' => self.new_folder(),
            _ if ctrl && ch == 'a' => {
                t.rows.update_selection(|sel| sel.fill(true));
                self.update_status(&t);
            }
            _ if ctrl && ch == 'c' => self.copy(false),
            _ if ctrl && ch == 'x' => self.copy(true),
            _ if ctrl && ch == 'v' => self.paste(),
            _ if is(Key::Delete) => self.delete(shift),
            _ if is(Key::F2) => self.start_rename(),
            _ if ctrl && ch == ' ' => {
                if let Some(c) = t.cursor.get() {
                    t.rows.toggle(c);
                }
                self.update_status(&t);
            }
            _ if is(Key::UpArrow) || is(Key::DownArrow) || is(Key::Home) || is(Key::End)
                || is(Key::PageUp) || is(Key::PageDown) =>
            {
                let ui = self.ui();
                let page = ((ui.get_list_height() / ui.get_row_height()) as usize).saturating_sub(1).max(1);
                let n = t.rows.len();
                let cur = self.cursor_row(&t);
                let row = match cur {
                    _ if is(Key::Home) => 0,
                    _ if is(Key::End) => n.saturating_sub(1),
                    None => 0,
                    Some(r) if is(Key::UpArrow) => r.saturating_sub(1),
                    Some(r) if is(Key::DownArrow) => r + 1,
                    Some(r) if is(Key::PageUp) => r.saturating_sub(page),
                    Some(r) => r + page,
                };
                self.move_cursor(&t, row, shift, ctrl);
            }
            // Printable text jumps to a matching name (Key::* specials live in the private-use area).
            _ if !ctrl && !alt && text.chars().all(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c)) => {
                self.type_ahead(&t, text)
            }
            _ => return false,
        }
        true
    }

    // ---------- navigation ----------

    fn open_rows(&self, t: &Rc<Tab>, idxs: Vec<u32>) {
        if let [i] = idxs[..]
            && t.rows.is_dir(i)
        {
            return self.navigate(t, t.path_of(i), Hist::Push, None);
        }
        let files: Vec<PathBuf> = idxs.into_iter().filter(|&i| !t.rows.is_dir(i)).map(|i| t.path_of(i)).collect();
        let id = t.id;
        thread::spawn(move || {
            for f in files {
                if let Err(e) = shell::open(&f) {
                    post(move |a| {
                        if let Some(t) = a.find(id) {
                            a.set_note(&t, format!("열 수 없음: {e}"));
                        }
                    });
                }
            }
        });
    }

    fn open_selection(&self) {
        let t = self.tab();
        let mut sel = t.rows.selection();
        if sel.is_empty() {
            sel.extend(t.cursor.get());
        }
        self.open_rows(&t, sel);
    }

    fn back(&self) {
        let t = self.tab();
        let pos = t.hist_pos.get();
        if pos > 0 {
            let p = t.history.borrow()[pos - 1].clone();
            self.navigate(&t, p, Hist::Go(pos - 1), None);
        }
    }

    fn forward(&self) {
        let t = self.tab();
        let pos = t.hist_pos.get();
        let next = t.history.borrow().get(pos + 1).cloned();
        if let Some(p) = next {
            self.navigate(&t, p, Hist::Go(pos + 1), None);
        }
    }

    /// Goes to the parent and selects the folder we came from.
    fn up(&self) {
        let t = self.tab();
        let cwd = t.cwd.borrow().clone();
        if let (Some(parent), Some(name)) = (cwd.parent(), cwd.file_name()) {
            let after = After { select: vec![name.to_string_lossy().into_owned()], ..Default::default() };
            self.navigate(&t, parent.to_path_buf(), Hist::Push, Some(after));
        }
    }

    fn address(&self, text: &str) {
        let t = self.tab();
        let mut s = text.trim().trim_matches('"').to_string();
        if s.is_empty() {
            return;
        }
        if s.len() == 2 && s.ends_with(':') {
            s.push('\\'); // "D:" means the drive root here, not D:'s current directory
        }
        let p = t.cwd.borrow().join(s); // absolute input replaces cwd
        self.ui().invoke_focus_list();
        // The user typed this path, so a file is opened; anything else is navigated to.
        let id = t.id;
        thread::spawn(move || {
            if p.is_file() {
                let r = shell::open(&p);
                post(move |a| {
                    if let Some(t) = a.find(id) {
                        a.sync_view(&t);
                        a.set_note(&t, r.err().map(|e| format!("열 수 없음: {e}")).unwrap_or_default());
                    }
                });
            } else {
                post(move |a| {
                    if let Some(t) = a.find(id) {
                        a.navigate(&t, p, Hist::Push, None);
                    }
                });
            }
        });
    }

    fn sort(&self, col: usize) {
        let t = self.tab();
        let key = SORT_KEYS[col.min(3)];
        let (cur, desc) = t.sort.get();
        t.sort.set((key, cur == key && !desc));
        self.sync_view(&t);
        self.resort(&t);
    }

    // ---------- search ----------

    /// Search box edits: search every drive through the index service.
    fn search(&self, text: &str) {
        let t = self.tab();
        match text.trim() {
            "" => self.exit_search(&t),
            q => self.search_in(&t, q.to_string()),
        }
    }

    fn search_in(&self, t: &Rc<Tab>, query: String) {
        if let Some(text) = content_query(&query) {
            return self.content_search(t, query, text);
        }
        let seq = self.search_seq.fetch_add(1, SeqCst) + 1;
        let (id, latest) = (t.id, self.search_seq.clone());
        thread::spawn(move || {
            let start = Instant::now();
            let r = index::query(&query, SEARCH_LIMIT);
            let took = start.elapsed();
            if latest.load(SeqCst) != seq {
                return; // typed on; a newer search is running
            }
            match r {
                Ok(res) => {
                    // The index holds names only; size and date come from the file system.
                    let entries = res.hits.iter().map(hit_entry).collect();
                    post(move |a| a.found(id, seq, query, res, entries, took));
                }
                Err(QueryError::NotInstalled) => post(move |a| a.offer_install(id, query)),
                Err(QueryError::Unavailable(e)) => post(move |a| {
                    if let Some(t) = a.find(id) {
                        a.set_note(&t, format!("검색할 수 없음: {e}"));
                    }
                }),
            }
        });
    }

    fn found(&self, id: u64, seq: u64, query: String, res: Results, entries: Vec<Entry>, took: Duration) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        t.epoch.fetch_add(1, SeqCst); // drop any folder listing still arriving
        t.pending.take();
        let (key, desc) = t.sort.get();
        let order = display_order(&entries, key, desc);
        let paths: Vec<PathBuf> = res.hits.into_iter().map(|h| h.path).collect();
        let locations = paths.iter().map(|p| p.parent().map(|d| d.display().to_string()).unwrap_or_default()).collect();
        t.bump_sort();
        t.rows.replace(entries, order);
        *t.rows.locations.borrow_mut() = locations;
        t.cursor.set(None);
        t.anchor.set(None);
        let mut note = format!("검색 {:.0} ms", ms(took));
        if res.more {
            note += &format!(" · 더 있음, 처음 {}개 표시", paths.len());
        }
        if res.indexing > 0 {
            note += " · 색인 중이라 결과가 아직 일부";
            // Ask again shortly, until the index is complete.
            let q = query.clone();
            slint::Timer::single_shot(Duration::from_secs(2), move || {
                app(|a| {
                    if let Some(t) = a.find(id)
                        && a.search_seq.load(SeqCst) == seq
                        && t.query().as_deref() == Some(&q)
                    {
                        a.search_in(&t, q);
                    }
                })
            });
        }
        *t.search.borrow_mut() = Some((query, paths));
        *t.note.borrow_mut() = note;
        if self.is_active(&t) {
            self.ui().set_search_mode(true);
            self.ui().set_list_y(0.0);
        }
        self.update_status(&t);
        self.sync_fit(&t);
    }

    /// "내용:<text>" searches inside the files under the current folder; hits stream in.
    fn content_search(&self, t: &Rc<Tab>, query: String, text: String) {
        let root = t.cwd.borrow().clone();
        if root.as_os_str().is_empty() {
            return;
        }
        let seq = self.search_seq.fetch_add(1, SeqCst) + 1;
        // Switch to an empty result list at once; it fills as files match.
        t.epoch.fetch_add(1, SeqCst);
        t.pending.take();
        t.bump_sort();
        t.rows.replace(Vec::new(), Vec::new());
        t.cursor.set(None);
        t.anchor.set(None);
        *t.search.borrow_mut() = Some((query, Vec::new()));
        *t.note.borrow_mut() = format!("'{}' 아래에서 내용 검색 중…", t.title());
        if self.is_active(t) {
            self.ui().set_search_mode(true);
            self.ui().set_list_y(0.0);
        }
        self.update_status(t);

        let (id, latest) = (t.id, self.search_seq.clone());
        // Walking a whole tree per keystroke is wasteful: start once typing pauses.
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            if latest.load(SeqCst) == seq {
                thread::spawn(move || content_worker(id, seq, latest, root, text));
            }
        });
    }
}

/// Runs a content search and streams the matching files to the tab in batches.
fn content_worker(id: u64, seq: u64, latest: Arc<AtomicU64>, root: PathBuf, text: String) {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let start = Instant::now();
    let pending = Mutex::new(Vec::<PathBuf>::new());
    let (count, done) = (AtomicUsize::new(0), AtomicBool::new(false));
    let flush = || {
        let batch = std::mem::take(&mut *pending.lock().unwrap());
        if !batch.is_empty() {
            let entries = batch.iter().map(|p| hit_entry(&index::Hit { path: p.clone(), dir: false })).collect();
            post(move |a| a.content_batch(id, seq, batch, entries));
        }
    };
    let r = thread::scope(|s| {
        // Shows hits within ~0.1 s even when they're sparse (a cold disk can take minutes to
        // finish), while batching keeps the UI cheap when they pour in.
        s.spawn(|| {
            while !done.load(SeqCst) {
                thread::sleep(Duration::from_millis(100));
                flush();
            }
        });
        let r = content::search(&root, &text, |path| {
            if latest.load(SeqCst) != seq {
                return false; // the query changed
            }
            pending.lock().unwrap().push(path);
            count.fetch_add(1, SeqCst) + 1 < CONTENT_LIMIT
        });
        done.store(true, SeqCst);
        r
    });
    flush();
    let (took, capped) = (start.elapsed(), count.load(SeqCst) >= CONTENT_LIMIT);
    post(move |a| a.content_done(id, seq, took, capped, r.err()));
}

impl App {
    fn content_batch(&self, id: u64, seq: u64, paths: Vec<PathBuf>, entries: Vec<Entry>) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        let rows = &t.rows;
        let (start, n) = {
            let mut all = rows.entries.borrow_mut();
            let start = all.len();
            Arc::make_mut(&mut all).extend(entries);
            let end = all.len();
            rows.order.borrow_mut().extend(start as u32..end as u32);
            rows.selected.borrow_mut().resize(end, false);
            rows.locations
                .borrow_mut()
                .extend(paths.iter().map(|p| p.parent().map(|d| d.display().to_string()).unwrap_or_default()));
            if let Some((_, hits)) = t.search.borrow_mut().as_mut() {
                hits.extend(paths);
            }
            (start, end - start)
        };
        rows.notify.row_added(start, n);
        self.update_status(&t);
    }

    fn content_done(&self, id: u64, seq: u64, took: Duration, capped: bool, err: Option<String>) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        let mut note = format!("내용 검색 {:.1} s", took.as_secs_f64());
        if capped {
            note += &format!(" · 처음 {CONTENT_LIMIT}개에서 멈춤");
        }
        if let Some(e) = err {
            note += &format!(" · {e}");
        }
        *t.note.borrow_mut() = note;
        self.sync_fit(&t);
        self.resort(&t);
    }

    fn exit_search(&self, t: &Rc<Tab>) {
        self.search_seq.fetch_add(1, SeqCst); // results still on their way are dropped
        if t.search.take().is_some() {
            self.reload(t, After::default());
        }
        self.sync_view(t);
    }

    /// The first search without the index service asks to install it (UAC).
    fn offer_install(&self, id: u64, query: String) {
        let Some(t) = self.find(id) else { return };
        if self.install_offered.replace(true) {
            return self.set_note(&t, "검색에는 인덱스 서비스가 필요함 (설치되지 않음)".into());
        }
        let hwnd = self.hwnd();
        let here = std::env::current_exe().unwrap_or_default();
        // Elevating an exe the user (or anything running as them) can overwrite would hand that
        // code admin rights; only offer it from an admin-only folder such as Program Files.
        if !shell::program_files().is_ok_and(|pf| here.starts_with(pf)) {
            return self.set_note(
                &t,
                "인덱스 서비스가 설치되지 않음 · 관리자 권한으로 'fex-index install'을 실행하세요".into(),
            );
        }
        let exe = here.with_file_name("fex-index.exe");
        thread::spawn(move || {
            let ask = "모든 드라이브를 즉시 검색하려면 Fast Explorer 인덱스 서비스가 필요합니다.\n\n\
                지금 설치할까요?\n\
                · 관리자 권한(UAC)이 필요합니다.\n\
                · 고정 NTFS 드라이브의 파일 이름만 색인합니다. 파일 내용은 읽지 않습니다.\n\
                · 검색은 이 Windows 계정에서만 쓸 수 있습니다.";
            let r = shell::confirm(hwnd, ask).then(|| shell::run_elevated(&exe, "install", hwnd));
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                match r {
                    None => a.set_note(&t, "검색에는 인덱스 서비스가 필요함".into()),
                    Some(Ok(0)) => {
                        a.set_note(&t, "인덱스 서비스 설치됨 · 색인 중…".into());
                        // Give the service a moment to open its pipe.
                        slint::Timer::single_shot(Duration::from_secs(1), move || app(|a| a.search_in(&t, query)));
                    }
                    Some(Ok(code)) => a.set_note(&t, format!("인덱스 서비스 설치 실패 (코드 {code})")),
                    Some(Err(e)) => a.set_note(&t, format!("인덱스 서비스 설치 취소: {e}")),
                }
            });
        });
    }

    // ---------- context menus ----------

    fn next_seq(&self) -> u64 {
        self.menu_seq.set(self.menu_seq.get() + 1);
        self.menu_seq.get()
    }

    /// Asks the host for a menu unless that same menu is already requested; returns its seq.
    fn prefetch(&self, background: bool, paths: Vec<PathBuf>) -> Result<u64, String> {
        if let Some((bg, p, seq)) = &*self.prefetched.borrow()
            && *bg == background
            && *p == paths
        {
            return Ok(*seq);
        }
        if !background && paths.iter().any(|p| p.parent() != paths[0].parent()) {
            return Err("셸 메뉴는 한 폴더 안의 항목에만 쓸 수 있음".into()); // e.g. search hits
        }
        let seq = self.next_seq();
        let req = match background {
            true => Request::Background { seq, folder: paths[0].clone() },
            false => Request::Items { seq, paths: paths.clone() },
        };
        self.host.send(&req)?;
        *self.prefetched.borrow_mut() = Some((background, paths, seq));
        Ok(seq)
    }

    /// Keeps the shell menu for the selection (or, with nothing selected, the folder) warm.
    fn prefetch_for(&self, t: &Tab) {
        let cwd = t.cwd.borrow().clone();
        if self.menu_busy.get() || cwd.as_os_str().is_empty() {
            return;
        }
        let sel = t.rows.selection();
        let _ = match sel.len() {
            0 => self.prefetch(true, vec![cwd]),
            1..=500 => self.prefetch(false, sel.iter().map(|&i| t.path_of(i)).collect()),
            _ => return, // huge selections: built on demand ("더 많은 옵션")
        };
    }

    /// The mouse usually rests on an item before right-clicking it: start building its menu.
    fn row_hover(&self, row: usize) {
        let t = self.tab();
        if self.menu_busy.get() || self.ui().get_rubber_on() {
            return;
        }
        if let Some(i) = t.rows.entry(row)
            && !t.rows.selected.borrow()[i as usize]
        {
            let _ = self.prefetch(false, vec![t.path_of(i)]);
        }
    }

    fn row_menu(&self, row: usize) {
        let t = self.tab();
        self.drag_armed.set(false);
        self.pending_collapse.set(None);
        let Some(i) = t.rows.entry(row) else { return };
        if !t.rows.selected.borrow()[i as usize] {
            t.rows.select_only(Some(i));
            t.anchor.set(Some(i));
        }
        t.cursor.set(Some(i));
        self.update_status(&t);
        let at = cursor_pos();
        // Run the menu's modal loop outside the pointer-event handler.
        let _ = slint::invoke_from_event_loop(move || app(|a| a.context_menu(false, at)));
    }

    fn blank_pressed(&self, right: bool, ctrl: bool) {
        let t = self.tab();
        self.drag_armed.set(false);
        self.pending_collapse.set(None);
        if right || !ctrl {
            t.rows.select_only(None);
            self.update_status(&t);
        }
        if right {
            let at = cursor_pos();
            let _ = slint::invoke_from_event_loop(move || app(|a| a.context_menu(true, at)));
        }
    }

    fn keyboard_menu(&self) {
        let t = self.tab();
        let background = t.rows.selection().is_empty();
        let at = self.row_screen_pos(if background { None } else { self.cursor_row(&t) });
        self.context_menu(background, at);
    }

    /// Screen point just below `row` (or the list's top-left), for keyboard-opened menus.
    fn row_screen_pos(&self, row: Option<usize>) -> (i32, i32) {
        let ui = self.ui();
        let scale = ui.window().scale_factor();
        let y = ui.get_list_origin_y()
            + row.map_or(0.0, |r| ui.get_list_y() + (r as f32 + 1.0) * ui.get_row_height());
        let mut pt = POINT { x: ((ui.get_list_origin_x() + 40.0) * scale) as i32, y: (y * scale) as i32 };
        unsafe {
            let _ = ClientToScreen(HWND(self.hwnd() as _), &mut pt);
        }
        (pt.x, pt.y)
    }

    /// Our own items show at once; the shell's join them if the host has them ready
    /// (prefetched), else they are one click away under "더 많은 옵션".
    fn context_menu(&self, background: bool, at: (i32, i32)) {
        let t = self.tab();
        let cwd = t.cwd.borrow().clone();
        let paths = if background { vec![cwd.clone()] } else { self.selected_paths(&t) };
        if self.menu_busy.get() || paths.is_empty() || cwd.as_os_str().is_empty() {
            return;
        }
        let seq = self.prefetch(background, paths.clone());
        let shell = seq.as_ref().ok().and_then(|&s| self.host.wait(s, MENU_WAIT));

        let mut top = if background {
            vec![
                Own { disabled: !shell::clipboard_has_files(), ..own(CMD_PASTE, "붙여넣기\tCtrl+V") },
                own(CMD_NEW_FOLDER, "새 폴더\tCtrl+Shift+N"),
                own(CMD_NEW_FILE, "새 텍스트 문서"),
                own(CMD_REFRESH, "새로 고침\tF5"),
                SEP,
                own(CMD_ADD_FAVORITE, "즐겨찾기에 추가"),
            ]
        } else {
            let sel = t.rows.selection();
            let mut v = vec![Own { default: true, ..own(CMD_OPEN, "열기\tEnter") }];
            if let [i] = sel[..]
                && t.rows.is_dir(i)
            {
                v.push(own(CMD_OPEN_TAB, "새 탭에서 열기"));
                v.push(own(CMD_ADD_FAVORITE, "즐겨찾기에 추가"));
            }
            v.extend([
                SEP,
                own(CMD_CUT, "잘라내기\tCtrl+X"),
                own(CMD_COPY, "복사\tCtrl+C"),
                own(CMD_COPY_PATH, "경로 복사\tCtrl+Shift+C"),
                SEP,
                own(CMD_RENAME, "이름 바꾸기\tF2"),
                own(CMD_DELETE, "삭제\tDel"),
            ]);
            v
        };
        let items: &[MenuItem] = match &shell {
            Some(Ok(items)) => items,
            _ => &[],
        };
        match (&seq, &shell) {
            (Ok(_), Some(Ok(_))) => {}
            (Ok(_), None) => top.extend([SEP, own(CMD_MORE, "더 많은 옵션…")]),
            (Ok(_), Some(Err(e))) => {
                self.set_note(&t, format!("셸 메뉴 실패: {e}"));
                top.extend([SEP, own(CMD_RETRY, "더 많은 옵션 (다시 시도)…")]);
            }
            (Err(e), _) => self.set_note(&t, e.clone()),
        }
        let bottom = [own(CMD_PROPERTIES, "속성\tAlt+Enter")];

        match popup::show(self.hwnd(), at, &top, items, &bottom) {
            0 => {}
            id @ 1..=0x7FFF => {
                if let Ok(seq) = seq {
                    self.invoke_shell(Request::Invoke { seq, id });
                }
            }
            CMD_MORE => self.more_options(background, paths, false, at),
            CMD_RETRY => self.more_options(background, paths, true, at),
            CMD_OPEN => self.open_selection(),
            CMD_OPEN_TAB => self.new_tab(paths[0].clone()),
            CMD_CUT => self.copy(true),
            CMD_COPY => self.copy(false),
            CMD_COPY_PATH => self.copy_path(),
            CMD_RENAME => self.start_rename(),
            CMD_DELETE => self.delete(false),
            CMD_PASTE => self.paste(),
            CMD_NEW_FOLDER => self.new_folder(),
            CMD_NEW_FILE => self.new_file(),
            CMD_REFRESH => self.reload(&t, After::default()),
            CMD_ADD_FAVORITE => {
                if let Some(p) = paths.first() {
                    self.add_favorite_path(p.clone());
                }
            }
            CMD_PROPERTIES => self.properties_of(paths),
            _ => {}
        }
    }

    /// Waits (off the UI thread) for the shell menu, then shows it on its own.
    fn more_options(&self, background: bool, paths: Vec<PathBuf>, retry: bool, at: (i32, i32)) {
        if retry {
            self.host.restart();
            *self.prefetched.borrow_mut() = None;
        }
        let seq = match self.prefetch(background, paths) {
            Ok(seq) => seq,
            Err(e) => return self.set_note(&self.tab(), e),
        };
        self.menu_busy.set(true);
        self.set_note(&self.tab(), "셸 메뉴 불러오는 중…".into());
        let host = self.host.clone();
        thread::spawn(move || {
            let r = host.wait(seq, HANG_LIMIT);
            post(move |a| a.show_more(seq, r, at));
        });
    }

    fn show_more(&self, seq: u64, r: Option<MenuResult>, at: (i32, i32)) {
        self.menu_busy.set(false);
        let t = self.tab();
        match r {
            Some(Ok(items)) => {
                self.set_note(&t, String::new());
                let id = popup::show(self.hwnd(), at, &[], &items, &[]);
                if (1..=0x7FFF).contains(&id) {
                    self.invoke_shell(Request::Invoke { seq, id });
                }
            }
            Some(Err(e)) => self.set_note(&t, format!("셸 메뉴 실패: {e}")),
            None => {
                // A shell extension is stuck: replace the host, Fast Explorer carries on.
                self.host.restart();
                *self.prefetched.borrow_mut() = None;
                self.set_note(&t, "셸 메뉴가 응답하지 않아 다시 시작함".into());
            }
        }
    }

    fn invoke_shell(&self, req: Request) {
        // Let the host bring the windows it opens (Open with, Properties, ...) to the front.
        if let Some(pid) = self.host.pid() {
            unsafe {
                let _ = AllowSetForegroundWindow(pid);
            }
        }
        if let Err(e) = self.host.send(&req) {
            self.set_note(&self.tab(), e);
        }
    }

    /// Alt+Enter: properties of the selection, or of the folder.
    fn properties(&self) {
        let t = self.tab();
        let mut paths = self.selected_paths(&t);
        if paths.is_empty() {
            paths.push(t.cwd.borrow().clone());
        }
        self.properties_of(paths);
    }

    fn properties_of(&self, paths: Vec<PathBuf>) {
        let seq = self.next_seq();
        self.invoke_shell(Request::Verb { seq, verb: "properties".into(), paths });
    }

    fn copy_path(&self) {
        let t = self.tab();
        let paths = self.selected_paths(&t);
        if paths.is_empty() {
            return;
        }
        let text = paths.iter().map(|p| format!("\"{}\"", p.display())).collect::<Vec<_>>().join("\r\n");
        let note = match shell::clipboard_set_text(&text, self.hwnd()) {
            Ok(()) => format!("경로 {}개 복사함", paths.len()),
            Err(e) => format!("클립보드 실패: {e}"),
        };
        self.set_note(&t, note);
    }

    // ---------- file operations ----------

    fn selected_paths(&self, t: &Tab) -> Vec<PathBuf> {
        let mut sel = t.rows.selection();
        if sel.is_empty() {
            sel.extend(t.cursor.get());
        }
        sel.into_iter().map(|i| t.path_of(i)).collect()
    }

    fn copy(&self, cut: bool) {
        let t = self.tab();
        let paths = self.selected_paths(&t);
        if paths.is_empty() {
            return;
        }
        let note = match shell::clipboard_set(&paths, cut, self.hwnd()) {
            Ok(()) => format!("{}개 {}", paths.len(), if cut { "잘라냄" } else { "복사함" }),
            Err(e) => format!("클립보드 실패: {e}"),
        };
        self.set_note(&t, note);
    }

    fn paste(&self) {
        let t = self.tab();
        // ponytail: reads the clipboard on the UI thread; an app that renders its clipboard
        // data lazily could stall this. Move to a worker if that shows up.
        let Some((src, cut)) = shell::clipboard_get(self.hwnd()) else {
            return self.set_note(&t, "붙여넣을 파일이 없음".into());
        };
        let names = src.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
        let dst = t.cwd.borrow().clone();
        let op = if cut { Op::Move(src, dst) } else { Op::Copy(src, dst) };
        self.run_op(&t, After { select: names, ..Default::default() }, move |hwnd| {
            let r = shell::run(&op, Some(hwnd));
            if cut && matches!(r, Ok(true)) {
                shell::clipboard_clear(hwnd);
            }
            r
        });
    }

    fn delete(&self, permanent: bool) {
        let t = self.tab();
        let paths = self.selected_paths(&t);
        if !paths.is_empty() {
            // The shell asks for confirmation itself on a permanent delete, like Explorer.
            self.run_op(&t, After::default(), move |hwnd| shell::run(&Op::Delete { paths, permanent }, Some(hwnd)));
        }
    }

    fn start_rename(&self) {
        let t = self.tab();
        let Some(c) = t.cursor.get().or_else(|| t.rows.selection().first().copied()) else { return };
        t.rows.select_only(Some(c));
        t.rows.editing.set(Some(c));
        t.rows.notify.reset();
        self.reveal_cursor(&t);
    }

    fn rename_done(&self, text: &str, commit: bool) {
        let t = self.tab();
        let Some(idx) = t.rows.editing.take() else { return };
        t.rows.notify.reset();
        self.ui().invoke_focus_list();
        let (old, ext) = (t.rows.name(idx), t.rows.entries.borrow()[idx as usize].extension());
        let mut new = text.trim().to_string();
        // With extensions hidden the box held only the stem; keep the extension, like Explorer.
        if !new.is_empty() && !SHOW_EXT.load(Relaxed) && !t.rows.is_dir(idx) && !ext.is_empty() {
            new = format!("{new}.{ext}");
        }
        if commit && !new.is_empty() && new != old {
            let path = t.path_of(idx);
            let after = After { select: vec![new.clone()], ..Default::default() };
            self.run_op(&t, after, move |hwnd| shell::rename(&path, &new, Some(hwnd)));
        }
    }

    fn new_folder(&self) {
        self.new_item(shell::new_folder);
    }

    fn new_file(&self) {
        self.new_item(shell::new_text_file);
    }

    /// Creates the item on a worker, then selects it with its name ready to edit.
    fn new_item(&self, create: fn(&std::path::Path) -> io::Result<String>) {
        let t = self.tab();
        if t.search.borrow().is_some() {
            return self.set_note(&t, "검색 결과에서는 새로 만들 수 없음".into());
        }
        self.ui().invoke_focus_list();
        let (id, dir) = (t.id, t.cwd.borrow().clone());
        thread::spawn(move || {
            let r = create(&dir);
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                match r {
                    Ok(name) => a.reload(&t, After { select: vec![name], rename: true, ..Default::default() }),
                    Err(e) => a.set_note(&t, format!("만들 수 없음: {e}")),
                }
            });
        });
    }

    /// "숨긴 항목" / "파일 확장명" toggled: re-filter and redraw every tab.
    fn view_changed(&self) {
        let ui = self.ui();
        SHOW_HIDDEN.store(ui.get_show_hidden(), Relaxed);
        SHOW_EXT.store(ui.get_show_ext(), Relaxed);
        for t in self.tabs.borrow().iter() {
            self.resort(t);
        }
        self.sync_fit(&self.tab());
        ui.invoke_focus_list();
    }

    fn icon_loaded(&self, key: IconKey, px: Option<icons::Pixels>) {
        let img = px.map(|p| slint::Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&p.rgba, p.width, p.height)));
        let schedule = ICONS.with(|c| {
            let mut c = c.borrow_mut();
            c.pending.remove(&key);
            c.images.insert(key, img);
            !std::mem::replace(&mut c.refresh_due, true)
        });
        if schedule {
            // Icons arrive in bursts: redraw the visible rows once per burst.
            slint::Timer::single_shot(Duration::from_millis(30), || app(|a| a.refresh_visible()));
        }
    }

    fn refresh_visible(&self) {
        ICONS.with(|c| c.borrow_mut().refresh_due = false);
        let (ui, t) = (self.ui(), self.tab());
        let h = ui.get_row_height().max(1.0);
        let first = (-ui.get_list_y() / h).max(0.0) as usize;
        let last = (first + (ui.get_list_height() / h) as usize + 2).min(t.rows.len());
        for r in first..last {
            t.rows.notify.row_changed(r);
        }
        self.sync_places();
    }

    // ---------- drag and drop ----------

    /// The folder a drop at screen `(x, y)` lands in: a folder row, else the current folder.
    fn drop_target_at(&self, x: i32, y: i32) -> Option<(PathBuf, Option<usize>)> {
        let ui = self.ui();
        let scale = ui.window().scale_factor();
        // OLE reports screen pixels; the list's absolute-position is client-relative logical px.
        let mut pt = POINT { x, y };
        unsafe {
            let _ = ScreenToClient(HWND(self.hwnd() as _), &mut pt);
        }
        let (x, y) = (pt.x as f32 / scale, pt.y as f32 / scale);
        let (ox, oy) = (ui.get_list_origin_x(), ui.get_list_origin_y());
        if x < ox || y < oy || x > ox + ui.get_list_width() || y > oy + ui.get_list_height() {
            return None;
        }
        let t = self.tab();
        let row = ((y - oy - ui.get_list_y()) / ui.get_row_height().max(1.0)).floor();
        if row >= 0.0
            && let Some(i) = t.rows.entry(row as usize)
            && t.rows.is_dir(i)
        {
            return Some((t.path_of(i), Some(row as usize)));
        }
        let cwd = t.cwd.borrow().clone();
        (!cwd.as_os_str().is_empty()).then_some((cwd, None))
    }

    fn set_drop_row(&self, row: Option<usize>) {
        let row = row.map_or(-1, |r| r as i32);
        if self.drop_row.replace(row) != row {
            self.ui().set_drop_row(row);
        }
    }

    /// Runs a native drop on a worker (it can show progress/conflict dialogs), then refreshes.
    fn drop_files(&self, files: Vec<PathBuf>, target: PathBuf, move_: bool) {
        let t = self.tab();
        let id = t.id;
        let hwnd = self.hwnd();
        let op = if move_ { Op::Move(files, target) } else { Op::Copy(files, target) };
        thread::spawn(move || {
            let r = shell::run(&op, Some(hwnd));
            let note = match &r {
                Ok(true) => String::new(),
                Ok(false) => "취소됨".into(),
                Err(e) => format!("실패: {e}"),
            };
            post(move |a| {
                if let Some(t) = a.find(id) {
                    a.set_note(&t, note);
                    a.reload(&t, After::default());
                }
            });
        });
    }

    /// Runs a file operation on a worker (it may show dialogs), then refreshes the tab.
    /// `job` gets the owner window and returns `Ok(false)` when the user cancelled.
    fn run_op(&self, t: &Tab, after: After, job: impl FnOnce(isize) -> io::Result<bool> + Send + 'static) {
        let (id, hwnd) = (t.id, self.hwnd());
        thread::spawn(move || {
            let r = job(hwnd);
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                *t.note.borrow_mut() = match r {
                    Ok(true) => String::new(),
                    Ok(false) => "취소됨".into(),
                    Err(e) => format!("실패: {e}"),
                };
                a.reload(&t, after);
            });
        });
    }
}

fn shown(e: &Entry) -> bool {
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    SHOW_HIDDEN.load(Relaxed) || e.attrs & FILE_ATTRIBUTE_HIDDEN == 0
}

/// The name as displayed: extensions may be hidden, like Explorer (folders keep dots).
fn display_name(e: &Entry) -> String {
    let mut name = e.name();
    let ext = e.extension();
    if !SHOW_EXT.load(Relaxed) && !e.is_dir() && !ext.is_empty() {
        name.truncate(name.len() - ext.len() - 1);
    }
    name
}

fn kind_of(e: &Entry) -> String {
    match (e.is_dir(), e.extension()) {
        (true, _) => "파일 폴더".to_string(),
        (false, x) if x.is_empty() => "파일".to_string(),
        (false, x) => format!("{} 파일", x.to_uppercase()),
    }
}

/// Sorted display order, without hidden items unless "숨긴 항목" is on.
fn display_order(entries: &[Entry], key: SortKey, desc: bool) -> Vec<u32> {
    let mut order = sort_order(entries, key, desc);
    order.retain(|&i| shown(&entries[i as usize]));
    order
}

/// The widest visible text in each column; the UI measures it exactly when a divider is
/// double-clicked. Approximate ranking here only decides which sample to measure.
struct FitSamples {
    name: String,
    location: String,
    date: String,
    kind: String,
    size: String,
}

fn fit_samples(t: &Tab) -> FitSamples {
    let entries = t.rows.entries.borrow();
    let locations = t.rows.locations.borrow();
    let searching = t.search.borrow().is_some();
    let mut name = "이름".to_string();
    let mut location = "위치".to_string();
    let mut date = "수정한 날짜".to_string();
    let mut kind = "유형".to_string();
    let mut size = "크기".to_string();
    let (mut name_w, mut location_w) = (text_units(&name), text_units(&location));
    let (mut kind_w, size_w) = (text_units(&kind), text_units(&size));
    let mut has_date = false;
    let mut max_size = None;
    for (i, e) in entries.iter().enumerate() {
        if !shown(e) {
            continue;
        }
        let n = display_name(e);
        let w = text_units(&n);
        if w > name_w {
            name_w = w;
            name = n;
        }
        if searching
            && let Some(l) = locations.get(i)
        {
            let w = text_units(l);
            if w > location_w {
                location_w = w;
                location = l.clone();
            }
        }
        let k = kind_of(e);
        let w = text_units(&k);
        if w > kind_w {
            kind_w = w;
            kind = k;
        }
        has_date |= e.modified > 0;
        if !e.is_dir() {
            max_size = Some(max_size.map_or(e.size, |m: u64| m.max(e.size)));
        }
    }
    if has_date && text_units("0000-00-00 00:00") > text_units(&date) {
        date = "0000-00-00 00:00".into();
    }
    if let Some(bytes) = max_size {
        let s = format_size(bytes);
        if text_units(&s) > size_w {
            size = s;
        }
    }
    FitSamples { name, location, date, kind, size }
}

/// Rough advance ranking (1/10 em), used only to pick which sample the UI should measure.
fn text_units(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            ' ' | '\'' | '.' | ',' | ':' | ';' | '!' | '|' | '`' | 'i' | 'l' | 'j' | 't' | 'f' | 'r' | 'I' => 3,
            'm' | 'w' | 'M' | 'W' => 9,
            c if c.is_ascii_uppercase() => 7,
            c if c.is_ascii() => 6,
            _ => 10,
        })
        .sum()
}

/// Icons already loaded (None: the shell had none), and the ones on their way.
#[derive(Default)]
struct IconCache {
    images: HashMap<IconKey, Option<slint::Image>>,
    pending: HashSet<IconKey>,
    tx: Option<mpsc::Sender<IconKey>>,
    refresh_due: bool,
}

thread_local! {
    static ICONS: RefCell<IconCache> = RefCell::default();
}

/// The icon for `key` if loaded; otherwise asks the icon thread for it and returns a blank
/// (the row is redrawn when it arrives).
fn icon_image(key: IconKey) -> slint::Image {
    ICONS.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(img) = c.images.get(&key) {
            return img.clone().unwrap_or_default();
        }
        if c.pending.insert(key.clone())
            && let Some(tx) = &c.tx
        {
            let _ = tx.send(key);
        }
        slint::Image::default()
    })
}

/// Shell icons are read on their own thread: some need the file itself (exe, lnk), and
/// shell handlers may be slow. ponytail: one thread and an unbounded cache; add an LRU if
/// folders full of distinct .exe icons ever matter.
fn start_icon_worker() {
    let (tx, rx) = mpsc::channel::<IconKey>();
    thread::spawn(move || {
        icons::init_thread();
        for key in rx {
            let px = icons::load(&key);
            post(move |a| a.icon_loaded(key, px));
        }
    });
    ICONS.with(|c| c.borrow_mut().tx = Some(tx));
}

/// A search hit as a list entry, with size/date/attributes read from the file system.
fn hit_entry(h: &index::Hit) -> Entry {
    use std::os::windows::fs::MetadataExt;
    let name = h.path.file_name().unwrap_or(h.path.as_os_str());
    match std::fs::symlink_metadata(&h.path) {
        Ok(m) => Entry::new(name, m.file_size(), m.last_write_time() as i64, m.file_attributes()),
        Err(_) => Entry::new(name, 0, 0, if h.dir { 0x10 } else { 0 }), // gone since it was indexed
    }
}

fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

/// Compares paths case-insensitively, ignoring a trailing separator.
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(|c| c == '\\' || c == '/').to_lowercase();
    norm(a) == norm(b)
}

/// Folder buttons for the address bar, from the drive down to `path`.
fn crumbs_for(path: &Path) -> Vec<Crumb> {
    let mut ancestors: Vec<&Path> = path.ancestors().collect();
    ancestors.reverse();
    ancestors
        .into_iter()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| {
            let mut title = match p.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => p.display().to_string(),
            };
            // "C:\" -> "C:", "\\server\share\" -> "\\server\share"
            if let Some(root) = title.strip_suffix('\\') {
                title = root.to_string();
            }
            Crumb { title: title.into(), path: p.display().to_string().into() }
        })
        .collect()
}

fn favorites_file() -> Option<PathBuf> {
    shell::roaming_appdata().ok().map(|d| d.join("Fast Explorer").join("favorites.txt"))
}

fn load_favorites(desktop: Option<&Path>, downloads: Option<&Path>) -> Vec<PathBuf> {
    if let Some(file) = favorites_file()
        && let Ok(text) = std::fs::read_to_string(&file)
    {
        return text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    }
    [desktop, downloads].into_iter().flatten().map(Path::to_path_buf).collect()
}

fn save_favorites(favorites: &[PathBuf]) {
    let Some(file) = favorites_file() else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = favorites.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\r\n");
    let _ = std::fs::write(file, text);
}

/// Maximized state of the last DWM frame update.
static FRAME_MAXIMIZED: AtomicBool = AtomicBool::new(false);

/// A no-frame window loses the DWM border and drop shadow. Extending the frame by one
/// pixel restores both without covering the client area; a maximized window must drop
/// the extended frame or a strip of desktop shows along the screen edges.
fn apply_window_frame(hwnd: HWND) {
    unsafe {
        // Slint draws the visible 1px frame; keep the native corners square so it is
        // not clipped at the corners on Windows 11.
        let preference = DWMWCP_DONOTROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const i32 as *const _,
            size_of::<i32>() as u32,
        );
        let maximized = IsZoomed(hwnd).as_bool();
        FRAME_MAXIMIZED.store(maximized, Relaxed);
        let margins = if maximized {
            MARGINS::default()
        } else {
            MARGINS { cxLeftWidth: 1, cxRightWidth: 1, cyTopHeight: 1, cyBottomHeight: 1 }
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

/// The mouse's back/forward buttons reach the window before any widget sees them.
fn install_history_buttons(hwnd: isize) {
    unsafe {
        let _ = SetWindowSubclass(HWND(hwnd as _), Some(history_subclass), 1, 0);
    }
}

unsafe extern "system" fn history_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_NCHITTEST
        && let Some(code) = resize_hit(hwnd, lparam)
    {
        return LRESULT(code as isize);
    }
    if msg == WM_SIZE && FRAME_MAXIMIZED.load(Relaxed) != unsafe { IsZoomed(hwnd) }.as_bool() {
        apply_window_frame(hwnd);
    }
    if msg == WM_XBUTTONDOWN {
        let button = ((wparam.0 >> 16) & 0xffff) as u16;
        if button == XBUTTON1 || button == XBUTTON2 {
            let back = button == XBUTTON1;
            post(move |a| if back { a.back() } else { a.forward() });
            return LRESULT(0);
        }
    }
    if msg == WM_XBUTTONUP {
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// A frameless window has no resize border; give the outer few pixels back to the OS.
fn resize_hit(hwnd: HWND, lparam: LPARAM) -> Option<u32> {
    if unsafe { IsZoomed(hwnd) }.as_bool() {
        return None;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    let x = (lparam.0 as u32 & 0xffff) as u16 as i16 as i32;
    let y = ((lparam.0 as u32 >> 16) & 0xffff) as u16 as i16 as i32;
    const B: i32 = 6;
    let (left, right) = (x < rect.left + B, x >= rect.right - B);
    let (top, bottom) = (y < rect.top + B, y >= rect.bottom - B);
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(HTTOPLEFT),
        (_, true, true, _) => Some(HTTOPRIGHT),
        (true, _, _, true) => Some(HTBOTTOMLEFT),
        (_, true, _, true) => Some(HTBOTTOMRIGHT),
        (true, _, _, _) => Some(HTLEFT),
        (_, true, _, _) => Some(HTRIGHT),
        (_, _, true, _) => Some(HTTOP),
        (_, _, _, true) => Some(HTBOTTOM),
        _ => None,
    }
}

/// Shrinks and centers the frameless window when its preferred size doesn't fit the monitor.
fn fit_window_to_screen(ui: &AppWindow, hwnd: HWND) {
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return;
    }
    let work = info.rcWork;
    let (avail_w, avail_h) = (work.right - work.left, work.bottom - work.top);
    let window = ui.window();
    let size = window.size();
    let (max_w, max_h) = ((avail_w - 24).max(1), (avail_h - 24).max(1));
    if size.width as i32 <= max_w && size.height as i32 <= max_h {
        return;
    }
    let w = (size.width as i32).min(max_w).max(1);
    let h = (size.height as i32).min(max_h).max(1);
    window.set_size(slint::PhysicalSize::new(w as u32, h as u32));
    window.set_position(slint::PhysicalPosition::new(
        work.left + (avail_w - w) / 2,
        work.top + (avail_h - h) / 2,
    ));
}

/// Registers the OLE drop target and XBUTTON handling once the native window exists.
fn install_native_hooks(weak: slint::Weak<AppWindow>, tries: u32) {
    slint::Timer::single_shot(Duration::from_millis(if tries == 0 { 0 } else { 50 }), move || {
        let Some(ui) = weak.upgrade() else { return };
        let hwnd = app(|a| a.hwnd());
        if hwnd != 0 {
            fit_window_to_screen(&ui, HWND(hwnd as _));
            apply_window_frame(HWND(hwnd as _));
            install_history_buttons(hwnd);
            dnd::install(hwnd, dnd::Hooks { target_at: dnd_target_at, dropped: dnd_dropped, hover: dnd_hover });
        } else if tries < 100 {
            install_native_hooks(weak.clone(), tries + 1);
        }
    });
}

fn dnd_target_at(x: i32, y: i32) -> Option<(PathBuf, Option<usize>)> {
    app(|a| a.drop_target_at(x, y))
}

fn dnd_dropped(files: Vec<PathBuf>, target: PathBuf, move_: bool) {
    app(|a| a.drop_files(files, target, move_));
}

fn dnd_hover(row: Option<usize>) {
    app(|a| a.set_drop_row(row));
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let tab_model = Rc::new(VecModel::default());
    ui.set_tabs(ModelRc::from(tab_model.clone()));

    let exe = std::env::current_exe().unwrap_or_default().with_file_name("fex-shell-host.exe");
    let host = Arc::new(MenuHost::new(exe, |_, r| {
        if let Err(e) = r {
            post(move |a| a.set_note(&a.tab(), format!("실행 실패: {e}")));
        }
    }));
    host.warm_up(); // loading shell extensions takes seconds the first time; do it now

    let (show_hidden, show_ext) = shell::explorer_view();
    SHOW_HIDDEN.store(show_hidden, Relaxed);
    SHOW_EXT.store(show_ext, Relaxed);
    ui.set_show_hidden(show_hidden);
    ui.set_show_ext(show_ext);
    start_icon_worker();

    let desktop = shell::desktop().ok();
    let downloads = shell::downloads().ok();
    let favorites = load_favorites(desktop.as_deref(), downloads.as_deref());
    let drives = shell::drives();

    APP.with(|a| {
        let _ = a.set(App {
            ui: ui.as_weak(),
            tabs: RefCell::default(),
            tab_model,
            active: Cell::new(0),
            next_id: Cell::new(0),
            typeahead: RefCell::default(),
            host,
            menu_seq: Cell::new(0),
            prefetched: RefCell::default(),
            menu_busy: Cell::new(false),
            search_seq: Arc::default(),
            install_offered: Cell::new(false),
            favorites: RefCell::new(favorites),
            desktop,
            downloads,
            drives,
            drag_armed: Cell::new(false),
            pending_collapse: Cell::new(None),
            rubber_base: RefCell::default(),
            rubber_last: Cell::new((usize::MAX, 0)),
            drop_row: Cell::new(-1),
        });
    });

    // Hiding a window alone leaves the process running; a close request quits the app.
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

    // The native window is realized once the event loop runs; retry until it has an HWND.
    install_native_hooks(ui.as_weak(), 0);

    ui.on_navigate(|t| app(|a| a.address(&t)));
    ui.on_search(|t| app(|a| a.search(&t)));
    ui.on_back(|| app(|a| a.back()));
    ui.on_forward(|| app(|a| a.forward()));
    ui.on_up(|| app(|a| a.up()));
    ui.on_refresh(|| app(|a| a.refresh()));
    ui.on_tab_select(|i| app(|a| a.select_tab(i as usize, true)));
    ui.on_tab_close(|i| app(|a| a.close_tab(i as usize)));
    ui.on_tab_new(|| app(|a| a.new_tab(a.tab().cwd.borrow().clone())));
    ui.on_sort(|c| app(|a| a.sort(c as usize)));
    ui.on_row_pressed(|r, m| app(|a| a.row_pressed(r as usize, m.control, m.shift)));
    ui.on_row_clicked(|r| app(|a| a.row_clicked(r as usize)));
    ui.on_row_middle(|r| app(|a| a.row_middle(r as usize)));
    ui.on_row_drag(|r| app(|a| a.row_drag(r as usize)));
    ui.on_row_open(|r| {
        app(|a| {
            let t = a.tab();
            if let Some(i) = t.rows.entry(r as usize) {
                a.open_rows(&t, vec![i]);
            }
        })
    });
    ui.on_rename_done(|text, commit| app(|a| a.rename_done(&text, commit)));
    ui.on_row_menu(|r| app(|a| a.row_menu(r as usize)));
    ui.on_row_hover(|r| app(|a| a.row_hover(r as usize)));
    ui.on_blank_pressed(|right, ctrl| app(|a| a.blank_pressed(right, ctrl)));
    ui.on_place_open(|p| app(|a| a.open_place(&p)));
    ui.on_place_add(|| app(|a| a.add_favorite()));
    ui.on_place_remove(|p| app(|a| a.remove_favorite(&p)));
    ui.on_history(|d| app(|a| if d < 0 { a.back() } else { a.forward() }));
    ui.on_rubber_start(|ctrl| app(|a| a.rubber_start(ctrl)));
    ui.on_rubber_move(|y1, y2| app(|a| a.rubber_move(y1, y2)));
    ui.on_rubber_end(|| app(|a| a.rubber_end()));
    ui.on_window_minimize(|| app(|a| a.minimize_window()));
    ui.on_window_toggle_maximize(|| app(|a| a.toggle_maximize()));
    ui.on_window_close(|| app(|a| a.close_window()));
    ui.on_new_folder(|| app(|a| a.new_folder()));
    ui.on_new_file(|| app(|a| a.new_file()));
    ui.on_delete_selected(|| app(|a| {
        a.ui().invoke_focus_list();
        a.delete(false);
    }));
    ui.on_view_changed(|| app(|a| a.view_changed()));
    ui.on_key(|e, in_list| {
        app(|a| a.key(&e.text, e.modifiers.control, e.modifiers.shift, e.modifiers.alt, in_list))
    });

    let start = std::env::args().nth(1).unwrap_or_else(|| r"C:\".into());
    app(|a| a.new_tab(PathBuf::from(start)));
    ui.run()
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_prefix() {
        assert_eq!(super::content_query("내용:보고서 초안").as_deref(), Some("보고서 초안"));
        assert_eq!(super::content_query("Content: TODO ").as_deref(), Some("TODO"));
        assert_eq!(super::content_query("내용:"), None); // nothing to look for yet
        assert_eq!(super::content_query("notepad"), None);
    }

    #[test]
    fn crumbs_from_the_drive_root() {
        let crumbs = super::crumbs_for(std::path::Path::new(r"C:\Users\hp\source"));
        let titles: Vec<String> = crumbs.iter().map(|c| c.title.to_string()).collect();
        let paths: Vec<String> = crumbs.iter().map(|c| c.path.to_string()).collect();
        assert_eq!(titles, ["C:", "Users", "hp", "source"]);
        assert_eq!(paths, [r"C:\", r"C:\Users", r"C:\Users\hp", r"C:\Users\hp\source"]);
    }
}
