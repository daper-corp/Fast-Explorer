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

use fex_core::icons::{self, IconKey};
use fex_core::index::{self, QueryError, Results};
use fex_core::menu::{HANG_LIMIT, MenuHost, MenuItem, MenuResult, Request};
use fex_core::search::{self, Pattern, Scope};
use fex_core::shell::{self, Op};
use fex_core::t;
use fex_core::watch;
use fex_core::{Entry, SortKey, format_filetime, format_size, list_dir, sort_order};
use popup::{Own, SEP, own};
use tree::{FlatTree, RowMeta, SearchTree, file_entry};
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
use windows::Win32::UI::Shell::{
    DefSubclassProc, SetWindowSubclass, SIID_DELETE, SIID_DOCNOASSOC, SIID_FOLDER, SIID_RENAME,
    SIID_SETTINGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, GetClientRect, GetCursorPos, GetWindowRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, PostMessageW, WM_CLOSE, WM_DEVICECHANGE, WM_NCHITTEST, WM_SIZE,
    WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

mod dnd;
mod popup;
mod tools;
mod tree;
mod update;

/// Gives one of our menu commands an icon.
fn with_icon(o: Own, icon: popup::Icon) -> Own {
    Own { icon: Some(icon), ..o }
}

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
const CMD_SEARCH_HERE: u32 = 0x8010;
const CMD_RENAME_MANY: u32 = 0x8011;
const CMD_LOCKS: u32 = 0x8012;
const CMD_EJECT: u32 = 0x8013;
const CMD_FORMAT: u32 = 0x8014;

/// How long a right-click waits for a shell menu that isn't prefetched yet.
const MENU_WAIT: Duration = Duration::from_millis(150);

/// Most search hits shown (the status bar says how many matched in all).
const SEARCH_LIMIT: usize = 2000;

/// The shell namespace path of "This PC". It cannot collide with a file-system path.
const THIS_PC: &str = r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
/// FILE_ATTRIBUTE_DIRECTORY, for the synthetic rows of the "This PC" view.
const ATTR_DIR: u32 = 0x10;

fn is_this_pc(path: &Path) -> bool {
    path.to_string_lossy().eq_ignore_ascii_case(THIS_PC)
}

/// The tab is showing the virtual "This PC" rows (not search results opened from it).
fn at_this_pc(t: &Tab) -> bool {
    t.search.borrow().is_none() && is_this_pc(&t.cwd.borrow())
}

/// "this pc" (any case), the localized name and the Korean "내 PC"/"내PC" are all accepted in the address bar.
fn is_this_pc_name(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    t == "내 pc" || t == "내pc" || t == "this pc" || t == "thispc" || t == THIS_PC.to_lowercase() || t == t!("This PC").to_lowercase()
}

/// Drives and user folders shown by the "This PC" view, built off the UI thread.
#[derive(Clone)]
struct ThisPcRow {
    title: String,
    path: PathBuf,
    kind: String,
    /// "Free space" column.
    date: String,
    /// "Total capacity" column.
    size: String,
    usage: f32,
    free: u64,
    total: u64,
}

/// The index service understands `re:` but not our UI's regex toggle, so mark the query.
fn service_pattern(pattern: &str, regex: bool) -> String {
    if !regex {
        return pattern.to_string();
    }
    let source = match pattern.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("re:")) {
        true => &pattern[3..],
        false => pattern,
    };
    format!("re:{source}")
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
    /// Search-tree display facts (empty when listing a folder or showing "This PC").
    meta: RefCell<Vec<RowMeta>>,
    /// The "Tree view" folder tree over `entries`; dropped whenever the entries are replaced.
    browse: RefCell<Option<SearchTree>>,
    /// Per-entry overrides used by the virtual "This PC" view. They stay empty for real folders,
    /// so row_data falls back to the entry itself.
    paths: RefCell<Vec<String>>,
    kinds: RefCell<Vec<String>>,
    dates: RefCell<Vec<String>>,
    sizes: RefCell<Vec<String>>,
    usage: RefCell<Vec<f32>>,
    /// The folder being listed (for icons that come from the file itself).
    base: RefCell<PathBuf>,
    notify: ModelNotify,
}

/// View options shared by every tab (initially Explorer's own settings).
static SHOW_HIDDEN: AtomicBool = AtomicBool::new(false);
static SHOW_EXT: AtomicBool = AtomicBool::new(true);
/// Folders list as an expandable tree (off by default).
static TREE_VIEW: AtomicBool = AtomicBool::new(false);

// Paths on the clipboard after Ctrl+X. Explorer shows them translucent until pasted.
thread_local! {
    static CUT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Case-insensitive identity for the cut set (Windows paths, trailing separator ignored).
fn path_key(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches(|c| c == '\\' || c == '/').to_lowercase()
}

fn is_cut(p: &Path) -> bool {
    CUT.with(|c| c.borrow().contains(&path_key(p)))
}

fn set_cut(paths: &[PathBuf]) {
    CUT.with(|c| {
        let mut c = c.borrow_mut();
        c.clear();
        c.extend(paths.iter().map(|p| path_key(p)));
    });
}

fn clear_cut() {
    CUT.with(|c| c.borrow_mut().clear());
}

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
        let virtual_path = self.paths.borrow().get(i as usize).cloned();
        let path = match virtual_path {
            Some(path) => PathBuf::from(path),
            None => match self.locations.borrow().get(i as usize) {
                Some(dir) => PathBuf::from(dir).join(e.os_name()),
                None => self.base.borrow().join(e.os_name()),
            },
        };
        let cut = is_cut(&path);
        let icon = match self.paths.borrow().get(i as usize) {
            Some(_) => icon_image(IconKey::File(path.clone())),
            None => icon_image(IconKey::for_entry(e.is_dir(), &ext, || path.clone())),
        };
        let date = match self.dates.borrow().get(i as usize).filter(|s| !s.is_empty()) {
            Some(s) => s.clone(),
            None => format_filetime(e.modified),
        };
        let kind = match self.kinds.borrow().get(i as usize).filter(|s| !s.is_empty()) {
            Some(s) => s.clone(),
            None => kind_of(e),
        };
        let size = match self.sizes.borrow().get(i as usize).filter(|s| !s.is_empty()) {
            Some(s) => s.clone(),
            None if e.is_dir() => String::new(),
            None => format_size(e.size),
        };
        let meta = self.meta.borrow().get(i as usize).copied().unwrap_or_default();
        Some(Row {
            date: date.into(),
            kind: kind.into(),
            size: size.into(),
            selected: self.selected.borrow()[i as usize],
            editing: self.editing.get() == Some(i),
            cut,
            stem: stem as i32,
            name: name.into(),
            location: self.locations.borrow().get(i as usize).map(|s| s.as_str().into()).unwrap_or_default(),
            depth: meta.depth as i32,
            expandable: meta.expandable,
            expanded: meta.expanded,
            icon,
            usage: self.usage.borrow().get(i as usize).copied().unwrap_or(-1.0),
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
        self.meta.borrow_mut().clear();
        self.browse.take();
        self.paths.borrow_mut().clear();
        self.kinds.borrow_mut().clear();
        self.dates.borrow_mut().clear();
        self.sizes.borrow_mut().clear();
        self.usage.borrow_mut().clear();
        self.editing.set(None);
        self.notify.reset();
    }

    /// Shows a search tree: entries are tree nodes and `flat` is the expanded part.
    fn replace_search(&self, entries: Vec<Entry>, flat: FlatTree) {
        *self.selected.borrow_mut() = vec![false; entries.len()];
        *self.entries.borrow_mut() = Arc::new(entries);
        self.browse.take();
        self.paths.borrow_mut().clear();
        self.kinds.borrow_mut().clear();
        self.dates.borrow_mut().clear();
        self.sizes.borrow_mut().clear();
        self.usage.borrow_mut().clear();
        self.editing.set(None);
        self.apply_flat(flat);
    }

    /// Updates the visible part of an already-loaded search tree, keeping its entries.
    fn update_search(&self, flat: FlatTree) {
        self.selected.borrow_mut().resize(self.entries.borrow().len(), false);
        self.apply_flat(flat);
    }

    fn apply_flat(&self, flat: FlatTree) {
        *self.order.borrow_mut() = flat.order;
        *self.meta.borrow_mut() = flat.metas;
        *self.locations.borrow_mut() = flat.locations;
        self.notify.reset();
    }

    /// Replaces the model with the synthetic "This PC" rows and their per-row columns.
    fn replace_virtual(
        &self,
        entries: Vec<Entry>,
        order: Vec<u32>,
        paths: Vec<String>,
        kinds: Vec<String>,
        dates: Vec<String>,
        sizes: Vec<String>,
        usage: Vec<f32>,
    ) {
        *self.selected.borrow_mut() = vec![false; entries.len()];
        *self.entries.borrow_mut() = Arc::new(entries);
        *self.order.borrow_mut() = order;
        *self.paths.borrow_mut() = paths;
        *self.kinds.borrow_mut() = kinds;
        *self.dates.borrow_mut() = dates;
        *self.sizes.borrow_mut() = sizes;
        *self.usage.borrow_mut() = usage;
        self.locations.borrow_mut().clear();
        self.meta.borrow_mut().clear();
        self.browse.take();
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

/// Records a visit in the tab's history (both real folders and the virtual "This PC").
fn record_history(t: &Tab, path: &Path, hist: Hist) {
    let mut h = t.history.borrow_mut();
    match hist {
        Hist::Go(i) => t.hist_pos.set(i),
        Hist::Push if h.is_empty() => h.push(path.to_path_buf()),
        Hist::Push if h[t.hist_pos.get()] != path => {
            h.truncate(t.hist_pos.get() + 1);
            h.push(path.to_path_buf());
            t.hist_pos.set(h.len() - 1);
        }
        Hist::Push => {}
    }
}

/// Type-ahead prefix: the lowercased text used for matching and the exact text typed.
#[derive(Default)]
struct TypeAhead {
    lower: String,
    typed: String,
    at: Option<Instant>,
}

/// One file operation that Ctrl+Z can invert, newest last.
enum Undo {
    Create(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
    /// `(original path, current path)` after a cut/paste or drag move.
    Move { pairs: Vec<(PathBuf, PathBuf)> },
    Copy { created: Vec<PathBuf> },
    Delete { items: Vec<shell::TrashedItem> },
    /// `(old path, new path)` for each item of a batch rename.
    RenameMany { pairs: Vec<(PathBuf, PathBuf)> },
}

/// Applied once the next listing of a tab is ready.
#[derive(Default)]
struct After {
    select: Vec<String>,
    rename: bool,
    /// The listing came from the folder watcher, not from the user.
    auto: bool,
}

/// When `target` is an ancestor of `from`, selects the child of `target` we are coming from.
fn came_from(from: &Path, target: &Path) -> Option<After> {
    let name = from.strip_prefix(target).ok()?.components().next()?;
    Some(After { select: vec![name.as_os_str().to_string_lossy().into_owned()], ..Default::default() })
}

/// How search results are matched and shown for one tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SearchOptions {
    folders: bool,
    files: bool,
    regex: bool,
    tree: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions { folders: true, files: true, regex: false, tree: false }
    }
}

/// The active search: what was asked, where it looked, and its folder tree.
struct Search {
    query: String,
    scope: Scope,
    tree: SearchTree,
    options: SearchOptions,
}

impl Search {
    fn flatten(&self, entries: &[Entry], key: SortKey, desc: bool) -> FlatTree {
        self.tree.flatten(
            entries,
            key,
            desc,
            SHOW_HIDDEN.load(Relaxed),
            self.options.folders,
            self.options.files,
            self.options.tree,
        )
    }
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
    /// Showing search results instead of the folder.
    search: RefCell<Option<Search>>,
    /// Folder/file, regex and result-view choices for this tab.
    search_options: Cell<SearchOptions>,
    /// Matching items in the current search (hidden ones excluded).
    hit_count: Cell<usize>,
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
            search_options: Cell::default(),
            hit_count: Cell::new(0),
            watch: RefCell::new(watch::DirWatch::new(move || post(move |a| a.folder_changed(id))).ok()),
            refresh_due: Cell::new(false),
        }
    }

    /// Exact path (from the raw UTF-16 name, not the lossy display string).
    fn path_of(&self, idx: u32) -> PathBuf {
        if let Some(path) = self.rows.paths.borrow().get(idx as usize) {
            return PathBuf::from(path);
        }
        if let Some(search) = &*self.search.borrow()
            && let Some(path) = search.tree.path(idx)
        {
            return path.to_path_buf();
        }
        if let Some(path) = self.rows.browse.borrow().as_ref().and_then(|b| b.path(idx)) {
            return path.to_path_buf();
        }
        self.cwd.borrow().join(self.rows.entries.borrow()[idx as usize].os_name())
    }

    fn query(&self) -> Option<String> {
        self.search.borrow().as_ref().map(|s| s.query.clone())
    }

    fn title(&self) -> String {
        let cwd = self.cwd.borrow();
        if is_this_pc(&cwd) {
            return t!("This PC").into();
        }
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
    /// Folders of recently closed tabs, newest last (Ctrl+Shift+T).
    closed: RefCell<Vec<PathBuf>>,
    tab_model: Rc<VecModel<TabInfo>>,
    active: Cell<usize>,
    next_id: Cell<u64>,
    typeahead: RefCell<TypeAhead>,
    /// Bumped per type-ahead hint so an older hide timer does not clear a newer hint.
    hint_seq: Cell<u64>,
    undo: RefCell<Vec<Undo>>,
    /// Shell context menus come from the separate fex-shell-host process.
    host: Arc<MenuHost>,
    menu_seq: Cell<u64>,
    /// The menu last requested from the host: (background?, paths, seq).
    prefetched: RefCell<Option<(bool, Vec<PathBuf>, u64)>>,
    /// Waiting for "More options"; no other query may supersede it meanwhile.
    menu_busy: Cell<bool>,
    /// The newest search; workers for older ones stop and their results are dropped.
    search_seq: Arc<AtomicU64>,
    install_offered: Cell<bool>,
    /// Sidebar: user folders, the default locations, and the mounted drives.
    favorites: RefCell<Vec<PathBuf>>,
    desktop: Option<PathBuf>,
    downloads: Option<PathBuf>,
    drives: RefCell<Vec<shell::Drive>>,
    /// A WM_DEVICECHANGE refresh is already scheduled.
    devices_due: Cell<bool>,
    /// The newest address-bar suggestion request; older workers are dropped.
    suggest_seq: Arc<AtomicU64>,
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

    /// Wheel-click: open in a new tab but stay on the current one.
    fn new_tab_bg(&self, path: PathBuf) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let tab = Rc::new(Tab::new(id));
        self.tabs.borrow_mut().push(tab.clone());
        self.sync_tabs();
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
        {
            let mut recent = self.closed.borrow_mut();
            recent.push(closed.cwd.borrow().clone());
            let extra = recent.len().saturating_sub(20);
            recent.drain(..extra);
        }
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

    /// Drag-and-drop tab reorder: moves the tab at `from` to index `to`.
    fn move_tab(&self, from: usize, to: usize) {
        let n = self.tabs.borrow().len();
        if from >= n || to >= n || from == to {
            return;
        }
        {
            let mut tabs = self.tabs.borrow_mut();
            let tab = tabs.remove(from);
            tabs.insert(to, tab);
        }
        // The active tab keeps its rows model; only its index in the strip changes.
        let active = self.active.get();
        let active = if active == from {
            to
        } else if from < active && active <= to {
            active - 1
        } else if to <= active && active < from {
            active + 1
        } else {
            active
        };
        self.active.set(active);
        self.ui().set_active_tab(active as i32);
        self.sync_tabs();
    }

    fn cycle_tab(&self, back: bool) {
        let n = self.tabs.borrow().len();
        let i = self.active.get();
        self.select_tab(if back { (i + n - 1) % n } else { (i + 1) % n }, true);
    }

    fn sync_tabs(&self) {
        let titles: Vec<TabInfo> =
            self.tabs.borrow().iter().map(|t| TabInfo { title: t.title().into() }).collect();
        // Update in place when the count is unchanged: replacing the model would rebuild the
        // tab elements and drop the pointer grab of a tab drag in progress.
        if titles.len() == self.tab_model.row_count() {
            for (i, t) in titles.into_iter().enumerate() {
                if self.tab_model.row_data(i).is_none_or(|o| o.title != t.title) {
                    self.tab_model.set_row_data(i, t);
                }
            }
        } else {
            self.tab_model.set_vec(titles);
        }
    }

    /// Pushes the active tab's path, history, sort and status to the window.
    fn sync_view(&self, t: &Tab) {
        if !self.is_active(t) {
            return self.sync_tabs();
        }
        let ui = self.ui();
        let cwd = t.cwd.borrow().clone();
        let this_pc = is_this_pc(&cwd);
        ui.set_this_pc(this_pc);
        ui.set_path(if this_pc { t!("This PC") } else { cwd.display().to_string() }.into());
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
        let options = t.search_options.get();
        ui.set_search_folders(options.folders);
        ui.set_search_files(options.files);
        ui.set_search_regex(options.regex);
        ui.set_search_view(if options.tree { 1 } else { 0 });
        self.sync_tabs();
        self.sync_places();
        self.update_status(t);
    }

    // ---------- sidebar ----------

    /// Rebuilds the sidebar models; cheap, and it picks up icons as they load.
    fn sync_places(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let cwd = self.tab().cwd.borrow().clone();
        ui.set_favorite_active(self.favorites.borrow().iter().any(|p| same_path(p, &cwd)));
        let favorites: Vec<Place> = self.favorites.borrow().iter().map(|p| self.place(p, &cwd, true)).collect();
        let drives: Vec<Place> =
            self.drives.borrow().iter().map(|d| self.place_drive(d, &cwd)).collect();
        ui.set_favorites(ModelRc::from(Rc::new(VecModel::from(favorites))));
        ui.set_drives(ModelRc::from(Rc::new(VecModel::from(drives))));
    }

    fn place(&self, path: &Path, cwd: &Path, removable: bool) -> Place {
        let title = match () {
            _ if self.desktop.as_deref().is_some_and(|d| same_path(d, path)) => t!("Desktop").into(),
            _ if self.downloads.as_deref().is_some_and(|d| same_path(d, path)) => t!("Downloads").into(),
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

    /// Sidebar middle-click: open the place in its own tab.
    fn open_place_in_tab(&self, path: &str) {
        self.new_tab_bg(PathBuf::from(path));
    }

    fn add_favorite(&self) {
        let cwd = self.tab().cwd.borrow().clone();
        self.add_favorite_path(cwd);
    }

    /// Toolbar star: add the folder shown, or remove it if it is already a favorite.
    fn toggle_favorite(&self) {
        let cwd = self.tab().cwd.borrow().clone();
        if cwd.as_os_str().is_empty() || is_this_pc(&cwd) {
            return;
        }
        let mut favorites = self.favorites.borrow_mut();
        if let Some(i) = favorites.iter().position(|p| same_path(p, &cwd)) {
            favorites.remove(i);
            save_favorites(&favorites);
            drop(favorites);
            self.sync_places();
            self.set_note(&self.tab(), t!("Removed from favorites"));
        } else {
            favorites.push(cwd);
            save_favorites(&favorites);
            drop(favorites);
            self.sync_places();
            self.set_note(&self.tab(), t!("Added to favorites"));
        }
    }

    fn add_favorite_path(&self, path: PathBuf) {
        let t = self.tab();
        if path.as_os_str().is_empty() || is_this_pc(&path) {
            return;
        }
        {
            let mut favorites = self.favorites.borrow_mut();
            if favorites.iter().any(|p| same_path(p, &path)) {
                drop(favorites);
                return self.set_note(&t, t!("Already in favorites"));
            }
            favorites.push(path);
            save_favorites(&favorites);
        }
        self.sync_places();
        self.set_note(&t, t!("Added to favorites"));
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
        // In search mode the list contains folder rows too; count the matching items.
        let search = t.search.borrow().is_some();
        let n = match search {
            true => t.hit_count.get(),
            false => t.rows.len(),
        };
        let sel = t.rows.selected.borrow().iter().filter(|&&s| s).count();
        let mut s = match search {
            true => t!("{} matches", n),
            false => t!("{} items", n),
        };
        if sel > 0 {
            s += &t!(" · {} selected", sel);
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

    /// A failed file operation: the status note plus a message box, so it cannot go unnoticed.
    fn show_error(&self, t: &Tab, what: &str, e: &io::Error) {
        // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION: another process holds the file.
        let busy = matches!(e.raw_os_error(), Some(32 | 33));
        let mut text = format!("{what}\n\n{e}");
        if busy {
            text.push_str(&t!("\n\nAnother process may be using the file or folder. Close that program and try again."));
        }
        self.alert(text);
        self.set_note(t, t!("Failed: {}", e));
    }

    fn alert(&self, text: String) {
        let hwnd = self.hwnd();
        thread::spawn(move || shell::alert(hwnd, &text));
    }

    fn set_note(&self, t: &Tab, note: String) {
        *t.note.borrow_mut() = note;
        self.update_status(t);
    }

    // ---------- address-bar autocomplete ----------

    /// Asks a worker for path suggestions; the newest request wins.
    fn suggest(&self, text: &str) {
        let seq = self.suggest_seq.fetch_add(1, SeqCst) + 1;
        let latest = self.suggest_seq.clone();
        let cwd = self.tab().cwd.borrow().clone();
        // From "This PC" a bare relative name is not meaningful; suggest from the system drive.
        let cwd = if is_this_pc(&cwd) { PathBuf::from(r"C:\") } else { cwd };
        let text = text.to_string();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(120)); // let typing settle
            if latest.load(SeqCst) != seq {
                return;
            }
            let items = suggest_paths(&text, &cwd);
            if latest.load(SeqCst) != seq {
                return;
            }
            post(move |a| a.suggestions_ready(seq, items));
        });
    }

    fn suggestions_ready(&self, seq: u64, items: Vec<Suggest>) {
        if self.suggest_seq.load(SeqCst) != seq {
            return;
        }
        let ui = self.ui();
        ui.set_suggest_index(-1);
        ui.set_suggestions(ModelRc::from(Rc::new(VecModel::from(items))));
    }

    fn cancel_suggest(&self) {
        self.suggest_seq.fetch_add(1, SeqCst);
        let ui = self.ui();
        ui.set_suggestions(ModelRc::from(Rc::new(VecModel::from(Vec::<Suggest>::new()))));
        ui.set_suggest_index(-1);
    }

    // ---------- loading ----------

    /// Lists `path` on a worker and streams it in. The old listing stays until the first batch
    /// arrives, so an unreadable folder (or a path that is now a file) leaves the tab where it was.
    /// Never opens files: only an explicit address-bar entry does that.
    fn navigate(&self, t: &Rc<Tab>, path: PathBuf, hist: Hist, after: Option<After>) {
        if is_this_pc(&path) {
            return self.open_this_pc(t, hist, after);
        }
        let epoch = t.epoch.fetch_add(1, SeqCst) + 1;
        t.refresh_due.set(false);
        *t.pending.borrow_mut() = Some((path.clone(), hist, after));
        self.set_note(t, t!("Loading…"));
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
        record_history(t, &path, hist);
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

    /// Shows the virtual "This PC" view: user folders and mounted drives with their free space.
    fn open_this_pc(&self, t: &Rc<Tab>, hist: Hist, after: Option<After>) {
        t.pending.take();
        t.search.take();
        self.search_seq.fetch_add(1, SeqCst);
        t.bump_sort();
        t.rows.replace(Vec::new(), Vec::new());
        t.cursor.set(None);
        t.anchor.set(None);
        *t.after.borrow_mut() = after;
        let path = PathBuf::from(THIS_PC);
        record_history(t, &path, hist);
        // The folder watcher has nothing to watch here; point it away so the previous
        // folder's events don't refresh this view.
        if let Some(w) = t.watch.borrow().as_ref() {
            w.watch(path.clone());
        }
        *t.rows.base.borrow_mut() = PathBuf::new();
        *t.cwd.borrow_mut() = path;
        if self.is_active(t) {
            self.ui().set_list_y(0.0);
        } else {
            t.scroll.set(0.0);
        }
        self.sync_view(t);
        self.load_this_pc(t);
    }

    fn load_this_pc(&self, t: &Rc<Tab>) {
        if t.query().is_some() {
            return; // search results are showing instead
        }
        let epoch = t.epoch.fetch_add(1, SeqCst) + 1;
        t.refresh_due.set(false);
        let id = t.id;
        self.set_note(t, t!("Loading…"));
        thread::spawn(move || {
            let drives = shell::drives();
            let rows = build_this_pc_rows(&drives);
            post(move |a| a.this_pc_loaded(id, epoch, drives, rows));
        });
    }

    fn this_pc_loaded(&self, id: u64, epoch: u64, drives: Vec<shell::Drive>, rows: Vec<ThisPcRow>) {
        let Some(t) = self.current(id, epoch) else { return };
        if !is_this_pc(&t.cwd.borrow()) || t.query().is_some() {
            return;
        }
        *self.drives.borrow_mut() = drives;
        self.sync_places();
        self.apply_this_pc_rows(&t, rows);
    }

    fn apply_this_pc_rows(&self, t: &Rc<Tab>, rows: Vec<ThisPcRow>) {
        let entries: Vec<Entry> = rows
            .iter()
            .map(|r| Entry::new(std::ffi::OsStr::new(&r.title), r.total, r.free as i64, ATTR_DIR))
            .collect();
        let paths: Vec<String> = rows.iter().map(|r| r.path.display().to_string()).collect();
        let kinds: Vec<String> = rows.iter().map(|r| r.kind.clone()).collect();
        let dates: Vec<String> = rows.iter().map(|r| r.date.clone()).collect();
        let sizes: Vec<String> = rows.iter().map(|r| r.size.clone()).collect();
        let usage: Vec<f32> = rows.iter().map(|r| r.usage).collect();
        let (key, desc) = t.sort.get();
        let mut order = display_order(&entries, key, desc);
        // Keep Explorer's grouping: user folders first, then drives. A stable sort preserves
        // the column order inside each group.
        group_this_pc_order(&mut order, &kinds);
        t.rows.replace_virtual(entries, order, paths, kinds, dates, sizes, usage);
        *t.note.borrow_mut() = String::new();
        t.list_ms.set(0.0);
        self.sync_fit(t);
        self.update_status(t);
    }

    /// A device was added or removed: refresh the sidebar and the "This PC" view once the
    /// burst of WM_DEVICECHANGE messages settles.
    fn devices_changed(&self) {
        if self.devices_due.replace(true) {
            return;
        }
        slint::Timer::single_shot(Duration::from_millis(600), || post(|a| a.refresh_drives()));
    }

    fn refresh_drives(&self) {
        self.devices_due.set(false);
        thread::spawn(move || {
            let drives = shell::drives();
            let rows = build_this_pc_rows(&drives);
            post(move |a| a.drives_refreshed(drives, rows));
        });
    }

    fn drives_refreshed(&self, drives: Vec<shell::Drive>, rows: Vec<ThisPcRow>) {
        *self.drives.borrow_mut() = drives;
        self.sync_places();
        // Refresh every tab that is showing "This PC" (search results and pending navigations
        // bring their own contents and are left alone).
        let ids: Vec<u64> = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| is_this_pc(&t.cwd.borrow()) && t.query().is_none() && t.pending.borrow().is_none())
            .map(|t| t.id)
            .collect();
        for id in ids {
            if let Some(t) = self.find(id) {
                t.epoch.fetch_add(1, SeqCst); // any in-flight snapshot is now older
                self.apply_this_pc_rows(&t, rows.clone());
            }
        }
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
                return self.set_note(&t, t!("Can't open: {}", e));
            }
            return self.set_note(&t, t!("Partially read: {}", e));
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
        let (key, desc) = t.sort.get();
        // ponytail: the folder tree is flattened on the UI thread; a huge folder may stall briefly.
        if t.search.borrow().is_none() && TREE_VIEW.load(Relaxed) && !is_this_pc(&t.cwd.borrow()) {
            self.browse_flatten(t);
            self.apply_after(t);
            t.rows.notify.reset();
            self.reveal_cursor(t);
            self.sync_fit(t);
            self.update_status(t);
            return;
        }
        // A search tree sorts within each folder, so it is cheap enough to do here.
        if t.search.borrow().is_some() {
            let flat = {
                let search = t.search.borrow();
                let search = search.as_ref().expect("checked above");
                let entries = t.rows.entries.borrow();
                search.flatten(&entries, key, desc)
            };
            t.hit_count.set(flat.hits);
            t.rows.apply_flat(flat);
            self.reveal_cursor(t);
            self.sync_fit(t);
            self.update_status(t);
            return;
        }
        let entries = t.rows.entries.borrow().clone();
        // Identifies the list being sorted. (The order's length can't: hidden items are left out.)
        let sorting = (Arc::as_ptr(&entries) as usize, entries.len());
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
        // Stale if the user navigated, searched or sorted again, or entries changed while sorting.
        if seq != t.sort_gen.get() || sorting != current || t.search.borrow().is_some() {
            return;
        }
        let mut order = order;
        if at_this_pc(&t) {
            let kinds = t.rows.kinds.borrow();
            group_this_pc_order(&mut order, &kinds);
        }
        *t.rows.order.borrow_mut() = order;
        if t.search.borrow().is_none() {
            *t.note.borrow_mut() = t!("List {} ms · sort {} ms", format!("{:.1}", t.list_ms.get()), format!("{:.1}", ms(took)));
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
        if is_this_pc(&t.cwd.borrow()) {
            return self.load_this_pc(t);
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
            return self.set_note(&t, t!("Can't refresh: {}", e));
        }
        if let Some(after) = t.after.borrow_mut().as_mut().filter(|a| a.select.is_empty()) {
            after.select = t.rows.selection().into_iter().map(|i| t.rows.name(i)).collect();
        }
        t.bump_sort();
        t.rows.replace(all, order);
        if TREE_VIEW.load(Relaxed) {
            self.browse_flatten(&t); // ponytail: a refresh collapses opened folders
        }
        t.list_ms.set(ms(took));
        t.cursor.set(None);
        t.anchor.set(None);
        self.apply_after(&t);
        t.rows.notify.reset();
        self.reveal_cursor(&t);
        let quiet = !t.note.borrow().is_empty();
        if auto && !quiet {
            *t.note.borrow_mut() = t!("Change detected · auto refresh");
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
            self.new_tab_bg(t.path_of(i));
        }
    }

    /// Left press moved far enough: hand the selection to OLE as a native drag.
    fn row_drag(&self, row: usize) {
        if !self.drag_armed.replace(false) {
            return;
        }
        let t = self.tab();
        if at_this_pc(&t) {
            return;
        }
        // `moved` also fires while another mouse button is held; only a left drag counts.
        if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000 == 0 {
            return;
        }
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
        self.release_pointer(PointerEventButton::Left);
    }

    /// Ends the pointer gesture a native menu left behind: its modal loop eats the
    /// button-up, so the row's TouchArea would otherwise keep the grab and send the next
    /// click (and the drag it starts) to the row that was right-clicked.
    fn end_menu_pointer(&self) {
        self.ui().window().dispatch_event(WindowEvent::PointerExited);
        self.shield_menu_click();
    }

    /// The left click that dismissed a native menu also lands in the window. Absorb it:
    /// the menu's modal loop leaves Slint's pointer position at the pre-menu spot, so the
    /// click would otherwise select, open, or start a drag at the wrong place.
    fn shield_menu_click(&self) {
        const VK_DOWN: u16 = 0x8000;
        if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & VK_DOWN == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd() as _);
        let (mut pt, mut client) = (POINT::default(), RECT::default());
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = GetClientRect(hwnd, &mut client);
            let _ = ScreenToClient(hwnd, &mut pt);
        }
        if pt.x < 0 || pt.y < 0 || pt.x >= client.right || pt.y >= client.bottom {
            return;
        }
        let ui = self.ui();
        ui.set_click_shield(true);
        let weak = ui.as_weak();
        // The shield clears itself on the button-up; this only recovers if none arrives.
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_click_shield(false);
            }
        });
    }

    fn release_pointer(&self, button: PointerEventButton) {
        let ui = self.ui();
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(HWND(self.hwnd() as _), &mut pt);
        }
        let scale = ui.window().scale_factor();
        let position = slint::LogicalPosition::new(pt.x as f32 / scale, pt.y as f32 / scale);
        ui.window().dispatch_event(WindowEvent::PointerReleased { position, button });
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
        // Selection is indexed by entry, not row: hidden items and collapsed tree nodes have no row.
        let base = self.rubber_base.borrow();
        let mut want = vec![false; t.rows.selected.borrow().len()];
        if base.len() == want.len() {
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
        let (prefix, typed) = {
            let mut ta = self.typeahead.borrow_mut();
            if ta.at.is_none_or(|last| now - last > Duration::from_secs(1)) {
                ta.lower.clear();
                ta.typed.clear();
            }
            ta.lower += &text.to_lowercase();
            ta.typed += text;
            ta.at = Some(now);
            (ta.lower.clone(), ta.typed.clone())
        };
        self.show_key_hint(typed);
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

    /// Shows the type-ahead text in the list toast, then hides it after a pause.
    fn show_key_hint(&self, text: String) {
        let seq = self.hint_seq.get() + 1;
        self.hint_seq.set(seq);
        self.ui().set_key_hint(text.into());
        slint::Timer::single_shot(Duration::from_millis(1400), move || {
            app(|a| {
                if a.hint_seq.get() == seq {
                    a.ui().set_key_hint("".into());
                }
            })
        });
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
            _ if ctrl && shift && ch == 't' => {
                let path = self.closed.borrow_mut().pop();
                if let Some(path) = path {
                    self.new_tab(path);
                }
            }
            _ if ctrl && ch == 't' => self.new_tab(home_path()),
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
            _ if ctrl && ch == 'z' && in_list => self.undo(),
            _ if is(Key::Delete) => self.delete(shift),
            _ if is(Key::F2) && shift => self.rename_many(),
            _ if is(Key::F2) => self.start_rename(),
            _ if ctrl && ch == ' ' => {
                if let Some(c) = t.cursor.get() {
                    t.rows.toggle(c);
                }
                self.update_status(&t);
            }
            // A search-tree folder collapses/expands with Left/Right; other rows move normally.
            _ if (is(Key::LeftArrow) || is(Key::RightArrow))
                && self.tree_arrow(&t, is(Key::RightArrow)) => {}
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
                            a.set_note(&t, t!("Can't open: {}", e));
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
            let after = came_from(&t.cwd.borrow(), &p);
            self.navigate(&t, p, Hist::Go(pos - 1), after);
        }
    }

    fn forward(&self) {
        let t = self.tab();
        let pos = t.hist_pos.get();
        let next = t.history.borrow().get(pos + 1).cloned();
        if let Some(p) = next {
            let after = came_from(&t.cwd.borrow(), &p);
            self.navigate(&t, p, Hist::Go(pos + 1), after);
        }
    }

    /// Goes to the parent and selects the folder we came from.
    fn up(&self) {
        let t = self.tab();
        let cwd = t.cwd.borrow().clone();
        if is_this_pc(&cwd) {
            return;
        }
        if let (Some(parent), Some(name)) = (cwd.parent(), cwd.file_name()) {
            let after = After { select: vec![name.to_string_lossy().into_owned()], ..Default::default() };
            self.navigate(&t, parent.to_path_buf(), Hist::Push, Some(after));
        }
    }

    fn address(&self, text: &str) {
        let t = self.tab();
        let mut s = expand_env(text.trim().trim_matches('"'));
        if s.is_empty() {
            return;
        }
        if is_this_pc_name(&s) {
            return self.navigate(&t, PathBuf::from(THIS_PC), Hist::Push, None);
        }
        if s.len() == 2 && s.ends_with(':') {
            s.push('\\'); // "D:" means the drive root here, not D:'s current directory
        }
        let cwd = t.cwd.borrow().clone();
        // Relative names have no meaning in the virtual "This PC" folder; a leading "\" is
        // resolved against the system drive, like Explorer's current-drive notation.
        let p = if is_this_pc(&cwd) {
            if Path::new(&s).is_absolute() {
                PathBuf::from(&s)
            } else if s.starts_with('\\') {
                PathBuf::from(format!("C:{s}"))
            } else {
                return self.set_note(&t, t!("Enter the full path"));
            }
        } else {
            cwd.join(&s) // absolute input replaces cwd
        };
        self.ui().invoke_focus_list();
        // The user typed this path, so a file is opened; anything else is navigated to.
        let id = t.id;
        thread::spawn(move || {
            if p.is_file() {
                let r = shell::open(&p);
                post(move |a| {
                    if let Some(t) = a.find(id) {
                        a.sync_view(&t);
                        a.set_note(&t, r.err().map(|e| t!("Can't open: {}", e)).unwrap_or_default());
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
        let cwd = t.cwd.borrow().clone();
        if is_this_pc(&cwd) || cwd.as_os_str().is_empty() {
            return self.set_note(t, t!("Search is only available in folders"));
        }
        let options = t.search_options.get();
        let scope = Scope::Under(cwd.clone());
        let plan = match search::plan_mode(&query, &cwd, &scope, options.regex) {
            Ok(plan) => plan,
            Err(e) => return self.set_note(t, e),
        };
        // A typed path may narrow the search, but the result must stay inside this folder.
        let Some(root) = plan.scope.root() else {
            return self.set_note(t, t!("Search is limited to the current folder and its subfolders"));
        };
        if search::below(root, &cwd).is_none() {
            return self.set_note(t, t!("Search is limited to the current folder and its subfolders"));
        }
        let pattern = match Pattern::compile_mode(&plan.pattern, options.regex) {
            Ok(pattern) => pattern,
            Err(e) => return self.set_note(t, e),
        };
        let service = service_pattern(&plan.pattern, options.regex);
        let seq = self.search_seq.fetch_add(1, SeqCst) + 1;
        match plan.scope.clone() {
            Scope::Direct(root) => self.direct_search(t, query, root, pattern, seq),
            Scope::Under(root) if index::is_indexable(&root) => {
                self.index_or_walk(t.id, query, plan.scope, service, seq, false, options.regex)
            }
            Scope::Under(root) => {
                let (id, latest) = (t.id, self.search_seq.clone());
                thread::spawn(move || walk_search_worker(id, seq, latest, root, query, pattern));
            }
            Scope::All => {
                self.index_or_walk(t.id, query, plan.scope, service, seq, false, options.regex)
            }
        }
    }

    /// One folder only: lists it and keeps the entries that match.
    fn direct_search(&self, t: &Rc<Tab>, query: String, root: PathBuf, pattern: Pattern, seq: u64) {
        let (id, latest) = (t.id, self.search_seq.clone());
        thread::spawn(move || {
            let start = Instant::now();
            let mut entries = Vec::new();
            let mut tree = SearchTree::default();
            tree.root(&mut entries, &root);
            let mut more = false;
            let mut count = 0usize;
            let res = list_dir(&root, |batch| {
                if latest.load(SeqCst) != seq {
                    return false; // typed on; a newer search is running
                }
                for e in batch {
                    if pattern.matches(&e.name()) {
                        let dir = e.is_dir();
                        let path = root.join(e.os_name());
                        tree.insert(&mut entries, &path, dir, Some(e), Some(&root));
                        count += 1;
                        if count >= SEARCH_LIMIT {
                            more = true;
                            return false;
                        }
                    }
                }
                true
            });
            let took = start.elapsed();
            if latest.load(SeqCst) != seq {
                return;
            }
            post(move |a| {
                a.direct_found(
                    id,
                    seq,
                    query,
                    Scope::Direct(root),
                    entries,
                    tree,
                    took,
                    more,
                    res.err().map(|e| e.to_string()),
                )
            });
        });
    }

    /// The index is the fast path; a subtree on a non-indexed volume (or without the service)
    /// falls back to walking the folder.
    fn index_or_walk(
        &self,
        id: u64,
        query: String,
        scope: Scope,
        pattern: String,
        seq: u64,
        offer_install: bool,
        regex: bool,
    ) {
        let latest = self.search_seq.clone();
        thread::spawn(move || {
            let start = Instant::now();
            let r = index::query(&scope, &pattern, SEARCH_LIMIT);
            let took = start.elapsed();
            if latest.load(SeqCst) != seq {
                return;
            }
            match r {
                Ok(res) => {
                    let (entries, tree) = tree_from_hits(&res.hits, &scope);
                    post(move |a| a.found(id, seq, query, scope, res, entries, tree, took));
                }
                Err(QueryError::NotInstalled) if offer_install => {
                    post(move |a| a.offer_install(id, query))
                }
                Err(e @ (QueryError::NotInstalled | QueryError::Unavailable(_) | QueryError::Rejected(_))) => {
                    match scope.root().map(Path::to_path_buf) {
                        Some(root) => {
                            let pattern = match Pattern::compile_mode(&pattern, regex) {
                                Ok(pattern) => pattern,
                                Err(e) => {
                                    return post(move |a| {
                                        if let Some(t) = a.find(id) {
                                            a.set_note(&t, e);
                                        }
                                    });
                                }
                            };
                            walk_search_worker(id, seq, latest, root, query, pattern);
                        }
                        None => {
                            let message = match e {
                                QueryError::NotInstalled => t!("Search needs the index service"),
                                QueryError::Unavailable(e) => t!("Can't search: {}", e),
                                QueryError::Rejected(e) => e,
                            };
                            post(move |a| {
                                if let Some(t) = a.find(id) {
                                    a.set_note(&t, message);
                                }
                            });
                        }
                    }
                }
            }
        });
    }

    /// Applies a finished result set (entries plus tree) to the tab.
    fn install_search(&self, t: &Rc<Tab>, query: String, scope: Scope, entries: Vec<Entry>, tree: SearchTree) {
        t.epoch.fetch_add(1, SeqCst); // drop any folder listing still arriving
        t.pending.take();
        t.bump_sort();
        let options = t.search_options.get();
        let flat = {
            let (key, desc) = t.sort.get();
            tree.flatten(
                &entries,
                key,
                desc,
                SHOW_HIDDEN.load(Relaxed),
                options.folders,
                options.files,
                options.tree,
            )
        };
        t.hit_count.set(flat.hits);
        t.rows.replace_search(entries, flat);
        t.cursor.set(None);
        t.anchor.set(None);
        *t.search.borrow_mut() = Some(Search { query, scope, tree, options });
    }

    #[allow(clippy::too_many_arguments)]
    fn found(&self, id: u64, seq: u64, query: String, scope: Scope, res: Results, entries: Vec<Entry>, tree: SearchTree, took: Duration) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        let returned = res.hits.len();
        let (more, indexing) = (res.more, res.indexing);
        let root = scope.root().map(Path::to_path_buf);
        self.install_search(&t, query.clone(), scope, entries, tree);
        let mut note = t!("Search {} ms", format!("{:.0}", ms(took)));
        if let Some(root) = root.filter(|r| !same_path(r, &t.cwd.borrow())) {
            note += &t!(" · under {}", root.display());
        }
        if more {
            note += &t!(" · more available, showing the first {}", returned);
        }
        if indexing > 0 {
            note += &t!(" · still indexing, results are partial");
            // Ask again shortly, until the index is complete.
            slint::Timer::single_shot(Duration::from_secs(2), move || {
                app(|a| {
                    if let Some(t) = a.find(id)
                        && a.search_seq.load(SeqCst) == seq
                        && t.query().as_deref() == Some(&query)
                    {
                        a.search_in(&t, query);
                    }
                })
            });
        }
        *t.note.borrow_mut() = note;
        if self.is_active(&t) {
            self.ui().set_search_mode(true);
            self.ui().set_list_y(0.0);
        }
        self.update_status(&t);
        self.sync_fit(&t);
    }

    /// A one-folder search finished.
    #[allow(clippy::too_many_arguments)]
    fn direct_found(
        &self,
        id: u64,
        seq: u64,
        query: String,
        scope: Scope,
        entries: Vec<Entry>,
        tree: SearchTree,
        took: Duration,
        more: bool,
        err: Option<String>,
    ) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        let root = scope.root().map(Path::to_path_buf);
        self.install_search(&t, query, scope, entries, tree);
        let mut note = t!("Search {} ms", format!("{:.0}", ms(took)));
        if let Some(root) = root.filter(|r| !same_path(r, &t.cwd.borrow())) {
            note += &t!(" · under {}", root.display());
        }
        if more {
            note += &t!(" · more available, showing the first {}", SEARCH_LIMIT);
        }
        if let Some(e) = err {
            note += &t!(" · Partially read: {}", e);
        }
        *t.note.borrow_mut() = note;
        if self.is_active(&t) {
            self.ui().set_search_mode(true);
            self.ui().set_list_y(0.0);
        }
        self.update_status(&t);
        self.sync_fit(&t);
    }

    /// A walk started: show an empty tree immediately; batches fill it in.
    fn walk_begin(&self, id: u64, seq: u64, query: String, root: PathBuf, entry: Entry) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        t.epoch.fetch_add(1, SeqCst);
        t.pending.take();
        t.bump_sort();
        let mut entries = Vec::new();
        let mut tree = SearchTree::default();
        tree.root_entry(&mut entries, &root, entry);
        t.rows.replace_search(entries, FlatTree::default());
        t.hit_count.set(0);
        t.cursor.set(None);
        t.anchor.set(None);
        *t.search.borrow_mut() = Some(Search { query, scope: Scope::Under(root), tree, options: t.search_options.get() });
        *t.note.borrow_mut() = t!("Searching the folder directly, without an index…");
        if self.is_active(&t) {
            self.ui().set_search_mode(true);
            self.ui().set_list_y(0.0);
        }
        self.update_status(&t);
    }

    /// Inserts one batch of walk hits into the search tree.
    fn name_batch(&self, id: u64, seq: u64, hits: Vec<(PathBuf, bool)>, entries: Vec<Entry>) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        let flat = {
            let mut search = t.search.borrow_mut();
            let Some(search) = search.as_mut() else { return };
            let base = search.scope.root().map(Path::to_path_buf);
            let mut all = t.rows.entries.borrow_mut();
            let nodes = Arc::make_mut(&mut all);
            for ((path, dir), entry) in hits.into_iter().zip(entries) {
                search.tree.insert(nodes, &path, dir, Some(entry), base.as_deref());
            }
            drop(all);
            let all = t.rows.entries.borrow();
            let (key, desc) = t.sort.get();
            search.flatten(&all, key, desc)
        };
        t.hit_count.set(flat.hits);
        t.rows.update_search(flat);
        self.update_status(&t);
    }

    fn walk_done(&self, id: u64, seq: u64, took: Duration, capped: bool, err: Option<String>) {
        let Some(t) = self.find(id) else { return };
        if self.search_seq.load(SeqCst) != seq {
            return;
        }
        let mut note = t!("Folder search {} s", format!("{:.1}", took.as_secs_f64()));
        if capped {
            note += &t!(" · stopped at the first {}", SEARCH_LIMIT);
        }
        if let Some(e) = err {
            note += &format!(" · {e}");
        }
        *t.note.borrow_mut() = note;
        self.update_status(&t);
        self.sync_fit(&t);
    }

    fn exit_search(&self, t: &Rc<Tab>) {
        self.search_seq.fetch_add(1, SeqCst); // results still on their way are dropped
        if t.search.take().is_some() {
            self.reload(t, After::default());
        }
        self.sync_view(t);
    }

    /// Search options changed: display filters re-flatten; regex needs a new query.
    fn search_options_changed(&self) {
        let t = self.tab();
        let mut options = SearchOptions {
            folders: self.ui().get_search_folders(),
            files: self.ui().get_search_files(),
            regex: self.ui().get_search_regex(),
            tree: self.ui().get_search_view() == 1,
        };
        if !options.folders && !options.files {
            options.files = true;
            self.ui().set_search_files(true);
        }
        let old = t.search_options.replace(options);
        if old == options {
            return;
        }
        let Some(query) = t.query() else { return };
        if old.regex != options.regex {
            self.search_in(&t, query);
        } else {
            self.reflatten_search(&t);
        }
    }

    /// Applies folder/file/view filters to the loaded results without running the search again.
    fn reflatten_search(&self, t: &Rc<Tab>) {
        let options = t.search_options.get();
        let flat = {
            let mut search = t.search.borrow_mut();
            let Some(search) = search.as_mut() else { return };
            search.options = options;
            let entries = t.rows.entries.borrow();
            let (key, desc) = t.sort.get();
            search.flatten(&entries, key, desc)
        };
        // Rows hidden by the new filters must not stay selected or under the cursor.
        let visible: HashSet<u32> = flat.order.iter().copied().collect();
        for (i, selected) in t.rows.selected.borrow_mut().iter_mut().enumerate() {
            if *selected && !visible.contains(&(i as u32)) {
                *selected = false;
            }
        }
        if t.cursor.get().is_some_and(|c| !visible.contains(&c)) {
            t.cursor.set(None);
        }
        if t.anchor.get().is_some_and(|a| !visible.contains(&a)) {
            t.anchor.set(None);
        }
        t.hit_count.set(flat.hits);
        t.rows.update_search(flat);
        self.sync_fit(t);
        self.update_status(t);
    }

    /// Shows the folder listing as a tree: the listed items are the roots, collapsed.
    fn browse_flatten(&self, t: &Tab) {
        let (key, desc) = t.sort.get();
        let flat = {
            let entries = t.rows.entries.borrow();
            let mut browse = t.rows.browse.borrow_mut();
            let tree = browse.get_or_insert_with(|| SearchTree::browse(&entries, &t.cwd.borrow()));
            tree.flatten(&entries, key, desc, SHOW_HIDDEN.load(Relaxed), true, true, true)
        };
        t.rows.update_search(flat);
    }

    /// Expands a folder of the browsing tree, reading it on a worker the first time.
    fn toggle_browse(&self, t: &Rc<Tab>, node: u32) {
        let (toggled, path) = {
            let mut browse = t.rows.browse.borrow_mut();
            let Some(tree) = browse.as_mut() else { return };
            if tree.toggle(node) {
                (true, None)
            } else if tree.is_unread(node) {
                (false, tree.path(node).map(Path::to_path_buf))
            } else {
                return;
            }
        };
        if toggled {
            self.browse_flatten(t);
            return self.update_status(t);
        }
        let Some(path) = path else { return };
        let (id, epoch) = (t.id, t.epoch.load(SeqCst)); // a new listing bumps the epoch
        thread::spawn(move || {
            let mut all = Vec::new();
            let res = list_dir(&path, |b| {
                all.extend(b);
                true
            });
            post(move |a| a.browse_loaded(id, epoch, node, all, res.err()));
        });
    }

    fn browse_loaded(&self, id: u64, epoch: u64, node: u32, list: Vec<Entry>, err: Option<io::Error>) {
        let Some(t) = self.current(id, epoch) else { return };
        if let Some(e) = err {
            return self.set_note(&t, t!("Can't open: {}", e));
        }
        {
            let mut entries = t.rows.entries.borrow_mut();
            let mut browse = t.rows.browse.borrow_mut();
            let Some(tree) = browse.as_mut().filter(|tree| tree.is_unread(node)) else { return };
            tree.add_children(Arc::make_mut(&mut entries), node, list);
        }
        self.browse_flatten(&t);
        self.update_status(&t);
    }

    /// Expands or collapses one folder row of the search tree.
    fn toggle_expand(&self, row: usize) {
        let t = self.tab();
        let Some(node) = t.rows.entry(row) else { return };
        if t.search.borrow().is_none() {
            return self.toggle_browse(&t, node);
        }
        let flat = {
            let mut search = t.search.borrow_mut();
            let Some(search) = search.as_mut() else { return };
            if !search.options.tree || !search.tree.toggle(node) {
                return;
            }
            let all = t.rows.entries.borrow();
            let (key, desc) = t.sort.get();
            search.flatten(&all, key, desc)
        };
        t.hit_count.set(flat.hits);
        t.rows.apply_flat(flat);
        self.update_status(&t);
    }

    /// Left/Right on a search-tree folder row; false lets normal cursor movement happen.
    fn tree_arrow(&self, t: &Tab, right: bool) -> bool {
        let tree_on = match &*t.search.borrow() {
            Some(s) => s.options.tree,
            None => t.rows.browse.borrow().is_some(),
        };
        if !tree_on {
            return false;
        }
        let Some(row) = self.cursor_row(t) else { return false };
        let Some(node) = t.rows.entry(row) else { return false };
        let meta = t.rows.meta.borrow().get(node as usize).copied().unwrap_or_default();
        if !meta.expandable || meta.expanded == right {
            return false;
        }
        self.toggle_expand(row);
        true
    }

    /// "Search in this folder": search inside `path` and focus the search box.
    fn search_here(&self, path: PathBuf) {
        let t = self.tab();
        if is_this_pc(&path) {
            return self.set_note(&t, t!("Search is only available in folders"));
        }
        if !same_path(&t.cwd.borrow(), &path) {
            self.navigate(&t, path, Hist::Push, None);
        }
        self.ui().invoke_focus_search();
    }

    /// The first search without the index service asks to install it (UAC).
    fn offer_install(&self, id: u64, query: String) {
        let Some(t) = self.find(id) else { return };
        if self.install_offered.replace(true) {
            return self.set_note(&t, t!("Search needs the index service (not installed)"));
        }
        let hwnd = self.hwnd();
        let here = std::env::current_exe().unwrap_or_default();
        // Elevating an exe the user (or anything running as them) can overwrite would hand that
        // code admin rights; only offer it from an admin-only folder such as Program Files.
        if !shell::program_files().is_ok_and(|pf| here.starts_with(pf)) {
            return self.set_note(
                &t,
                t!("Index service is not installed · run 'fex-index install' as administrator"),
            );
        }
        let exe = here.with_file_name("fex-index.exe");
        thread::spawn(move || {
            let ask = t!("Fast Explorer needs its index service to search every drive instantly.\n\nInstall it now?\n· Administrator permission (UAC) is required.\n· Only file names on fixed NTFS drives are indexed; file contents are never read.\n· Search is only available to this Windows account.");
            let r = shell::confirm(hwnd, &ask).then(|| shell::run_elevated(&exe, "install", hwnd));
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                match r {
                    None => a.set_note(&t, t!("Search needs the index service")),
                    Some(Ok(0)) => {
                        a.set_note(&t, t!("Index service installed · indexing…"));
                        // Give the service a moment to open its pipe.
                        slint::Timer::single_shot(Duration::from_secs(1), move || app(|a| a.search_in(&t, query)));
                    }
                    Some(Ok(code)) => a.set_note(&t, t!("Index service install failed (code {})", code)),
                    Some(Err(e)) => a.set_note(&t, t!("Index service install cancelled: {}", e)),
                }
            });
        });
    }

    /// Looks for a newer release on a worker thread and shows the outcome in the settings panel.
    /// Installing always asks first and runs the installer (UAC); we quit so it can replace files.
    fn check_update(&self) {
        let hwnd = self.hwnd();
        self.ui().set_update_status(t!("Checking…").into());
        thread::spawn(move || {
            let found = update::check();
            update::mark_checked();
            let status = match &found {
                Ok(Some(rel)) => t!("New version v{} available", rel.version),
                Ok(None) => t!("You're up to date"),
                Err(e) => t!("Check failed: {}", e),
            };
            post(move |a| a.ui().set_update_status(status.into()));
            let Ok(Some(rel)) = found else { return };
            let ask = t!(
                "Version v{} is available (current v{}).\n\nDownload and install it now? Fast Explorer will close during installation.",
                rel.version,
                update::CURRENT
            );
            if !shell::confirm(hwnd, &ask) {
                return;
            }
            post(|a| {
                let tabs: Vec<PathBuf> = a.tabs.borrow().iter().map(|t| t.cwd.borrow().clone()).collect();
                update::save_session(a.active.get(), &tabs);
                a.ui().set_update_status(t!("Downloading…").into());
            });
            match update::download(&rel).map_err(io::Error::other).and_then(|f| shell::open(&f)) {
                Ok(()) => post(|_| {
                    let _ = slint::quit_event_loop();
                }),
                Err(e) => {
                    post(|a| a.ui().set_update_status(t!("Update failed").into()));
                    shell::alert(hwnd, &t!("Update failed: {}", e));
                }
            }
        });
    }

    /// Switches the language (0 = system, 1 = English, 2 = Korean) immediately.
    fn set_language(&self, i: i32) {
        let lang = ["", "en", "ko"].get(i as usize).copied().unwrap_or("");
        *LANG.lock().unwrap() = lang.to_string();
        save_view();
        fex_core::i18n::set_lang(lang);
        let _ = slint::select_bundled_translation(if fex_core::i18n::korean() { "ko" } else { "en" });
        // ponytail: rebuild listings so row texts follow; transient notes stay in the old language
        for t in self.tabs.borrow().iter() {
            self.reload(t, After::default());
        }
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
            return Err(t!("The shell menu only works on items in a single folder").into()); // e.g. search hits
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
        if self.menu_busy.get() || cwd.as_os_str().is_empty() || is_this_pc(&cwd) {
            return;
        }
        let sel = t.rows.selection();
        let _ = match sel.len() {
            0 => self.prefetch(true, vec![cwd]),
            1..=500 => self.prefetch(false, sel.iter().map(|&i| t.path_of(i)).collect()),
            _ => return, // huge selections: built on demand ("More options")
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
        // Run the menu's modal loop outside the pointer-event handler, after a repaint
        // so the selection highlight is visible while the menu is open.
        slint::Timer::single_shot(Duration::from_millis(30), move || app(|a| a.context_menu(false, at)));
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
    /// (prefetched), else they are one click away under "More options".
    fn context_menu(&self, background: bool, at: (i32, i32)) {
        let t = self.tab();
        let cwd = t.cwd.borrow().clone();
        let paths = if background { vec![cwd.clone()] } else { self.selected_paths(&t) };
        if self.menu_busy.get() || paths.is_empty() || cwd.as_os_str().is_empty() {
            return;
        }
        if at_this_pc(&t) {
            if background {
                let top = [own(CMD_REFRESH, hint(t!("Refresh"), "F5"))];
                let id = popup::show(self.hwnd(), at, &top, &[], &[]);
                self.end_menu_pointer();
                if id == CMD_REFRESH {
                    self.reload(&t, After::default());
                }
                return;
            }
        }
        let seq = self.prefetch(background, paths.clone());
        let shell = seq.as_ref().ok().and_then(|&s| self.host.wait(s, MENU_WAIT));

        let mut top = if background {
            vec![
                with_icon(Own { disabled: !shell::clipboard_has_files(), ..own(CMD_PASTE, hint(t!("Paste"), "Ctrl+V")) }, popup::Icon::Glyph(popup::glyph::PASTE)),
                with_icon(own(CMD_NEW_FOLDER, hint(t!("New folder"), "Ctrl+Shift+N")), popup::Icon::Stock(SIID_FOLDER)),
                with_icon(own(CMD_NEW_FILE, t!("New text document")), popup::Icon::Stock(SIID_DOCNOASSOC)),
                with_icon(own(CMD_REFRESH, hint(t!("Refresh"), "F5")), popup::Icon::Glyph(popup::glyph::REFRESH)),
                SEP,
                with_icon(own(CMD_SEARCH_HERE, t!("Search in this folder")), popup::Icon::Glyph(popup::glyph::SEARCH)),
                with_icon(own(CMD_ADD_FAVORITE, t!("Add to favorites")), popup::Icon::Glyph(popup::glyph::STAR)),
            ]
        } else if at_this_pc(&t) {
            vec![
                Own { default: true, ..own(CMD_OPEN, hint(t!("Open"), "Enter")) },
                with_icon(own(CMD_OPEN_TAB, t!("Open in new tab")), popup::Icon::Glyph(popup::glyph::NEW_TAB)),
                with_icon(own(CMD_SEARCH_HERE, t!("Search in this folder")), popup::Icon::Glyph(popup::glyph::SEARCH)),
                with_icon(own(CMD_ADD_FAVORITE, t!("Add to favorites")), popup::Icon::Glyph(popup::glyph::STAR)),
                SEP,
                with_icon(own(CMD_COPY_PATH, hint(t!("Copy path"), "Ctrl+Shift+C")), popup::Icon::Glyph(popup::glyph::LINK)),
            ]
        } else {
            let sel = t.rows.selection();
            // Parent folders shown for context are not results: no rename/delete on them.
            let structural = t
                .search
                .borrow()
                .as_ref()
                .is_some_and(|s| sel.iter().any(|&i| !s.tree.is_hit(i)));
            let item_icon = popup::Icon::Path(paths[0].clone());
            let mut v = vec![with_icon(Own { default: true, ..own(CMD_OPEN, hint(t!("Open"), "Enter")) }, item_icon)];
            if let [i] = sel[..]
                && t.rows.is_dir(i)
            {
                v.push(with_icon(own(CMD_OPEN_TAB, t!("Open in new tab")), popup::Icon::Glyph(popup::glyph::NEW_TAB)));
                v.push(with_icon(own(CMD_SEARCH_HERE, t!("Search in this folder")), popup::Icon::Glyph(popup::glyph::SEARCH)));
                v.push(with_icon(own(CMD_ADD_FAVORITE, t!("Add to favorites")), popup::Icon::Glyph(popup::glyph::STAR)));
            }
            v.extend([
                SEP,
                with_icon(own(CMD_CUT, hint(t!("Cut"), "Ctrl+X")), popup::Icon::Glyph(popup::glyph::CUT)),
                with_icon(own(CMD_COPY, hint(t!("Copy"), "Ctrl+C")), popup::Icon::Glyph(popup::glyph::COPY)),
                with_icon(own(CMD_COPY_PATH, hint(t!("Copy path"), "Ctrl+Shift+C")), popup::Icon::Glyph(popup::glyph::LINK)),
            ]);
            v.push(with_icon(own(CMD_LOCKS, t!("Find processes using it")), popup::Icon::Glyph(popup::glyph::LOCK)));
            if !structural {
                v.extend([
                    SEP,
                    with_icon(own(CMD_RENAME, hint(t!("Rename"), "F2")), popup::Icon::Stock(SIID_RENAME)),
                    with_icon(own(CMD_DELETE, hint(t!("Delete"), "Del")), popup::Icon::Stock(SIID_DELETE)),
                    SEP,
                    with_icon(own(CMD_RENAME_MANY, hint(t!("Rename multiple"), "Shift+F2")), popup::Icon::Glyph(popup::glyph::RENAME_MANY)),
                ]);
            }
            v
        };
        let items: &[MenuItem] = match &shell {
            Some(Ok(items)) => items,
            _ => &[],
        };
        match (&seq, &shell) {
            (Ok(_), Some(Ok(_))) => {}
            (Ok(_), None) => top.extend([SEP, with_icon(own(CMD_MORE, t!("More options…")), popup::Icon::Glyph(popup::glyph::MORE))]),
            (Ok(_), Some(Err(e))) => {
                self.set_note(&t, t!("Shell menu failed: {}", e));
                top.extend([SEP, with_icon(own(CMD_RETRY, t!("More options (retry)…")), popup::Icon::Glyph(popup::glyph::MORE))]);
            }
            (Err(e), _) => self.set_note(&t, e.clone()),
        }
        let bottom = [with_icon(own(CMD_PROPERTIES, hint(t!("Properties"), "Alt+Enter")), popup::Icon::Stock(SIID_SETTINGS))];

        let id = popup::show(self.hwnd(), at, &top, items, &bottom);
        self.end_menu_pointer();
        match id {
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
            CMD_RENAME_MANY => self.rename_many(),
            CMD_LOCKS => self.find_locks(),
            CMD_DELETE => self.delete(false),
            CMD_PASTE => self.paste(),
            CMD_NEW_FOLDER => self.new_folder(),
            CMD_NEW_FILE => self.new_file(),
            CMD_REFRESH => self.reload(&t, After::default()),
            CMD_SEARCH_HERE => {
                if let Some(p) = paths.first() {
                    self.search_here(p.clone());
                }
            }
            CMD_ADD_FAVORITE => {
                if let Some(p) = paths.first() {
                    self.add_favorite_path(p.clone());
                }
            }
            CMD_PROPERTIES => self.properties_of(paths),
            _ => {}
        }
    }

    /// Sidebar drive right-click: open, eject (removable only), format, properties.
    fn drive_menu(&self, path: &str, at: (i32, i32)) {
        let root = PathBuf::from(path);
        if !self.drives.borrow().iter().any(|d| same_path(&d.path, &root)) {
            return self.favorite_menu(root, at);
        }
        let removable = self.drives.borrow().iter().any(|d| same_path(&d.path, &root) && d.kind == "Removable Disk");
        let fixed_or_removable = self.drives.borrow().iter().any(|d| same_path(&d.path, &root) && matches!(d.kind, "Removable Disk" | "Local Disk"));
        let mut top = vec![
            Own { default: true, ..own(CMD_OPEN, t!("Open")) },
            with_icon(own(CMD_OPEN_TAB, t!("Open in new tab")), popup::Icon::Glyph(popup::glyph::NEW_TAB)),
            SEP,
        ];
        if removable {
            top.push(own(CMD_EJECT, t!("Eject")));
        }
        if fixed_or_removable {
            top.push(own(CMD_FORMAT, t!("Format…")));
        }
        let bottom = [with_icon(own(CMD_PROPERTIES, t!("Properties")), popup::Icon::Stock(SIID_SETTINGS))];
        let id = popup::show(self.hwnd(), at, &top, &[], &bottom);
        self.end_menu_pointer();
        match id {
            CMD_OPEN => self.open_place(path),
            CMD_OPEN_TAB => self.open_place_in_tab(path),
            CMD_EJECT => self.eject_drive(root),
            CMD_FORMAT => {
                let hwnd = self.hwnd();
                thread::spawn(move || shell::format_dialog(hwnd, &root));
            }
            CMD_PROPERTIES => self.properties_of(vec![root]),
            _ => {}
        }
    }

    /// Sidebar favorite right-click.
    fn favorite_menu(&self, path: PathBuf, at: (i32, i32)) {
        let top = [
            Own { default: true, ..own(CMD_OPEN, t!("Open")) },
            with_icon(own(CMD_OPEN_TAB, t!("Open in new tab")), popup::Icon::Glyph(popup::glyph::NEW_TAB)),
            with_icon(own(CMD_ADD_FAVORITE, t!("Remove from favorites")), popup::Icon::Glyph(popup::glyph::STAR)),
        ];
        let bottom = [with_icon(own(CMD_PROPERTIES, t!("Properties")), popup::Icon::Stock(SIID_SETTINGS))];
        let id = popup::show(self.hwnd(), at, &top, &[], &bottom);
        self.end_menu_pointer();
        let s = path.display().to_string();
        match id {
            CMD_OPEN => self.open_place(&s),
            CMD_OPEN_TAB => self.open_place_in_tab(&s),
            CMD_ADD_FAVORITE => self.remove_favorite(&s),
            CMD_PROPERTIES => self.properties_of(vec![path]),
            _ => {}
        }
    }

    /// Leaves the drive in every tab (watchers hold it open), ejects off the UI thread,
    /// then refreshes the sidebar and "This PC" so the result is visible.
    fn eject_drive(&self, root: PathBuf) {
        let tabs: Vec<Rc<Tab>> = self.tabs.borrow().iter().cloned().collect();
        for t in tabs {
            let on = t.cwd.borrow().to_string_lossy().get(..2).is_some_and(|p| root.to_string_lossy().get(..2).is_some_and(|r| p.eq_ignore_ascii_case(r)));
            if on {
                self.navigate(&t, PathBuf::from(THIS_PC), Hist::Push, None);
            }
        }
        let t = self.tab();
        self.set_note(&t, t!("Ejecting…"));
        thread::spawn(move || {
            let r = shell::eject(&root);
            post(move |a| {
                let note = match r {
                    Ok(()) => t!("{} ejected", root.display()),
                    Err(e) => e,
                };
                a.set_note(&a.tab(), note);
                a.refresh_drives();
            });
        });
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
        self.set_note(&self.tab(), t!("Loading shell menu…"));
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
                self.end_menu_pointer();
                if (1..=0x7FFF).contains(&id) {
                    self.invoke_shell(Request::Invoke { seq, id });
                }
            }
            Some(Err(e)) => self.set_note(&t, t!("Shell menu failed: {}", e)),
            None => {
                // A shell extension is stuck: replace the host, Fast Explorer carries on.
                self.host.restart();
                *self.prefetched.borrow_mut() = None;
                self.set_note(&t, t!("The shell menu stopped responding and was restarted"));
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
            if is_this_pc(&t.cwd.borrow()) {
                return;
            }
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
            Ok(()) => t!("Copied {} paths", paths.len()),
            Err(e) => t!("Clipboard failed: {}", e),
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
        if at_this_pc(&t) {
            return self.set_note(&t, t!("Can't copy in This PC"));
        }
        let paths = self.selected_paths(&t);
        if paths.is_empty() {
            return;
        }
        let note = match shell::clipboard_set(&paths, cut, self.hwnd()) {
            Ok(()) => {
                if cut {
                    set_cut(&paths);
                } else {
                    clear_cut();
                }
                self.refresh_cut();
                if cut { t!("Cut {} items", paths.len()) } else { t!("Copied {} items", paths.len()) }
            }
            Err(e) => t!("Clipboard failed: {}", e),
        };
        self.set_note(&t, note);
    }

    /// Repaints every tab so cut items gain or lose their translucent look.
    fn refresh_cut(&self) {
        for t in self.tabs.borrow().iter() {
            t.rows.notify.reset();
        }
    }

    fn paste(&self) {
        let t = self.tab();
        if is_this_pc(&t.cwd.borrow()) {
            return self.set_note(&t, t!("Can't paste into This PC"));
        }
        // ponytail: reads the clipboard on the UI thread; an app that renders its clipboard
        // data lazily could stall this. Move to a worker if that shows up.
        let Some((src, cut)) = shell::clipboard_get(self.hwnd()) else {
            return self.set_note(&t, t!("No files to paste"));
        };
        let names = src.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
        let dst = t.cwd.borrow().clone();
        let op = if cut { Op::Move(src.clone(), dst.clone()) } else { Op::Copy(src.clone(), dst.clone()) };
        self.run_op(&t, After { select: names, ..Default::default() }, cut, move |hwnd| {
            let before = dir_entries(&dst);
            let r = shell::run(&op, Some(hwnd));
            if cut && matches!(r, Ok(true)) {
                shell::clipboard_clear(hwnd);
            }
            match r {
                Ok(true) => {
                    let created: Vec<PathBuf> =
                        dir_entries(&dst).difference(&before).map(|n| dst.join(n)).collect();
                    let undo = if cut {
                        Undo::Move { pairs: pair_moves(&src, &created) }
                    } else {
                        Undo::Copy { created }
                    };
                    Ok((true, Some(undo)))
                }
                Ok(false) => Ok((false, None)),
                Err(e) => Err(e),
            }
        });
    }

    /// Search results show parent folders for context; those rows must not be deleted or renamed.
    fn includes_tree_container(&self, t: &Tab) -> bool {
        let search = t.search.borrow();
        let Some(search) = search.as_ref() else { return false };
        let mut sel = t.rows.selection();
        if sel.is_empty() {
            sel.extend(t.cursor.get());
        }
        sel.into_iter().any(|i| !search.tree.is_hit(i))
    }

    fn delete(&self, permanent: bool) {
        let t = self.tab();
        if at_this_pc(&t) {
            return self.set_note(&t, t!("Can't delete in This PC"));
        }
        if self.includes_tree_container(&t) {
            return self.set_note(&t, t!("Folder tree rows can't be deleted"));
        }
        let paths = self.selected_paths(&t);
        if paths.is_empty() {
            return;
        }
        // The shell asks for confirmation itself on a permanent delete, like Explorer.
        self.run_op(&t, After::default(), false, move |hwnd| {
            let items = trash_items(&paths);
            let ok = shell::run(&Op::Delete { paths, permanent }, Some(hwnd))?;
            Ok((ok, (!permanent && ok).then_some(Undo::Delete { items })))
        });
    }

    fn start_rename(&self) {
        let t = self.tab();
        if at_this_pc(&t) {
            return self.set_note(&t, t!("Can't rename in This PC"));
        }
        if self.includes_tree_container(&t) {
            return self.set_note(&t, t!("Folder tree rows can't be renamed"));
        }
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
            let from = t.path_of(idx);
            let to = from.with_file_name(&new);
            let after = After { select: vec![new.clone()], ..Default::default() };
            self.run_op(&t, after, false, move |hwnd| {
                let ok = shell::rename(&from, &new, Some(hwnd))?;
                Ok((ok, ok.then_some(Undo::Rename { from: from.clone(), to })))
            });
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
        if at_this_pc(&t) {
            return self.set_note(&t, t!("Can't create new items in This PC"));
        }
        if t.search.borrow().is_some() {
            return self.set_note(&t, t!("Can't create new items in search results"));
        }
        self.ui().invoke_focus_list();
        let (id, dir) = (t.id, t.cwd.borrow().clone());
        let path_dir = dir.clone();
        thread::spawn(move || {
            let r = create(&dir);
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                match r {
                    Ok(name) => {
                        a.push_undo(Undo::Create(path_dir.join(&name)));
                        a.reload(&t, After { select: vec![name], rename: true, ..Default::default() });
                    }
                    Err(e) => a.show_error(&t, &t!("Couldn't create it."), &e),
                }
            });
        });
    }

    /// "Hidden items" / "File extensions" toggled: remember the choice, re-filter and redraw every tab.
    fn view_changed(&self) {
        let ui = self.ui();
        SHOW_HIDDEN.store(ui.get_show_hidden(), Relaxed);
        SHOW_EXT.store(ui.get_show_ext(), Relaxed);
        let tree = ui.get_tree_view();
        let tree_changed = TREE_VIEW.swap(tree, Relaxed) != tree;
        save_view();
        for t in self.tabs.borrow().iter() {
            let plain = t.search.borrow().is_none() && t.pending.borrow().is_none() && !is_this_pc(&t.cwd.borrow());
            if tree_changed && plain {
                self.reload(t, After::default()); // rebuild the listing in the new shape
            } else {
                self.resort(t);
            }
        }
        self.sync_fit(&self.tab());
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
        if is_this_pc(&cwd) {
            return None; // dropping onto a drive/folder row was handled above
        }
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
        if is_this_pc(&target) {
            return;
        }
        let t = self.tab();
        let id = t.id;
        let hwnd = self.hwnd();
        let op = if move_ { Op::Move(files.clone(), target.clone()) } else { Op::Copy(files.clone(), target.clone()) };
        thread::spawn(move || {
            let before = dir_entries(&target);
            let r = shell::run(&op, Some(hwnd));
            let undo = match &r {
                Ok(true) => {
                    let created: Vec<PathBuf> =
                        dir_entries(&target).difference(&before).map(|n| target.join(n)).collect();
                    Some(if move_ {
                        Undo::Move { pairs: pair_moves(&files, &created) }
                    } else {
                        Undo::Copy { created }
                    })
                }
                _ => None,
            };
            let (note, err) = match r {
                Ok(true) => (String::new(), None),
                Ok(false) => (t!("Cancelled").into(), None),
                Err(e) => (String::new(), Some(e)),
            };
            post(move |a| {
                if let Some(t) = a.find(id) {
                    if let Some(u) = undo {
                        a.push_undo(u);
                    }
                    a.set_note(&t, note);
                    if let Some(e) = err {
                        a.show_error(&t, &t!("Couldn't move or copy the files."), &e);
                    }
                    a.reload(&t, After::default());
                }
            });
        });
    }

    /// Runs a file operation on a worker (it may show dialogs), then refreshes the tab.
    /// `job` gets the owner window and returns `Ok((done, undo))`; `done` is false on cancel.
    fn run_op(
        &self,
        t: &Tab,
        after: After,
        cut_paste: bool,
        job: impl FnOnce(isize) -> io::Result<(bool, Option<Undo>)> + Send + 'static,
    ) {
        let (id, hwnd) = (t.id, self.hwnd());
        thread::spawn(move || {
            let r = job(hwnd);
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                match r {
                    Ok((true, undo)) => {
                        if let Some(u) = undo {
                            a.push_undo(u);
                        }
                        if cut_paste {
                            clear_cut();
                            a.refresh_cut();
                        }
                        *t.note.borrow_mut() = String::new();
                    }
                    Ok((false, _)) => *t.note.borrow_mut() = t!("Cancelled"),
                    Err(e) => a.show_error(&t, &t!("Couldn't complete the operation."), &e),
                }
                a.reload(&t, after);
            });
        });
    }

    fn push_undo(&self, u: Undo) {
        let mut undo = self.undo.borrow_mut();
        undo.push(u);
        if undo.len() > 50 {
            undo.remove(0);
        }
    }

    /// Ctrl+Z: runs the inverse of the most recent tracked operation.
    fn undo(&self) {
        let Some(u) = self.undo.borrow_mut().pop() else {
            return self.set_note(&self.tab(), t!("Nothing to undo"));
        };
        let t = self.tab();
        let (id, hwnd) = (t.id, self.hwnd());
        thread::spawn(move || {
            let r: io::Result<bool> = (|| {
                match u {
                    Undo::Create(path) => {
                        shell::run(&Op::Delete { paths: vec![path], permanent: false }, Some(hwnd))
                    }
                    Undo::Copy { created } => {
                        shell::run(&Op::Delete { paths: created, permanent: false }, Some(hwnd))
                    }
                    Undo::Rename { from, to } => {
                        let name =
                            from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        shell::rename(&to, &name, Some(hwnd))
                    }
                    Undo::RenameMany { pairs } => {
                        let back: Vec<(PathBuf, String)> = pairs
                            .iter()
                            .rev()
                            .map(|(from, to)| (to.clone(), from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
                            .collect();
                        let (done, errors) = tools::apply_renames(&back, hwnd);
                        match errors.first() {
                            Some(e) => Err(io::Error::other(e.clone())),
                            None => Ok(!done.is_empty()),
                        }
                    }
                    Undo::Move { pairs } => {
                        let mut by_parent: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
                        for (from, now) in pairs {
                            if let Some(parent) = from.parent() {
                                by_parent.entry(parent.to_path_buf()).or_default().push(now);
                            }
                        }
                        let mut ok = true;
                        for (parent, paths) in by_parent {
                            ok &= shell::run(&Op::Move(paths, parent), Some(hwnd))?;
                        }
                        Ok(ok)
                    }
                    Undo::Delete { items } => {
                        let n = shell::restore_deleted(&items, Some(hwnd))?;
                        if n == 0 {
                            Err(io::Error::new(io::ErrorKind::NotFound, t!("Not found in the Recycle Bin")))
                        } else {
                            Ok(true)
                        }
                    }
                }
            })();
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                match r {
                    Ok(true) => {
                        a.set_note(&t, t!("Undone"));
                        a.reload(&t, After::default());
                    }
                    Ok(false) => a.set_note(&t, t!("Undo cancelled")),
                    Err(e) => a.show_error(&t, &t!("Couldn't undo."), &e),
                }
            });
        });
    }
}

/// Names directly inside `dir`, used to see what an operation actually created.
fn dir_entries(dir: &Path) -> HashSet<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| PathBuf::from(e.file_name())).collect())
        .unwrap_or_default()
}

/// Pairs original paths with the items an operation created. Conflict renames ("name - Copy")
/// cannot be matched by name, so equal leftovers are paired in order.
fn pair_moves(src: &[PathBuf], created: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut used = vec![false; created.len()];
    let mut pairs = Vec::new();
    let mut unmatched = Vec::new();
    for s in src {
        let hit = s.file_name().and_then(|name| {
            let name = name.to_string_lossy();
            created.iter().enumerate().position(|(i, c)| {
                !used[i] && c.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&name))
            })
        });
        match hit {
            Some(i) => {
                used[i] = true;
                pairs.push((s.clone(), created[i].clone()));
            }
            None => unmatched.push(s.clone()),
        }
    }
    let rest: Vec<PathBuf> = created.iter().enumerate().filter(|(i, _)| !used[*i]).map(|(_, p)| p.clone()).collect();
    if unmatched.len() == rest.len() {
        pairs.extend(unmatched.into_iter().zip(rest));
    }
    pairs
}

/// What the Recycle Bin needs to find these items again for Ctrl+Z.
fn trash_items(paths: &[PathBuf]) -> Vec<shell::TrashedItem> {
    paths
        .iter()
        .map(|p| {
            let md = std::fs::symlink_metadata(p).ok();
            shell::TrashedItem {
                name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                from: p.parent().map(|d| d.display().to_string()).unwrap_or_default(),
                dir: md.is_some_and(|m| m.is_dir()),
            }
        })
        .collect()
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

fn hint(label: String, key: &str) -> String {
    format!("{label}\t{key}")
}

fn kind_of(e: &Entry) -> String {
    match (e.is_dir(), e.extension()) {
        (true, _) => t!("File folder"),
        (false, x) if x.is_empty() => t!("File"),
        (false, x) => t!("{} file", x.to_uppercase()),
    }
}

/// "This PC": keep user folders above drives while preserving the chosen column order within
/// each group. `kinds` is aligned with the entries; user folders are listed first.
fn group_this_pc_order(order: &mut [u32], kinds: &[String]) {
    let folder = t!("File folder");
    order.sort_by_key(|&i| u8::from(kinds.get(i as usize) != Some(&folder)));
}

/// Explorer-like capacity for drives: "236 GB", "1.81 TB", "512 MB".
fn format_capacity(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("TB", 1 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (unit, size) in UNITS {
        if bytes >= size {
            let value = bytes as f64 / size as f64;
            let text = if value >= 100.0 {
                format!("{value:.0}")
            } else if value >= 10.0 {
                format!("{value:.1}")
            } else {
                format!("{value:.2}")
            };
            return format!("{text} {unit}");
        }
    }
    "0 KB".to_string()
}

fn build_this_pc_rows(drives: &[shell::Drive]) -> Vec<ThisPcRow> {
    let mut rows: Vec<ThisPcRow> = shell::user_folders()
        .into_iter()
        .map(|(title, path)| ThisPcRow {
            title,
            path,
            kind: t!("File folder"),
            date: String::new(),
            size: String::new(),
            usage: -1.0,
            free: 0,
            total: 0,
        })
        .collect();
    for drive in drives {
        let (date, size, usage, free, total) = match shell::drive_space(&drive.path) {
            Some((free, total)) => {
                let usage = (total.saturating_sub(free)) as f32 / total as f32;
                (t!("{} free", format_capacity(free)), format_capacity(total), usage, free, total)
            }
            None => (t!("Unavailable"), String::new(), -1.0, 0, 0),
        };
        rows.push(ThisPcRow {
            title: drive.title.clone(),
            path: drive.path.clone(),
            kind: fex_core::i18n::tr(drive.kind),
            date,
            size,
            usage,
            free,
            total,
        });
    }
    rows
}

/// Path completion for the address bar: "This PC", drive roots, then matching folders/files
/// under the typed parent. Runs on a worker.
fn suggest_paths(input: &str, cwd: &Path) -> Vec<Suggest> {
    const LIMIT: usize = 8;
    let raw = input.trim().trim_matches('"');
    if raw.is_empty() {
        return Vec::new();
    }
    let expanded = expand_env(raw);
    let text = expanded.as_str();
    let lower = text.to_lowercase();
    let mut out: Vec<Suggest> = Vec::new();
    if "내 pc".starts_with(&lower) || "this pc".starts_with(&lower) || t!("This PC").to_lowercase().starts_with(&lower) || THIS_PC.to_lowercase().starts_with(&lower) {
        out.push(Suggest { name: t!("This PC").into(), detail: t!("Drives and folders").into(), path: THIS_PC.into() });
    }
    // "C" / "C:" autocompletes to the drive root.
    if text.len() <= 2 && !text.contains('\\') && !text.contains('/') {
        let typed = text.to_lowercase();
        for drive in shell::drives() {
            let root = drive.path.display().to_string();
            if root.to_lowercase().starts_with(&typed) {
                out.push(Suggest { name: drive.title.into(), detail: root.clone().into(), path: root.into() });
            }
        }
    }
    let (parent, prefix) = split_for_suggest(text, cwd);
    let prefix = prefix.to_lowercase();
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    if let Ok(entries) = std::fs::read_dir(&parent) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().starts_with(&prefix) {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let path = entry.path().display().to_string();
            let item = Suggest {
                name: name.into(),
                detail: if is_dir {
                    parent.display().to_string()
                } else {
                    t!("{} · File", parent.display())
                }
                .into(),
                path: path.into(),
            };
            if is_dir {
                dirs.push(item);
            } else {
                files.push(item);
            }
            if dirs.len() + files.len() >= 400 {
                break; // keep a pathological folder from dominating the request
            }
        }
    }
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.extend(dirs);
    out.extend(files);
    let mut seen = HashSet::new();
    out.retain(|s| seen.insert(s.path.to_lowercase()));
    out.truncate(LIMIT);
    out
}

/// Splits typed text into (folder to list, name prefix). A trailing separator lists the
/// folder itself; a bare relative name is resolved against `cwd`.
fn split_for_suggest(text: &str, cwd: &Path) -> (PathBuf, String) {
    if text.ends_with('\\') || text.ends_with('/') {
        return (PathBuf::from(text), String::new());
    }
    if text.len() == 2 && text.ends_with(':') {
        return (PathBuf::from(format!("{text}\\")), String::new());
    }
    let path = Path::new(text);
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            (parent.to_path_buf(), name.to_string_lossy().into_owned())
        }
        (Some(parent), _) if !parent.as_os_str().is_empty() => (parent.to_path_buf(), String::new()),
        _ => (cwd.to_path_buf(), text.to_string()),
    }
}

/// Expands %VARIABLE% references; unknown names stay as typed.
fn expand_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match std::env::var(name) {
            Ok(value) => out.push_str(&value),
            Err(_) => {
                out.push('%');
                out.push_str(name);
                out.push('%');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Sorted display order, without hidden items unless "Hidden items" is on.
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
    let kinds = t.rows.kinds.borrow();
    let dates = t.rows.dates.borrow();
    let sizes = t.rows.sizes.borrow();
    let searching = t.search.borrow().is_some();
    let this_pc = is_this_pc(&t.cwd.borrow()) && !searching;
    let mut name = t!("Name");
    let mut location = t!("Location");
    let mut date = if this_pc { t!("Free space") } else { t!("Date modified") };
    let mut kind = t!("Type");
    let mut size = if this_pc { t!("Total capacity") } else { t!("Size") };
    let (mut name_w, mut location_w) = (text_units(&name), text_units(&location));
    let (mut kind_w, mut size_w, mut date_w) = (text_units(&kind), text_units(&size), text_units(&date));
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
        let k = match kinds.get(i).filter(|s| !s.is_empty()) {
            Some(s) => s.clone(),
            None => kind_of(e),
        };
        let w = text_units(&k);
        if w > kind_w {
            kind_w = w;
            kind = k;
        }
        match dates.get(i).filter(|s| !s.is_empty()) {
            Some(d) => {
                let w = text_units(d);
                if w > date_w {
                    date_w = w;
                    date = d.clone();
                }
            }
            None => has_date |= e.modified > 0,
        }
        match sizes.get(i).filter(|s| !s.is_empty()) {
            Some(s) => {
                let w = text_units(s);
                if w > size_w {
                    size_w = w;
                    size = s.clone();
                }
            }
            None if !e.is_dir() => max_size = Some(max_size.map_or(e.size, |m: u64| m.max(e.size))),
            None => {}
        }
    }
    if has_date && text_units("0000-00-00 00:00") > date_w {
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
/// Builds the search tree for index hits. A node index is also its entry index.
fn tree_from_hits(hits: &[index::Hit], scope: &Scope) -> (Vec<Entry>, SearchTree) {
    let mut entries = Vec::new();
    let mut tree = SearchTree::default();
    let base = scope.root();
    for h in hits {
        tree.insert(&mut entries, &h.path, h.dir, Some(file_entry(&h.path, h.dir)), base);
    }
    (entries, tree)
}

/// Walks a folder for matching names, streaming hits to the tab in batches.
fn walk_search_worker(id: u64, seq: u64, latest: Arc<AtomicU64>, root: PathBuf, query: String, pattern: Pattern) {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let start = Instant::now();
    if !root.is_dir() {
        let note = t!("Folder not found: {}", root.display());
        post(move |a| {
            if let Some(t) = a.find(id) {
                a.set_note(&t, note);
            }
        });
        return;
    }
    let (q, r) = (query.clone(), root.clone());
    let root_entry = file_entry(&root, true);
    post(move |a| a.walk_begin(id, seq, q, r, root_entry));
    let pending = Mutex::new(Vec::<(PathBuf, bool)>::new());
    let (count, done) = (AtomicUsize::new(0), AtomicBool::new(false));
    let flush = || {
        let batch = std::mem::take(&mut *pending.lock().unwrap());
        if !batch.is_empty() {
            let entries = batch.iter().map(|(p, dir)| file_entry(p, *dir)).collect();
            post(move |a| a.name_batch(id, seq, batch, entries));
        }
    };
    let r = thread::scope(|s| {
        // Show hits within ~0.1 s even when they are sparse.
        s.spawn(|| {
            while !done.load(SeqCst) {
                thread::sleep(Duration::from_millis(100));
                flush();
            }
        });
        let r = search::walk_names(&root, &pattern, |path, dir| {
            if latest.load(SeqCst) != seq {
                return false;
            }
            pending.lock().unwrap().push((path, dir));
            count.fetch_add(1, SeqCst) + 1 < SEARCH_LIMIT
        });
        done.store(true, SeqCst);
        r
    });
    flush();
    let (took, capped) = (start.elapsed(), count.load(SeqCst) >= SEARCH_LIMIT);
    post(move |a| a.walk_done(id, seq, took, capped, r.err()));
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
    if is_this_pc(path) {
        return vec![Crumb { title: t!("This PC").into(), path: THIS_PC.into() }];
    }
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

fn view_file() -> Option<PathBuf> {
    shell::roaming_appdata().ok().map(|d| d.join("Fast Explorer").join("view.txt"))
}

/// The folder a new window/tab opens ("" = C:\), kept in `view.txt` as `home=`.
static HOME: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// UI language choice kept in `view.txt` as `lang=` ("" = system, "en", "ko").
static LANG: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Reads `lang=` from `view.txt` and applies it (an explicit `FEX_LANG` wins).
fn apply_saved_lang() {
    let text = view_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
    let lang = text.lines().find_map(|l| l.trim().strip_prefix("lang=")).unwrap_or("").trim().to_string();
    if std::env::var_os("FEX_LANG").is_none() {
        fex_core::i18n::set_lang(&lang);
    }
    *LANG.lock().unwrap() = lang;
}

/// The configured default folder, or `C:\` when unset or no longer there.
fn home_path() -> PathBuf {
    let home = HOME.lock().map(|h| h.clone()).unwrap_or_default();
    if is_this_pc_name(&home) {
        return PathBuf::from(THIS_PC);
    }
    let p = PathBuf::from(&home);
    if !home.is_empty() && p.is_dir() { p } else { PathBuf::from(r"C:\") }
}

/// The view options saved by the last session. The first run takes Explorer's own settings.
fn load_view() -> (bool, bool) {
    let (mut hidden, mut ext) = shell::explorer_view();
    let Some(text) = view_file().and_then(|f| std::fs::read_to_string(f).ok()) else {
        return (hidden, ext);
    };
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else { continue };
        if key.trim() == "lang" {
            continue;
        }
        if key.trim() == "home" {
            *HOME.lock().unwrap() = value.trim().to_string();
            continue;
        }
        let on = match value.trim() {
            "1" => true,
            "0" => false,
            _ => continue,
        };
        match key.trim() {
            "hidden" => hidden = on,
            "ext" => ext = on,
            "tree" => TREE_VIEW.store(on, Relaxed),
            _ => {}
        }
    }
    (hidden, ext)
}

fn save_view() {
    let Some(file) = view_file() else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let home = HOME.lock().map(|h| h.clone()).unwrap_or_default();
    let lang = LANG.lock().map(|l| l.clone()).unwrap_or_default();
    let text = format!(
        "hidden={}\r\next={}\r\ntree={}\r\nlang={lang}\r\nhome={home}",
        SHOW_HIDDEN.load(Relaxed) as u8,
        SHOW_EXT.load(Relaxed) as u8,
        TREE_VIEW.load(Relaxed) as u8
    );
    let _ = std::fs::write(file, text);
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
    if msg == WM_DEVICECHANGE {
        post(|a| a.devices_changed());
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

/// Release builds have no console, so a panic would end the app silently: record it.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = shell::roaming_appdata().ok().map(|d| d.join("Fast Explorer")) {
            use std::io::Write;
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
                let _ = writeln!(f, "{info}\n{}\n", std::backtrace::Backtrace::force_capture());
            }
        }
        default(info);
    }));
}

fn main() -> Result<(), slint::PlatformError> {
    log_panics();
    apply_saved_lang();
    let ui = AppWindow::new()?;
    if fex_core::i18n::korean() {
        let _ = slint::select_bundled_translation("ko");
    }
    let tab_model = Rc::new(VecModel::default());
    ui.set_tabs(ModelRc::from(tab_model.clone()));

    let exe = std::env::current_exe().unwrap_or_default().with_file_name("fex-shell-host.exe");
    let host = Arc::new(MenuHost::new(exe, |_, r| {
        if let Err(e) = r {
            post(move |a| a.set_note(&a.tab(), t!("Launch failed: {}", e)));
        }
    }));
    host.warm_up(); // loading shell extensions takes seconds the first time; do it now

    let (show_hidden, show_ext) = load_view();
    SHOW_HIDDEN.store(show_hidden, Relaxed);
    SHOW_EXT.store(show_ext, Relaxed);
    ui.set_show_hidden(show_hidden);
    ui.set_show_ext(show_ext);
    ui.set_tree_view(TREE_VIEW.load(Relaxed));
    ui.set_home_path(HOME.lock().unwrap().clone().into());
    ui.set_lang_index(match LANG.lock().unwrap().as_str() {
        "en" => 1,
        "ko" => 2,
        _ => 0,
    });
    ui.set_app_version(update::CURRENT.into());
    ui.set_auto_update(update::auto_enabled());
    ui.set_this_pc_path(THIS_PC.into());
    start_icon_worker();

    let desktop = shell::desktop().ok();
    let downloads = shell::downloads().ok();
    let favorites = load_favorites(desktop.as_deref(), downloads.as_deref());
    let drives = shell::drives();

    APP.with(|a| {
        let _ = a.set(App {
            ui: ui.as_weak(),
            tabs: RefCell::default(),
            closed: RefCell::default(),
            tab_model,
            active: Cell::new(0),
            next_id: Cell::new(0),
            typeahead: RefCell::default(),
            hint_seq: Cell::new(0),
            undo: RefCell::default(),
            host,
            menu_seq: Cell::new(0),
            prefetched: RefCell::default(),
            menu_busy: Cell::new(false),
            search_seq: Arc::default(),
            install_offered: Cell::new(false),
            favorites: RefCell::new(favorites),
            desktop,
            downloads,
            drives: RefCell::new(drives),
            devices_due: Cell::new(false),
            suggest_seq: Arc::default(),
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
    ui.on_search_options_changed(|| app(|a| a.search_options_changed()));
    ui.on_row_expand(|r| app(|a| a.toggle_expand(r as usize)));
    ui.on_back(|| app(|a| a.back()));
    ui.on_forward(|| app(|a| a.forward()));
    ui.on_up(|| app(|a| a.up()));
    ui.on_refresh(|| app(|a| a.refresh()));
    ui.on_tab_select(|i| app(|a| a.select_tab(i as usize, true)));
    ui.on_tab_close(|i| app(|a| a.close_tab(i as usize)));
    ui.on_tab_new(|| app(|a| a.new_tab(home_path())));
    ui.on_home_changed(|text| {
        *HOME.lock().unwrap() = text.trim().to_string();
        save_view();
    });
    ui.on_home_use_current(|| app(|a| {
        let cwd = a.tab().cwd.borrow().display().to_string();
        *HOME.lock().unwrap() = cwd.clone();
        save_view();
        a.ui().set_home_path(cwd.into());
    }));
    ui.on_tab_move(|from, to| app(|a| a.move_tab(from as usize, to as usize)));
    ui.on_favorite_toggle(|| app(|a| a.toggle_favorite()));
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
    ui.on_place_open_tab(|p| app(|a| a.open_place_in_tab(&p)));
    ui.on_place_add(|| app(|a| a.add_favorite()));
    ui.on_place_remove(|p| app(|a| a.remove_favorite(&p)));
    ui.on_drive_menu(|p| {
        let at = cursor_pos();
        slint::Timer::single_shot(Duration::from_millis(30), move || app(|a| a.drive_menu(&p, at)));
    });
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
    ui.on_auto_update_changed(|| app(|a| update::set_auto(a.ui().get_auto_update())));
    ui.on_lang_changed(|i| app(|a| a.set_language(i)));
    ui.on_check_update(|| app(|a| a.check_update()));
    ui.on_suggest(|t| app(|a| a.suggest(&t)));
    ui.on_suggest_cancel(|| app(|a| a.cancel_suggest()));
    ui.on_key(|e, in_list| {
        app(|a| a.key(&e.text, e.modifiers.control, e.modifiers.shift, e.modifiers.alt, in_list))
    });

    let session = if std::env::args().nth(1).is_none() { update::take_session() } else { None };
    if let Some((active, tabs)) = session {
        for t in tabs {
            app(|a| a.new_tab(t));
        }
        app(|a| a.select_tab(active, true));
    } else {
        let start = match std::env::args().nth(1) {
            Some(s) if is_this_pc_name(&s) => PathBuf::from(THIS_PC),
            Some(s) => PathBuf::from(s),
            None => home_path(),
        };
        app(|a| a.new_tab(start));
    }
    if update::auto_due() {
        app(|a| a.check_update());
    }
    ui.run()
}

#[cfg(test)]
mod tests {
    use fex_core::t;

    #[test]
    fn crumbs_from_the_drive_root() {
        let crumbs = super::crumbs_for(std::path::Path::new(r"C:\Users\hp\source"));
        let titles: Vec<String> = crumbs.iter().map(|c| c.title.to_string()).collect();
        let paths: Vec<String> = crumbs.iter().map(|c| c.path.to_string()).collect();
        assert_eq!(titles, ["C:", "Users", "hp", "source"]);
        assert_eq!(paths, [r"C:\", r"C:\Users", r"C:\Users\hp", r"C:\Users\hp\source"]);
    }

    #[test]
    fn this_pc_names_and_crumbs() {
        for name in ["내 PC", "내PC", "this pc", "THIS PC", super::THIS_PC] {
            assert!(super::is_this_pc_name(name), "{name}");
        }
        assert!(!super::is_this_pc_name(r"C:\"));
        assert!(!super::is_this_pc_name("내 PCX"));

        let crumbs = super::crumbs_for(std::path::Path::new(super::THIS_PC));
        assert_eq!(crumbs.len(), 1);
        assert_eq!(crumbs[0].title.as_str(), t!("This PC"));
        assert_eq!(crumbs[0].path, super::THIS_PC);
    }

    #[test]
    fn capacity_strings() {
        assert_eq!(super::format_capacity(0), "0 KB");
        assert_eq!(super::format_capacity(512 << 20), "512 MB");
        assert_eq!(super::format_capacity(1 << 30), "1.00 GB");
        assert_eq!(super::format_capacity(100 << 30), "100 GB");
        assert_eq!(super::format_capacity(1 << 40), "1.00 TB");
    }

    #[test]
    fn groups_folders_before_drives() {
        let kinds: Vec<String> = [t!("File folder"), t!("Local Disk"), t!("File folder"), t!("Removable Disk")]
            .to_vec();
        let mut order = vec![1, 3, 0, 2];
        super::group_this_pc_order(&mut order, &kinds);
        assert_eq!(order, [0, 2, 1, 3]); // the order inside each group is preserved
    }

    #[test]
    fn splits_suggestion_input() {
        let cwd = std::path::Path::new(r"C:\Users\hp");
        assert_eq!(
            super::split_for_suggest(r"C:\Win", cwd),
            (std::path::PathBuf::from(r"C:\"), "Win".to_string())
        );
        assert_eq!(
            super::split_for_suggest(r"C:\Windows\", cwd),
            (std::path::PathBuf::from(r"C:\Windows\"), String::new())
        );
        assert_eq!(
            super::split_for_suggest("Doc", cwd),
            (std::path::PathBuf::from(cwd), "Doc".to_string())
        );
        assert_eq!(
            super::split_for_suggest("C:", cwd),
            (std::path::PathBuf::from(r"C:\"), String::new())
        );
    }

    #[test]
    fn suggests_children_and_expands_environment() {
        let dir = std::env::temp_dir().join(format!("fex-suggest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::write(dir.join("alpine.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("beta")).unwrap();

        let typed = format!(r"{}\al", dir.display());
        let items = super::suggest_paths(&typed, std::path::Path::new(r"C:\"));
        let names: Vec<String> = items.iter().map(|s| s.name.to_string()).collect();
        assert!(names.contains(&"alpha".to_string()), "{names:?}");
        assert!(names.contains(&"alpine.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"beta".to_string()), "{names:?}");

        std::fs::remove_dir_all(&dir).unwrap();
        if let Ok(temp) = std::env::var("TEMP") {
            assert_eq!(super::expand_env(r"%TEMP%\x"), format!(r"{temp}\x"));
        }
        assert_eq!(super::expand_env("%NO_SUCH_VAR_FEX%"), "%NO_SUCH_VAR_FEX%");
        assert_eq!(super::expand_env("100%"), "100%");
    }

    #[test]
    fn search_option_defaults_and_service_pattern() {
        let defaults = super::SearchOptions::default();
        assert!(defaults.folders && defaults.files && !defaults.regex && !defaults.tree);
        assert_eq!(super::service_pattern("*.txt", false), "*.txt");
        assert_eq!(super::service_pattern("^a", true), "re:^a");
        assert_eq!(super::service_pattern("re:^a", true), "re:^a");
    }
}
