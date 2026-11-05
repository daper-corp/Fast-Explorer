#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::{Cell, OnceCell, RefCell};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::thread;
use std::time::{Duration, Instant};

use fex_core::{Entry, SortKey, format_filetime, format_size, list_dir, sort_order};
use slint::{Model, ModelNotify, ModelRc, ModelTracker, SharedString, StandardListViewItem, VecModel};

slint::slint! {
    import { StandardTableView, LineEdit, Button, HorizontalBox, VerticalBox } from "std-widgets.slint";

    export component AppWindow inherits Window {
        title: "Fex";
        preferred-width: 1000px;
        preferred-height: 700px;
        forward-focus: table;

        in-out property <string> path;
        in-out property <int> current-row: -1;
        in property <string> status;
        in property <[[StandardListViewItem]]> rows;

        callback navigate(string);
        callback up();
        callback sort(int, bool);
        callback row-down(int);
        callback activate(int);

        VerticalBox {
            HorizontalBox {
                padding: 0;
                Button { text: "↑"; clicked => { root.up(); } }
                LineEdit { text <=> root.path; accepted(t) => { root.navigate(t); } }
            }
            FocusScope {
                vertical-stretch: 1;
                key-pressed(e) => {
                    if (e.text == Key.Return) { root.activate(root.current-row); return accept; }
                    if (e.text == Key.Backspace) { root.up(); return accept; }
                    reject
                }
                table := StandardTableView {
                    width: 100%;
                    height: 100%;
                    columns: [
                        { title: "이름", width: 420px },
                        { title: "수정한 날짜", width: 150px },
                        { title: "유형", width: 120px },
                        { title: "크기", width: 110px },
                    ];
                    rows: root.rows;
                    current-row <=> root.current-row;
                    sort-ascending(c) => { root.sort(c, false); }
                    sort-descending(c) => { root.sort(c, true); }
                    row-pointer-event(row, e, pos) => {
                        if (e.kind == PointerEventKind.down && e.button == PointerEventButton.left) {
                            root.row-down(row);
                        }
                    }
                }
            }
            Text { text: root.status; }
        }
    }
}

/// Virtualized table model: only rows Slint asks for (the visible ones) get formatted.
#[derive(Default)]
struct Rows {
    entries: RefCell<Arc<Vec<Entry>>>,
    order: RefCell<Vec<u32>>,
    notify: ModelNotify,
}

impl Model for Rows {
    type Data = ModelRc<StandardListViewItem>;

    fn row_count(&self) -> usize {
        self.order.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let i = *self.order.borrow().get(row)? as usize;
        let entries = self.entries.borrow();
        let e = &entries[i];
        let kind = match (e.is_dir(), e.extension()) {
            (true, _) => "파일 폴더".to_string(),
            (false, x) if x.is_empty() => "파일".to_string(),
            (false, x) => format!("{} 파일", x.to_uppercase()),
        };
        let size = if e.is_dir() { String::new() } else { format_size(e.size) };
        let cells = [e.name(), format_filetime(e.modified), kind, size]
            .map(|s| StandardListViewItem::from(SharedString::from(s)));
        Some(ModelRc::new(VecModel::from(cells.to_vec())))
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

/// UI-thread state. Workers report back through `invoke_from_event_loop` + `app()`.
struct App {
    ui: slint::Weak<AppWindow>,
    rows: std::rc::Rc<Rows>,
    cwd: RefCell<PathBuf>,
    /// Bumped on every navigation; workers for an older folder stop and their results are dropped.
    nav_gen: Arc<AtomicU64>,
    /// Bumped on every navigation and sort request; only the newest sort result is applied.
    sort_gen: Cell<u64>,
    sort: Cell<(SortKey, bool)>,
    list_time: Cell<Duration>,
    last_click: Cell<Option<(i32, Instant)>>,
}

thread_local! {
    static APP: OnceCell<App> = const { OnceCell::new() };
}

fn app(f: impl FnOnce(&App)) {
    APP.with(|a| f(a.get().expect("app initialized")));
}

impl App {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window alive")
    }

    fn navigate(&self, path: PathBuf) {
        let nav = self.nav_gen.fetch_add(1, SeqCst) + 1;
        self.sort_gen.set(self.sort_gen.get() + 1);
        *self.rows.entries.borrow_mut() = Arc::default();
        self.rows.order.borrow_mut().clear();
        self.rows.notify.reset();
        let ui = self.ui();
        ui.set_path(path.display().to_string().into());
        ui.set_current_row(-1);
        ui.set_status("불러오는 중…".into());
        *self.cwd.borrow_mut() = path.clone();

        let current = self.nav_gen.clone();
        thread::spawn(move || {
            let t = Instant::now();
            let res = list_dir(&path, |batch| {
                if current.load(SeqCst) != nav {
                    return false;
                }
                let _ = slint::invoke_from_event_loop(move || app(|a| a.append(nav, batch)));
                true
            });
            let elapsed = t.elapsed();
            let err = res.err().map(|e| e.to_string());
            let _ = slint::invoke_from_event_loop(move || app(|a| a.listed(nav, elapsed, err)));
        });
    }

    fn append(&self, nav: u64, batch: Vec<Entry>) {
        if nav != self.nav_gen.load(SeqCst) {
            return;
        }
        let (start, n) = {
            let mut entries = self.rows.entries.borrow_mut();
            let start = entries.len();
            Arc::make_mut(&mut entries).extend(batch);
            let end = entries.len();
            self.rows.order.borrow_mut().extend(start as u32..end as u32);
            (start, end - start)
        };
        self.rows.notify.row_added(start, n);
    }

    fn listed(&self, nav: u64, elapsed: Duration, err: Option<String>) {
        if nav != self.nav_gen.load(SeqCst) {
            return;
        }
        if let Some(e) = err {
            self.ui().set_status(format!("열 수 없음: {e}").into());
            return;
        }
        self.list_time.set(elapsed);
        self.resort();
    }

    /// Sorts on a worker thread, then swaps in the new order.
    fn resort(&self) {
        let seq = self.sort_gen.get() + 1;
        self.sort_gen.set(seq);
        let entries = self.rows.entries.borrow().clone();
        let (key, desc) = self.sort.get();
        thread::spawn(move || {
            let t = Instant::now();
            let order = sort_order(&entries, key, desc);
            let elapsed = t.elapsed();
            let _ = slint::invoke_from_event_loop(move || app(|a| a.sorted(seq, order, elapsed)));
        });
    }

    fn sorted(&self, seq: u64, order: Vec<u32>, elapsed: Duration) {
        // Stale if the user navigated/sorted again, or more entries arrived meanwhile.
        if seq != self.sort_gen.get() || order.len() != self.rows.entries.borrow().len() {
            return;
        }
        *self.rows.order.borrow_mut() = order;
        self.rows.notify.reset();
        let n = self.rows.row_count();
        self.ui().set_status(
            format!(
                "{n}개 항목 · 목록 {:.1} ms · 정렬 {:.1} ms",
                self.list_time.get().as_secs_f64() * 1e3,
                elapsed.as_secs_f64() * 1e3
            )
            .into(),
        );
    }

    fn row_down(&self, row: i32) {
        let now = Instant::now();
        let double = matches!(self.last_click.get(),
            Some((r, t)) if r == row && now - t < Duration::from_millis(500));
        self.last_click.set(if double { None } else { Some((row, now)) });
        if double {
            self.activate(row);
        }
    }

    fn activate(&self, row: i32) {
        let target = {
            let order = self.rows.order.borrow();
            let entries = self.rows.entries.borrow();
            order
                .get(row as usize)
                .map(|&i| &entries[i as usize])
                .filter(|e| e.is_dir())
                .map(|e| self.cwd.borrow().join(e.name()))
        };
        // ponytail: files don't open yet; ShellExecute arrives with file operations (stage 2).
        if let Some(path) = target {
            self.navigate(path);
        }
    }

    fn up(&self) {
        let parent = self.cwd.borrow().parent().map(PathBuf::from);
        if let Some(p) = parent {
            self.navigate(p);
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let rows = std::rc::Rc::new(Rows::default());
    ui.set_rows(ModelRc::from(rows.clone()));

    APP.with(|a| {
        let _ = a.set(App {
            ui: ui.as_weak(),
            rows,
            cwd: RefCell::default(),
            nav_gen: Arc::default(),
            sort_gen: Cell::new(0),
            sort: Cell::new((SortKey::Name, false)),
            list_time: Cell::default(),
            last_click: Cell::new(None),
        });
    });

    ui.on_navigate(|t| app(|a| a.navigate(PathBuf::from(t.trim()))));
    ui.on_up(|| app(|a| a.up()));
    ui.on_row_down(|r| app(|a| a.row_down(r)));
    ui.on_activate(|r| app(|a| a.activate(r)));
    ui.on_sort(|col, desc| {
        app(|a| {
            let key = [SortKey::Name, SortKey::Modified, SortKey::Type, SortKey::Size][col as usize];
            a.sort.set((key, desc));
            a.resort();
        })
    });

    let start = std::env::args().nth(1).unwrap_or_else(|| r"C:\".into());
    app(|a| a.navigate(PathBuf::from(start)));
    ui.run()
}
