//! Shell context menus, served by the separate `fex-shell-host` process.
//!
//! Shell extensions (IContextMenu handlers) run inside the host, so one that hangs or crashes
//! takes the host down, not Fast Explorer: the client restarts it on the next request, and kills
//! it if a menu takes longer than [`HANG_LIMIT`].
//!
//! Protocol: one request per line on the host's stdin, replies on its stdout, tab-separated.
//!   Q seq path...        item menu for paths in one folder     -> M seq count, then `count` item lines
//!   B seq folder         folder background menu                -> M ... (or X seq error)
//!   I seq id             invoke item `id` of menu `seq`        -> R seq [error]
//!   V seq verb path...   build the item menu, invoke `verb`    -> R seq [error]

use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A menu query unanswered for this long means a stuck shell extension: the host is replaced.
pub const HANG_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuItem {
    /// Nesting level; items following a `submenu` item one level deeper are its children.
    pub depth: u8,
    /// Command id to send back with [`Request::Invoke`] (1..=0x7FFF).
    pub id: u32,
    pub separator: bool,
    pub disabled: bool,
    pub default: bool,
    pub submenu: bool,
    /// Canonical verb ("open", "properties", ...) when the handler reports one.
    pub verb: String,
    /// Menu text, with `&` accelerator markers.
    pub label: String,
}

impl MenuItem {
    pub fn encode(&self) -> String {
        let flags: String = [(self.separator, 's'), (self.disabled, 'd'), (self.default, 'b'), (self.submenu, 'p')]
            .iter()
            .filter(|f| f.0)
            .map(|f| f.1)
            .collect();
        format!("{}\t{}\t{}\t{}\t{}", self.depth, self.id, flags, one_line(&self.verb), one_line(&self.label))
    }

    pub fn decode(line: &str) -> Option<Self> {
        let mut p = line.splitn(5, '\t');
        let (depth, id, flags) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?);
        Some(MenuItem {
            depth,
            id,
            separator: flags.contains('s'),
            disabled: flags.contains('d'),
            default: flags.contains('b'),
            submenu: flags.contains('p'),
            verb: p.next()?.to_string(),
            label: p.next()?.to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Items { seq: u64, paths: Vec<PathBuf> },
    Background { seq: u64, folder: PathBuf },
    Invoke { seq: u64, id: u32 },
    Verb { seq: u64, verb: String, paths: Vec<PathBuf> },
}

impl Request {
    pub fn seq(&self) -> u64 {
        match self {
            Request::Items { seq, .. } | Request::Background { seq, .. } => *seq,
            Request::Invoke { seq, .. } | Request::Verb { seq, .. } => *seq,
        }
    }

    pub fn is_query(&self) -> bool {
        matches!(self, Request::Items { .. } | Request::Background { .. })
    }

    pub fn encode(&self) -> String {
        let paths = |ps: &[PathBuf]| ps.iter().map(|p| enc_path(p)).collect::<Vec<_>>().join("\t");
        match self {
            Request::Items { seq, paths: ps } => format!("Q\t{seq}\t{}", paths(ps)),
            Request::Background { seq, folder } => format!("B\t{seq}\t{}", enc_path(folder)),
            Request::Invoke { seq, id } => format!("I\t{seq}\t{id}"),
            Request::Verb { seq, verb, paths: ps } => format!("V\t{seq}\t{}\t{}", one_line(verb), paths(ps)),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let mut p = line.split('\t');
        let (kind, seq) = (p.next()?, p.next()?.parse().ok()?);
        Some(match kind {
            "Q" => Request::Items { seq, paths: p.map(dec_path).collect() },
            "B" => Request::Background { seq, folder: dec_path(p.next()?) },
            "I" => Request::Invoke { seq, id: p.next()?.parse().ok()? },
            "V" => Request::Verb { seq, verb: p.next()?.to_string(), paths: p.map(dec_path).collect() },
            _ => return None,
        })
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

/// Paths travel as text; ones that aren't valid Unicode (lone surrogates) go as hex UTF-16
/// behind a `\x01` marker so they arrive exactly as they were.
fn enc_path(p: &Path) -> String {
    match p.to_str() {
        Some(s) => s.to_string(),
        None => std::iter::once("\x01".to_string())
            .chain(p.as_os_str().encode_wide().map(|u| format!("{u:04x}")))
            .collect(),
    }
}

fn dec_path(s: &str) -> PathBuf {
    match s.strip_prefix('\x01') {
        Some(hex) => {
            let units: Vec<u16> = (0..hex.len() / 4)
                .filter_map(|i| u16::from_str_radix(&hex[i * 4..i * 4 + 4], 16).ok())
                .collect();
            PathBuf::from(std::ffi::OsString::from_wide(&units))
        }
        None => PathBuf::from(s),
    }
}

/// Result of the newest menu query.
pub type MenuResult = Result<Vec<MenuItem>, String>;

#[derive(Default)]
struct Latest {
    /// The newest query sent, and when.
    asked: Option<(u64, Instant)>,
    answer: Option<(u64, MenuResult)>,
    /// Which host process is current; replies from a replaced one are ignored.
    generation: u64,
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
}

/// Client side: owns the host process. Cheap to share (`Arc`); all methods are thread-safe.
pub struct MenuHost {
    exe: PathBuf,
    proc: Mutex<Option<Proc>>,
    latest: Arc<(Mutex<Latest>, Condvar)>,
    on_invoked: Arc<dyn Fn(u64, Result<(), String>) + Send + Sync>,
}

impl MenuHost {
    /// `on_invoked` is called on a reader thread when an Invoke/Verb request finishes.
    pub fn new(exe: PathBuf, on_invoked: impl Fn(u64, Result<(), String>) + Send + Sync + 'static) -> Self {
        MenuHost {
            exe,
            proc: Mutex::new(None),
            latest: Arc::default(),
            on_invoked: Arc::new(on_invoked),
        }
    }

    /// Starts the host ahead of the first menu (shell DLLs take a moment to load).
    pub fn warm_up(&self) {
        let mut proc = self.proc.lock().unwrap();
        if proc.is_none() {
            *proc = self.spawn().ok();
        }
    }

    /// Sends a request. A newer query supersedes older ones.
    pub fn send(&self, req: &Request) -> Result<(), String> {
        if req.is_query() {
            let (m, cv) = &*self.latest;
            m.lock().unwrap().asked = Some((req.seq(), Instant::now()));
            cv.notify_all();
        }
        let line = req.encode() + "\n";
        let mut proc = self.proc.lock().unwrap();
        if self.stuck() {
            kill(&mut proc);
        }
        for _ in 0..2 {
            if proc.as_mut().is_none_or(|p| !matches!(p.child.try_wait(), Ok(None))) {
                *proc = Some(self.spawn().map_err(|e| format!("셸 메뉴 프로세스를 시작할 수 없음: {e}"))?);
            }
            let p = proc.as_mut().unwrap();
            if p.stdin.write_all(line.as_bytes()).and_then(|_| p.stdin.flush()).is_ok() {
                return Ok(());
            }
            kill(&mut proc);
        }
        Err("셸 메뉴 프로세스에 보낼 수 없음".into())
    }

    /// Waits up to `timeout` for the answer to query `seq`. `None`: not answered yet
    /// (or superseded by a newer query).
    pub fn wait(&self, seq: u64, timeout: Duration) -> Option<MenuResult> {
        let (m, cv) = &*self.latest;
        let deadline = Instant::now() + timeout;
        let mut l = m.lock().unwrap();
        loop {
            if let Some((s, r)) = &l.answer
                && *s == seq
            {
                return Some(r.clone());
            }
            if l.asked.is_none_or(|(s, _)| s != seq) {
                return None;
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            l = cv.wait_timeout(l, left).unwrap().0;
        }
    }

    /// Kills the host (e.g. after a menu timed out); the next request starts a fresh one.
    pub fn restart(&self) {
        kill(&mut self.proc.lock().unwrap());
    }

    pub fn pid(&self) -> Option<u32> {
        self.proc.lock().unwrap().as_ref().map(|p| p.child.id())
    }

    fn stuck(&self) -> bool {
        let l = self.latest.0.lock().unwrap();
        matches!(l.asked, Some((s, t)) if t.elapsed() > HANG_LIMIT && l.answer.as_ref().is_none_or(|a| a.0 != s))
    }

    fn spawn(&self) -> std::io::Result<Proc> {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut child = Command::new(&self.exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let generation = {
            let mut l = self.latest.0.lock().unwrap();
            l.generation += 1;
            l.generation
        };
        let (latest, on_invoked) = (self.latest.clone(), self.on_invoked.clone());
        thread::spawn(move || read_replies(stdout, generation, &latest, &*on_invoked));
        Ok(Proc { child, stdin })
    }
}

impl Drop for MenuHost {
    fn drop(&mut self) {
        kill(&mut self.proc.lock().unwrap());
    }
}

fn kill(proc: &mut Option<Proc>) {
    if let Some(mut p) = proc.take() {
        let _ = p.child.kill();
        let _ = p.child.wait();
    }
}

fn read_replies(
    out: ChildStdout,
    generation: u64,
    latest: &(Mutex<Latest>, Condvar),
    on_invoked: &(dyn Fn(u64, Result<(), String>) + Send + Sync),
) {
    let (m, cv) = latest;
    let answer = |seq: u64, r: MenuResult| {
        let mut l = m.lock().unwrap();
        if l.generation == generation {
            l.answer = Some((seq, r));
            cv.notify_all();
        }
    };
    let mut lines = BufReader::new(out).lines();
    while let Some(Ok(line)) = lines.next() {
        let mut p = line.splitn(3, '\t');
        let (kind, seq) = (p.next(), p.next().and_then(|s| s.parse::<u64>().ok()));
        let (Some(kind), Some(seq), rest) = (kind, seq, p.next().unwrap_or("")) else { continue };
        match kind {
            "M" => {
                let n: usize = rest.parse().unwrap_or(0);
                let items = (0..n).filter_map(|_| lines.next()?.ok()).filter_map(|l| MenuItem::decode(&l)).collect();
                answer(seq, Ok(items));
            }
            "X" => answer(seq, Err(rest.to_string())),
            "R" => on_invoked(seq, if rest.is_empty() { Ok(()) } else { Err(rest.to_string()) }),
            _ => {}
        }
    }
    // The host exited or was killed: fail the query it still owed, so nobody waits for nothing.
    let mut l = m.lock().unwrap();
    if l.generation == generation
        && let Some((seq, _)) = l.asked
        && l.answer.as_ref().is_none_or(|a| a.0 != seq)
    {
        l.answer = Some((seq, Err("셸 메뉴 프로세스가 종료됨".into())));
        cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_items_round_trip() {
        let odd = PathBuf::from(std::ffi::OsString::from_wide(&[b'a' as u16, 0xD800, b'b' as u16]));
        for r in [
            Request::Items { seq: 7, paths: vec![r"C:\a b\x.txt".into(), odd.clone()] },
            Request::Background { seq: 8, folder: r"C:\".into() },
            Request::Invoke { seq: 9, id: 42 },
            Request::Verb { seq: 10, verb: "properties".into(), paths: vec![odd] },
        ] {
            assert_eq!(Request::decode(&r.encode()), Some(r));
        }
        let item = MenuItem { depth: 1, id: 5, submenu: true, verb: "x".into(), label: "&Send to\tCtrl".into(), ..Default::default() };
        let back = MenuItem::decode(&item.encode()).unwrap();
        assert_eq!((back.depth, back.id, back.submenu, back.label.as_str()), (1, 5, true, "&Send to Ctrl"));
    }
}
