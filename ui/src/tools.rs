//! Built-in counterparts of two PowerToys helpers: batch rename (PowerRename) and
//! "which process is using this?" (File Locksmith).

use super::*;
use fex_core::t;
use fex_core::locks::{self, Locker};
use fex_core::rename::{self, Case, Part, Planned, Rule, Status};

thread_local! {
    static RENAME: RefCell<Option<RenameWindow>> = const { RefCell::new(None) };
    static LOCKS: RefCell<Option<LocksUi>> = const { RefCell::new(None) };
}

fn lower(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

// ---------- batch rename ----------

struct RenameState {
    tab: u64,
    items: Vec<(PathBuf, bool)>,
    /// Lowercased paths present in the folders involved, for clash detection.
    taken: HashSet<String>,
    plan: Vec<Planned>,
}

fn rule_of(w: &RenameWindow) -> Rule {
    Rule {
        find: w.get_find().to_string(),
        replace: w.get_replace().to_string(),
        regex: w.get_use_regex(),
        match_case: w.get_match_case(),
        all: w.get_replace_all(),
        part: match w.get_part() {
            1 => Part::Ext,
            2 => Part::Full,
            _ => Part::Stem,
        },
        case: match w.get_case_mode() {
            1 => Case::Lower,
            2 => Case::Upper,
            _ => Case::Keep,
        },
    }
}

fn refresh_preview(w: &RenameWindow, st: &RefCell<RenameState>) {
    let rule = rule_of(w);
    let mut st = st.borrow_mut();
    let plan = rename::plan(&rule, &st.items, |p| st.taken.contains(&lower(p)));
    match plan {
        Err(e) => {
            w.set_error(e.into());
            w.set_rows(ModelRc::default());
            w.set_summary("".into());
            w.set_can_apply(false);
            st.plan.clear();
        }
        Ok(plan) => {
            let rows: Vec<RenameRow> = plan
                .iter()
                .map(|p| {
                    let old = p.from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let problem = match &p.status {
                        Status::Error(e) => e.clone(),
                        _ => String::new(),
                    };
                    RenameRow {
                        changed: p.status != Status::Unchanged,
                        old: old.into(),
                        new: p.new_name.clone().into(),
                        problem: problem.into(),
                    }
                })
                .collect();
            let ok = plan.iter().filter(|p| p.status == Status::Ok).count();
            let bad = plan.iter().filter(|p| matches!(p.status, Status::Error(_))).count();
            let mut summary = t!("{} total, {} changed", plan.len(), ok);
            if bad > 0 {
                summary.push_str(&t!(" · {} with problems skipped", bad));
            }
            w.set_summary(summary.into());
            w.set_error("".into());
            w.set_can_apply(ok > 0);
            w.set_rows(ModelRc::new(VecModel::from(rows)));
            st.plan = plan;
        }
    }
}

/// Renames `(path, new name)` pairs; returns the `(from, to)` pairs done and the errors.
/// When a new name is another item's old name (a→b, b→c), items go through temporary names.
pub(super) fn apply_renames(pairs: &[(PathBuf, String)], hwnd: isize) -> (Vec<(PathBuf, PathBuf)>, Vec<String>) {
    let (mut done, mut errors) = (Vec::new(), Vec::new());
    let sources: HashSet<String> = pairs.iter().map(|(f, _)| lower(f)).collect();
    let chained = pairs.iter().any(|(f, n)| {
        let t = lower(&f.with_file_name(n));
        t != lower(f) && sources.contains(&t)
    });
    let name_of = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let fail = |errors: &mut Vec<String>, p: &Path, e: &dyn std::fmt::Display| errors.push(format!("{}: {e}", name_of(p)));
    if !chained {
        for (from, new) in pairs {
            match shell::rename(from, new, Some(hwnd)) {
                Ok(true) => done.push((from.clone(), from.with_file_name(new))),
                Ok(false) => {}
                Err(e) => fail(&mut errors, from, &e),
            }
        }
        return (done, errors);
    }
    let pid = std::process::id();
    let mut staged = Vec::new();
    for (i, (from, new)) in pairs.iter().enumerate() {
        let temp = format!(".fexren-{pid}-{i}");
        match shell::rename(from, &temp, Some(hwnd)) {
            Ok(true) => staged.push((from.clone(), from.with_file_name(&temp), new.clone())),
            Ok(false) => {}
            Err(e) => fail(&mut errors, from, &e),
        }
    }
    for (from, temp, new) in staged {
        match shell::rename(&temp, &new, Some(hwnd)) {
            Ok(true) => done.push((from.clone(), from.with_file_name(&new))),
            r => {
                if let Err(e) = r {
                    fail(&mut errors, &from, &e);
                }
                let _ = shell::rename(&temp, &name_of(&from), Some(hwnd)); // put the old name back
            }
        }
    }
    (done, errors)
}

impl App {
    /// Shift+F2: opens the batch rename dialog for the selection.
    pub(super) fn rename_many(&self) {
        let t = self.tab();
        if at_this_pc(&t) {
            return self.set_note(&t, t!("Cannot rename items in This PC"));
        }
        if self.includes_tree_container(&t) {
            return self.set_note(&t, t!("Folder structure rows cannot be renamed"));
        }
        let mut sel = t.rows.selection();
        if sel.is_empty() {
            sel.extend(t.cursor.get());
        }
        if sel.is_empty() {
            return;
        }
        let items: Vec<(PathBuf, bool)> = sel.into_iter().map(|i| (t.path_of(i), t.rows.is_dir(i))).collect();
        // ponytail: one directory read per folder on the UI thread to spot name clashes in the
        // preview; move to a worker if network folders make opening the dialog sluggish.
        let parents: HashSet<PathBuf> = items.iter().filter_map(|(p, _)| p.parent().map(Path::to_path_buf)).collect();
        let taken: HashSet<String> = parents
            .iter()
            .filter_map(|d| std::fs::read_dir(d).ok().map(|rd| (d, rd)))
            .flat_map(|(d, rd)| rd.flatten().map(|e| lower(&d.join(e.file_name()))).collect::<Vec<_>>())
            .collect();
        let Ok(win) = RenameWindow::new() else { return };
        let st = Rc::new(RefCell::new(RenameState { tab: t.id, items, taken, plan: Vec::new() }));
        {
            let (weak, st) = (win.as_weak(), st.clone());
            win.on_changed(move || {
                if let Some(w) = weak.upgrade() {
                    refresh_preview(&w, &st);
                }
            });
        }
        win.on_cancel(|| close_rename());
        {
            let (weak, st) = (win.as_weak(), st.clone());
            win.on_apply(move || {
                let Some(w) = weak.upgrade() else { return };
                let (tab, pairs): (u64, Vec<(PathBuf, String)>) = {
                    let st = st.borrow();
                    (st.tab, st.plan.iter().filter(|p| p.status == Status::Ok).map(|p| (p.from.clone(), p.new_name.clone())).collect())
                };
                if pairs.is_empty() {
                    return;
                }
                w.set_busy(true);
                w.set_summary(t!("Renaming…").into());
                let hwnd = app(|a| a.hwnd());
                thread::spawn(move || {
                    let (done, errors) = apply_renames(&pairs, hwnd);
                    post(move |a| a.renames_finished(tab, done, errors));
                });
            });
        }
        refresh_preview(&win, &st);
        let _ = win.show();
        RENAME.with(|r| *r.borrow_mut() = Some(win));
    }

    fn renames_finished(&self, tab: u64, done: Vec<(PathBuf, PathBuf)>, errors: Vec<String>) {
        let win = RENAME.with(|r| r.borrow().as_ref().map(|w| w.clone_strong()));
        let Some(t) = self.find(tab) else { return close_rename() };
        if errors.is_empty() {
            close_rename();
        } else if let Some(w) = win {
            w.set_busy(false);
        }
        let select = done.iter().filter_map(|(_, to)| to.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        if !done.is_empty() {
            self.push_undo(Undo::RenameMany { pairs: done.clone() });
            self.set_note(&t, t!("Renamed {}", done.len()));
            self.reload(&t, After { select, ..Default::default() });
        }
        if !errors.is_empty() {
            let mut text = t!("Could not rename {} items.\n\n", errors.len());
            text.push_str(&errors.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
            if errors.len() > 8 {
                text.push_str(&t!("\n… and {} more", errors.len() - 8));
            }
            text.push_str(&t!("\n\nIf another process is using them, use 'Find processes in use' to check."));
            self.alert(text);
        }
    }

    // ---------- in use by ----------

    /// Opens the "in use by" dialog for the selection (or the current folder).
    pub(super) fn find_locks(&self) {
        let t = self.tab();
        if at_this_pc(&t) {
            return;
        }
        let mut paths = self.selected_paths(&t);
        if paths.is_empty() {
            paths.push(t.cwd.borrow().clone());
        }
        let Ok(win) = LocksWindow::new() else { return };
        let first = paths[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| paths[0].display().to_string());
        win.set_header(if paths.len() == 1 { t!("Target: {}", first) } else { t!("Target: {} and {} more", first, paths.len() - 1) }.into());
        win.on_cancel(|| close_locks());
        win.on_refresh(|| locks_scan());
        win.on_kill(|i| locks_kill(Some(i as usize)));
        win.on_kill_all(|| locks_kill(None));
        let _ = win.show();
        LOCKS.with(|l| *l.borrow_mut() = Some(LocksUi { win, lockers: Vec::new(), paths }));
        locks_scan();
    }
}

fn close_rename() {
    if let Some(w) = RENAME.with(|r| r.borrow_mut().take()) {
        let _ = w.hide();
    }
}

// ---------- in-use dialog state ----------

struct LocksUi {
    win: LocksWindow,
    lockers: Vec<Locker>,
    paths: Vec<PathBuf>,
}

fn close_locks() {
    if let Some(u) = LOCKS.with(|l| l.borrow_mut().take()) {
        let _ = u.win.hide();
    }
}

fn locks_window() -> Option<LocksWindow> {
    LOCKS.with(|l| l.borrow().as_ref().map(|u| u.win.clone_strong()))
}

fn locks_scan() {
    let Some(win) = locks_window() else { return };
    let paths = LOCKS.with(|l| l.borrow().as_ref().map(|u| u.paths.clone())).unwrap_or_default();
    win.set_busy(true);
    win.set_status(t!("Searching…").into());
    thread::spawn(move || {
        let r = locks::find_paths(&paths);
        post(move |_| locks_found(r));
    });
}

fn locks_found(r: io::Result<(Vec<Locker>, bool)>) {
    let Some(win) = locks_window() else { return };
    win.set_busy(false);
    let (lockers, capped) = match r {
        Ok(v) => v,
        Err(e) => {
            win.set_status(t!("Could not check: {}", e).into());
            (Vec::new(), false)
        }
    };
    let rows: Vec<LockRow> = lockers
        .iter()
        .map(|l| LockRow {
            name: l.name.clone().into(),
            pid: l.pid.to_string().into(),
            path: l.exe.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| t!("(path unavailable)")).into(),
            service: l.service,
        })
        .collect();
    if win.get_status() == t!("Searching…").as_str() {
        let note = match (lockers.len(), capped) {
            (0, false) => t!("No processes in use"),
            (n, false) => t!("{} processes", n),
            (n, true) => t!("{} processes · too many files, only some checked", n),
        };
        win.set_status(note.into());
    }
    win.set_rows(ModelRc::new(VecModel::from(rows)));
    LOCKS.with(|l| {
        if let Some(u) = l.borrow_mut().as_mut() {
            u.lockers = lockers;
        }
    });
}

/// Ends one process (`Some(row)`) or all listed ones (`None`), after asking.
fn locks_kill(which: Option<usize>) {
    let Some(win) = locks_window() else { return };
    let targets: Vec<Locker> = LOCKS.with(|l| {
        l.borrow().as_ref().map(|u| match which {
            Some(i) => u.lockers.get(i).cloned().into_iter().collect(),
            None => u.lockers.clone(),
        })
    })
    .unwrap_or_default();
    if targets.is_empty() {
        return;
    }
    win.set_busy(true);
    let hwnd = app(|a| a.hwnd());
    let names = targets.iter().take(5).map(|l| l.name.clone()).collect::<Vec<_>>().join(", ");
    let more = if targets.len() > 5 { t!(" and {} more", targets.len() - 5) } else { String::new() };
    let ask = t!("Force quit {}{}?\n\nUnsaved work may be lost.", names, more);
    thread::spawn(move || {
        let mut failed = Vec::new();
        if shell::confirm(hwnd, &ask) {
            for l in &targets {
                if let Err(e) = locks::kill(l) {
                    failed.push(format!("{} (PID {}): {e}", l.name, l.pid));
                }
            }
        }
        if !failed.is_empty() {
            let msg = t!(
                "Some processes could not be ended.\n\n{}\n\nProcesses running as administrator may not be endable.",
                failed.join("\n")
            );
            shell::alert(hwnd, &msg);
        }
        post(|_| locks_scan());
    });
}
