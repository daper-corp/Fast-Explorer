#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::thread;
use std::time::{Duration, Instant};

use fex_core::shell::{self, Op};
use fex_core::{Entry, SortKey, format_filetime, format_size, list_dir, sort_order};
use slint::platform::Key;
use slint::{ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, SharedString, VecModel};

slint::include_modules!();

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
    notify: ModelNotify,
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
        let name = e.name();
        let stem = match name.rfind('.') {
            Some(p) if p > 0 && !e.is_dir() => p,
            _ => name.len(),
        };
        let kind = match (e.is_dir(), e.extension()) {
            (true, _) => "파일 폴더".to_string(),
            (false, x) if x.is_empty() => "파일".to_string(),
            (false, x) => format!("{} 파일", x.to_uppercase()),
        };
        Some(Row {
            date: format_filetime(e.modified).into(),
            kind: kind.into(),
            size: if e.is_dir() { SharedString::new() } else { format_size(e.size).into() },
            selected: self.selected.borrow()[i as usize],
            editing: self.editing.get() == Some(i),
            stem: stem as i32,
            name: name.into(),
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
        self.editing.set(None);
        self.notify.reset();
    }

    /// Selected entry indices in display order.
    fn selection(&self) -> Vec<u32> {
        let sel = self.selected.borrow();
        self.order.borrow().iter().copied().filter(|&i| sel[i as usize]).collect()
    }

    fn select_only(&self, idx: Option<u32>) {
        let mut sel = self.selected.borrow_mut();
        sel.fill(false);
        if let Some(i) = idx {
            sel[i as usize] = true;
        }
    }

    fn select_range(&self, a: usize, b: usize, add: bool) {
        let order = self.order.borrow();
        let mut sel = self.selected.borrow_mut();
        if !add {
            sel.fill(false);
        }
        for r in a.min(b)..=a.max(b) {
            sel[order[r] as usize] = true;
        }
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
        }
    }

    fn path_of(&self, idx: u32) -> PathBuf {
        self.cwd.borrow().join(self.rows.name(idx))
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
        ui.set_list_y(t.scroll.get());
        ui.invoke_focus_list();
    }

    fn close_tab(&self, i: usize) {
        let n = self.tabs.borrow().len();
        if i >= n {
            return;
        }
        if n == 1 {
            let _ = self.ui().hide();
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
        ui.set_path(t.cwd.borrow().display().to_string().into());
        ui.set_folder(t.title().into());
        let (pos, len) = (t.hist_pos.get(), t.history.borrow().len());
        ui.set_can_back(pos > 0);
        ui.set_can_forward(pos + 1 < len);
        let (key, desc) = t.sort.get();
        ui.set_sort_column(SORT_KEYS.iter().position(|&k| k == key).unwrap_or(0) as i32);
        ui.set_sort_desc(desc);
        self.sync_tabs();
        self.update_status(t);
    }

    fn update_status(&self, t: &Tab) {
        if !self.is_active(t) {
            return;
        }
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
        self.ui().set_status(s.into());
    }

    fn set_note(&self, t: &Tab, note: String) {
        *t.note.borrow_mut() = note;
        self.update_status(t);
    }

    // ---------- loading ----------

    /// Lists `path` on a worker and streams it in. The old listing stays until the first batch
    /// arrives, so an unreadable folder leaves the tab where it was. A file path is opened instead.
    fn navigate(&self, t: &Rc<Tab>, path: PathBuf, hist: Hist, after: Option<After>) {
        let epoch = t.epoch.fetch_add(1, SeqCst) + 1;
        *t.pending.borrow_mut() = Some((path.clone(), hist, after));
        self.set_note(t, "불러오는 중…".into());
        let (id, cancel) = (t.id, t.epoch.clone());
        thread::spawn(move || {
            if path.is_file() {
                let r = shell::open(&path);
                return post(move |a| a.opened(id, epoch, r));
            }
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

    fn opened(&self, id: u64, epoch: u64, r: io::Result<()>) {
        let Some(t) = self.current(id, epoch) else { return };
        t.pending.take();
        self.sync_view(&t);
        self.set_note(&t, r.err().map(|e| format!("열 수 없음: {e}")).unwrap_or_default());
    }

    /// Makes a pending navigation the tab's current folder.
    fn commit(&self, t: &Rc<Tab>) {
        let Some((path, hist, after)) = t.pending.take() else { return };
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
            rows.order.borrow_mut().extend(start as u32..end as u32);
            rows.selected.borrow_mut().resize(end, false);
            (start, end - start)
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
        self.resort(&t);
    }

    /// Sorts on a worker, then swaps in the new order.
    fn resort(&self, t: &Rc<Tab>) {
        t.bump_sort();
        let seq = t.sort_gen.get();
        let entries = t.rows.entries.borrow().clone();
        let (key, desc) = t.sort.get();
        let id = t.id;
        thread::spawn(move || {
            let start = Instant::now();
            let order = sort_order(&entries, key, desc);
            let took = start.elapsed();
            post(move |a| a.sorted(id, seq, order, took));
        });
    }

    fn sorted(&self, id: u64, seq: u64, order: Vec<u32>, took: Duration) {
        let Some(t) = self.find(id) else { return };
        if seq != t.sort_gen.get() || order.len() != t.rows.entries.borrow().len() {
            return;
        }
        *t.rows.order.borrow_mut() = order;
        *t.note.borrow_mut() = format!("목록 {:.1} ms · 정렬 {:.1} ms", t.list_ms.get(), ms(took));
        self.apply_after(&t);
        t.rows.notify.reset();
        self.reveal_cursor(&t);
        self.update_status(&t);
    }

    /// Re-lists the current folder in one piece (no flicker), keeping the selection by name
    /// unless `after` names what to select.
    fn reload(&self, t: &Rc<Tab>, after: After) {
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
            let order = sort_order(&all, key, desc);
            post(move |a| a.reloaded(id, epoch, all, order, took, res.err()));
        });
    }

    fn reloaded(&self, id: u64, epoch: u64, all: Vec<Entry>, order: Vec<u32>, took: Duration, err: Option<io::Error>) {
        let Some(t) = self.current(id, epoch) else { return };
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
        self.update_status(&t);
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
        if shift {
            let anchor = t.anchor.get().and_then(|a| t.rows.row_of(a)).unwrap_or(row);
            t.rows.select_range(anchor, row, ctrl);
        } else if ctrl {
            let mut sel = t.rows.selected.borrow_mut();
            sel[idx as usize] = !sel[idx as usize];
            t.anchor.set(Some(idx));
        } else {
            t.rows.select_only(Some(idx));
            t.anchor.set(Some(idx));
        }
        t.cursor.set(Some(idx));
        t.rows.notify.reset();
        self.update_status(&t);
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
        t.rows.notify.reset();
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
            _ if is(Key::F5) || (ctrl && ch == 'r') => self.reload(&t, After::default()),
            _ if is(Key::Escape) && !in_list => {
                self.sync_view(&t);
                self.ui().invoke_focus_list();
            }
            _ if !in_list => return false,

            // File list.
            _ if is(Key::Return) => self.open_selection(),
            _ if is(Key::Backspace) => self.back(),
            _ if ctrl && shift && ch == 'n' => self.new_folder(),
            _ if ctrl && ch == 'a' => {
                t.rows.selected.borrow_mut().fill(true);
                t.rows.notify.reset();
                self.update_status(&t);
            }
            _ if ctrl && ch == 'c' => self.copy(false),
            _ if ctrl && ch == 'x' => self.copy(true),
            _ if ctrl && ch == 'v' => self.paste(),
            _ if is(Key::Delete) => self.delete(shift),
            _ if is(Key::F2) => self.start_rename(),
            _ if ctrl && ch == ' ' => {
                if let Some(c) = t.cursor.get() {
                    let mut sel = t.rows.selected.borrow_mut();
                    sel[c as usize] = !sel[c as usize];
                }
                t.rows.notify.reset();
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
            let after = After { select: vec![name.to_string_lossy().into_owned()], rename: false };
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
        self.navigate(&t, p, Hist::Push, None);
        self.ui().invoke_focus_list();
    }

    fn sort(&self, col: usize) {
        let t = self.tab();
        let key = SORT_KEYS[col.min(3)];
        let (cur, desc) = t.sort.get();
        t.sort.set((key, cur == key && !desc));
        self.sync_view(&t);
        self.resort(&t);
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
        self.run_op(&t, After { select: names, rename: false }, move |hwnd| {
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
        let (old, new) = (t.rows.name(idx), text.trim());
        if commit && !new.is_empty() && new != old {
            let (path, new) = (t.path_of(idx), new.to_string());
            let after = After { select: vec![new.clone()], rename: false };
            self.run_op(&t, after, move |hwnd| shell::rename(&path, &new, Some(hwnd)));
        }
    }

    fn new_folder(&self) {
        let t = self.tab();
        let (id, dir) = (t.id, t.cwd.borrow().clone());
        thread::spawn(move || {
            let r = shell::new_folder(&dir);
            post(move |a| {
                let Some(t) = a.find(id) else { return };
                match r {
                    Ok(name) => a.reload(&t, After { select: vec![name], rename: true }),
                    Err(e) => a.set_note(&t, format!("새 폴더 실패: {e}")),
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

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let tab_model = Rc::new(VecModel::default());
    ui.set_tabs(ModelRc::from(tab_model.clone()));

    APP.with(|a| {
        let _ = a.set(App {
            ui: ui.as_weak(),
            tabs: RefCell::default(),
            tab_model,
            active: Cell::new(0),
            next_id: Cell::new(0),
            typeahead: RefCell::default(),
        });
    });

    ui.on_navigate(|t| app(|a| a.address(&t)));
    ui.on_back(|| app(|a| a.back()));
    ui.on_forward(|| app(|a| a.forward()));
    ui.on_up(|| app(|a| a.up()));
    ui.on_tab_select(|i| app(|a| a.select_tab(i as usize, true)));
    ui.on_tab_close(|i| app(|a| a.close_tab(i as usize)));
    ui.on_tab_new(|| app(|a| a.new_tab(a.tab().cwd.borrow().clone())));
    ui.on_sort(|c| app(|a| a.sort(c as usize)));
    ui.on_row_pressed(|r, m| app(|a| a.row_pressed(r as usize, m.control, m.shift)));
    ui.on_row_open(|r| {
        app(|a| {
            let t = a.tab();
            if let Some(i) = t.rows.entry(r as usize) {
                a.open_rows(&t, vec![i]);
            }
        })
    });
    ui.on_rename_done(|text, commit| app(|a| a.rename_done(&text, commit)));
    ui.on_key(|e, in_list| {
        app(|a| a.key(&e.text, e.modifiers.control, e.modifiers.shift, e.modifiers.alt, in_list))
    });

    let start = std::env::args().nth(1).unwrap_or_else(|| r"C:\".into());
    app(|a| a.new_tab(PathBuf::from(start)));
    ui.run()
}
