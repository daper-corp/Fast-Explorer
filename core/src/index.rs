//! Instant file-name search.
//!
//! An in-memory index of every name on an NTFS volume, built from the MFT (FSCTL_ENUM_USN_DATA)
//! and kept current from the USN change journal. Reading volumes needs admin rights, so the index
//! lives in the `fex-index` service; Fast Explorer queries it over a named pipe ([`query`]).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// File reference numbers are already well spread (record number + sequence); one multiply
/// mixes them enough, and is far cheaper than the default SipHash for millions of inserts.
#[derive(Default)]
struct FrnHasher(u64);

impl Hasher for FrnHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (n ^ (n >> 29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME,
};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_MANAGER_CONNECT,
    SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS,
};
use windows::core::{HSTRING, Owned};

use crate::menu::{dec_path, enc_path};
use crate::search::{Pattern, Scope};

pub const PIPE_NAME: &str = r"\\.\pipe\fast-explorer-index";
pub const SERVICE_NAME: &str = "FastExplorerIndex";

const GENERIC_READ: u32 = 0x8000_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// NTFS record number of the root folder; the low 48 bits of a file reference.
const ROOT_RECORD: u64 = 5;
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const DEAD: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct Rec {
    frn: u64,
    parent: u64,
    /// Name, as a span of `Volume::names`.
    off: u32,
    len: u16,
    dir: bool,
}

/// One volume's names. ~60 bytes per file: records in a Vec, names in one arena string.
pub struct Volume {
    letter: char,
    recs: Vec<Rec>,
    by_frn: HashMap<u64, u32, BuildHasherDefault<FrnHasher>>,
    names: String,
    free: Vec<u32>,
    /// Arena bytes no longer referenced (renamed/deleted); compacted once it is half the arena.
    garbage: usize,
}

impl Volume {
    pub fn new(letter: char) -> Self {
        Volume { letter, recs: Vec::new(), by_frn: HashMap::default(), names: String::new(), free: Vec::new(), garbage: 0 }
    }

    /// Returns spare capacity left over from growing (after the initial build).
    fn shrink(&mut self) {
        self.recs.shrink_to_fit();
        self.by_frn.shrink_to_fit();
        self.names.shrink_to_fit();
    }

    pub fn letter(&self) -> char {
        self.letter
    }

    pub fn len(&self) -> usize {
        self.by_frn.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_frn.is_empty()
    }

    fn name(&self, r: &Rec) -> &str {
        &self.names[r.off as usize..r.off as usize + r.len as usize]
    }

    fn push_name(&mut self, name: &str) -> (u32, u16) {
        // NTFS names are at most 255 UTF-16 units, so at most 765 UTF-8 bytes.
        let off = self.names.len() as u32;
        self.names.push_str(name);
        (off, name.len() as u16)
    }

    /// Adds a file, or updates it after a rename or move.
    pub fn upsert(&mut self, frn: u64, parent: u64, name: &str, dir: bool) {
        if let Some(&i) = self.by_frn.get(&frn) {
            let r = self.recs[i as usize];
            let (off, len) = if self.name(&r) == name {
                (r.off, r.len)
            } else {
                self.garbage += r.len as usize;
                self.push_name(name)
            };
            self.recs[i as usize] = Rec { frn, parent, off, len, dir };
            return;
        }
        let (off, len) = self.push_name(name);
        let rec = Rec { frn, parent, off, len, dir };
        let i = match self.free.pop() {
            Some(i) => {
                self.recs[i as usize] = rec;
                i
            }
            None => {
                self.recs.push(rec);
                self.recs.len() as u32 - 1
            }
        };
        self.by_frn.insert(frn, i);
    }

    pub fn remove(&mut self, frn: u64) {
        if let Some(i) = self.by_frn.remove(&frn) {
            let r = &mut self.recs[i as usize];
            self.garbage += r.len as usize;
            r.frn = DEAD;
            self.free.push(i);
        }
        if self.garbage > (1 << 20) && self.garbage > self.names.len() / 2 {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let mut fresh = String::with_capacity(self.names.len() - self.garbage);
        for r in self.recs.iter_mut().filter(|r| r.frn != DEAD) {
            let s = &self.names[r.off as usize..r.off as usize + r.len as usize];
            r.off = fresh.len() as u32;
            fresh.push_str(s);
        }
        self.names = fresh;
        self.garbage = 0;
    }

    /// "C:\dir\name", or None while the folder chain is incomplete.
    fn path(&self, i: u32) -> Option<String> {
        let mut parts = Vec::new();
        let mut r = self.recs[i as usize];
        for _ in 0..512 {
            parts.push(self.name(&r));
            if r.parent & RECORD_MASK == ROOT_RECORD {
                parts.push("");
                let mut path = format!("{}:", self.letter);
                path.push_str(&parts.iter().rev().copied().collect::<Vec<_>>().join("\\"));
                return Some(path);
            }
            r = self.recs[*self.by_frn.get(&r.parent)? as usize];
        }
        None // a cycle; shouldn't happen
    }

    /// Records whose name matches the pattern and whose path is in the scope, scanned on all
    /// cores. Paths are only built for records that already match the name (scoped searches).
    fn matches(&self, pattern: &Pattern, scope: &Scope) -> Vec<u32> {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = self.recs.len().div_ceil(threads).max(1 << 14);
        let needs_path = pattern.needs_path() || !matches!(scope, Scope::All);
        std::thread::scope(|s| {
            let handles: Vec<_> = self
                .recs
                .chunks(chunk)
                .enumerate()
                .map(|(c, recs)| {
                    s.spawn(move || {
                        let base = (c * chunk) as u32;
                        recs.iter()
                            .enumerate()
                            .filter(|(i, r)| {
                                r.frn != DEAD
                                    && pattern.matches(self.name(r))
                                    && (!needs_path
                                        || self.path(base + *i as u32).is_some_and(|p| {
                                            scope.includes(std::path::Path::new(&p))
                                                && (!pattern.needs_path() || pattern.matches_path(&p, scope.root()))
                                        }))
                            })
                            .map(|(i, _)| base + i as u32)
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub dir: bool,
}

/// Searches every volume: names matching the query and paths inside `scope`.
/// Returns the total match count and up to `limit` hits.
pub fn search<'a>(
    volumes: impl IntoIterator<Item = &'a Volume>,
    scope: &Scope,
    query: &str,
    limit: usize,
) -> Result<(usize, Vec<Hit>), String> {
    let pattern = Pattern::compile(query)?;
    let (mut total, mut hits) = (0, Vec::new());
    for v in volumes {
        if !scope.covers_volume(v.letter()) {
            continue;
        }
        let found = v.matches(&pattern, scope);
        total += found.len();
        for i in found {
            if hits.len() == limit {
                break;
            }
            if let Some(path) = v.path(i) {
                hits.push(Hit { path: path.into(), dir: v.recs[i as usize].dir });
            }
        }
    }
    Ok((total, hits))
}

// ---------- reading a volume (admin) ----------

/// A raw USN record (V2, what NTFS returns), decoded.
pub struct Change {
    pub frn: u64,
    pub parent: u64,
    pub reason: u32,
    pub dir: bool,
    pub name: String,
}

/// Decodes the USN_RECORD_V2 entries packed in `buf`.
pub fn parse_records(buf: &[u8], mut each: impl FnMut(Change)) {
    let u16_at = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    let mut off = 0;
    while off + 60 <= buf.len() {
        let len = u32_at(off) as usize;
        if len < 60 || off + len > buf.len() {
            break;
        }
        if u16_at(off + 4) == 2 {
            let (name_len, name_off) = (u16_at(off + 56) as usize, u16_at(off + 58) as usize);
            if name_off + name_len <= len {
                let units: Vec<u16> =
                    buf[off + name_off..off + name_off + name_len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                each(Change {
                    frn: u64_at(off + 8),
                    parent: u64_at(off + 16),
                    reason: u32_at(off + 40),
                    dir: u32_at(off + 52) & FILE_ATTRIBUTE_DIRECTORY != 0,
                    name: String::from_utf16_lossy(&units),
                });
            }
        }
        off += len;
    }
}

impl Volume {
    /// Applies a journal record.
    pub fn apply(&mut self, c: &Change) {
        if c.reason & USN_REASON_FILE_DELETE != 0 {
            self.remove(c.frn);
        } else if c.reason & USN_REASON_RENAME_OLD_NAME == 0 || c.reason & USN_REASON_RENAME_NEW_NAME != 0 {
            self.upsert(c.frn, c.parent, &c.name, c.dir);
        }
    }
}

/// Where to resume reading a volume's change journal.
pub struct JournalPos {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// The fixed NTFS volumes the index service builds an index for.
pub fn fixed_ntfs_volumes() -> Vec<char> {
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    ('A'..='Z')
        .filter(|&c| mask & (1 << (c as u32 - 'A' as u32)) != 0)
        .filter(|&c| {
            let root = HSTRING::from(format!("{c}:\\"));
            let mut fs = [0u16; 16];
            unsafe {
                GetDriveTypeW(&root) == DRIVE_FIXED
                    && GetVolumeInformationW(&root, None, None, None, None, Some(&mut fs)).is_ok()
                    && String::from_utf16_lossy(&fs).trim_end_matches('\0') == "NTFS"
            }
        })
        .collect()
}

/// Whether `path` lives on a volume the index service can answer for.
pub fn is_indexable(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    let letter = (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        .then(|| b[0].to_ascii_uppercase() as char);
    letter.is_some_and(|l| fixed_ntfs_volumes().contains(&l))
}

pub fn open_volume(letter: char) -> io::Result<Owned<HANDLE>> {
    let path = HSTRING::from(format!(r"\\.\{letter}:"));
    let h = unsafe {
        CreateFileW(&path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)?
    };
    Ok(unsafe { Owned::new(h) })
}

/// Reads every name on the volume from the MFT. Needs admin.
pub fn build(vol: HANDLE, letter: char) -> io::Result<(Volume, JournalPos)> {
    let mut jd = USN_JOURNAL_DATA_V0::default();
    unsafe {
        DeviceIoControl(vol, FSCTL_QUERY_USN_JOURNAL, None, 0, Some((&raw mut jd).cast()), size_of_val(&jd) as u32, None, None)?;
    }
    let mut index = Volume::new(letter);
    let mut med = MFT_ENUM_DATA_V0 { StartFileReferenceNumber: 0, LowUsn: 0, HighUsn: jd.NextUsn };
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let mut got = 0u32;
        let r = unsafe {
            DeviceIoControl(
                vol,
                FSCTL_ENUM_USN_DATA,
                Some((&raw const med).cast()),
                size_of_val(&med) as u32,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                Some(&mut got),
                None,
            )
        };
        if r.is_err() || got <= 8 {
            break; // ERROR_HANDLE_EOF: the whole MFT has been read
        }
        med.StartFileReferenceNumber = u64::from_le_bytes(buf[..8].try_into().unwrap());
        parse_records(&buf[8..got as usize], |c| index.upsert(c.frn, c.parent, &c.name, c.dir));
    }
    index.shrink();
    Ok((index, JournalPos { journal_id: jd.UsnJournalID, next_usn: jd.NextUsn }))
}

/// Blocks until the journal has changes past `pos`, applies them via `apply`, and advances `pos`.
/// An error (journal wrapped or recreated) means the index must be rebuilt.
pub fn read_changes(vol: HANDLE, pos: &mut JournalPos, buf: &mut [u8], apply: impl FnMut(Change)) -> io::Result<()> {
    let rd = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: pos.next_usn,
        ReasonMask: u32::MAX,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 1,
        UsnJournalID: pos.journal_id,
    };
    let mut got = 0u32;
    unsafe {
        DeviceIoControl(
            vol,
            FSCTL_READ_USN_JOURNAL,
            Some((&raw const rd).cast()),
            size_of_val(&rd) as u32,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            Some(&mut got),
            None,
        )?;
    }
    if got >= 8 {
        pos.next_usn = i64::from_le_bytes(buf[..8].try_into().unwrap());
        parse_records(&buf[8..got as usize], apply);
    }
    Ok(())
}

// ---------- the pipe protocol ----------
//
// Request:  S <limit> <scope-kind> <scope-path> <query>\n
//   scope-kind: a (all drives) | u (under path) | d (direct children of path)
// Reply:    OK <count> <indexing-volumes> <more 0|1>\n  then `count` lines:  d|f <path>\n
//           ERR <message>\n
// No overall match count is sent: it would include files hidden from the client.

/// The scope as two wire fields: a kind letter and an encoded path (empty for `All`).
pub fn encode_scope(scope: &Scope) -> (&'static str, String) {
    match scope {
        Scope::All => ("a", String::new()),
        Scope::Under(p) => ("u", enc_path(p)),
        Scope::Direct(p) => ("d", enc_path(p)),
    }
}

pub fn decode_scope(kind: &str, path: &str) -> Option<Scope> {
    match kind {
        "a" => Some(Scope::All),
        "u" => Some(Scope::Under(dec_path(path))),
        "d" => Some(Scope::Direct(dec_path(path))),
        _ => None,
    }
}

pub struct Results {
    pub hits: Vec<Hit>,
    /// More visible matches exist than were returned.
    pub more: bool,
    /// Volumes still being indexed (results from them are incomplete).
    pub indexing: usize,
}

/// Version 2 carries the search scope. A bare `OK` reply comes from an older service.
pub const REPLY_OK: &str = "OK2";

pub fn encode_results(out: &mut impl Write, hits: &[Hit], indexing: usize, more: bool) -> io::Result<()> {
    writeln!(out, "{REPLY_OK}\t{}\t{indexing}\t{}", hits.len(), more as u8)?;
    for h in hits {
        writeln!(out, "{}\t{}", if h.dir { 'd' } else { 'f' }, enc_path(&h.path))?;
    }
    Ok(())
}

pub fn decode_results(input: impl BufRead) -> io::Result<Results> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, crate::t!("Invalid reply"));
    let mut lines = input.lines();
    let head = lines.next().ok_or_else(bad)??;
    let mut f = head.split('\t');
    match f.next() {
        Some(REPLY_OK) => {}
        Some("OK") => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, crate::t!("Index service is outdated · reinstall it")));
        }
        Some("ERR") => return Err(io::Error::new(io::ErrorKind::InvalidInput, f.next().unwrap_or("").to_string())),
        _ => return Err(bad()),
    }
    let mut num = || f.next().and_then(|s| s.parse::<usize>().ok()).ok_or_else(bad);
    let (count, indexing, more) = (num()?, num()?, num()? != 0);
    let mut hits = Vec::with_capacity(count.min(1 << 16));
    for line in lines.take(count) {
        let line = line?;
        let (kind, path) = line.split_once('\t').ok_or_else(bad)?;
        hits.push(Hit { path: dec_path(path), dir: kind == "d" });
    }
    Ok(Results { hits, more, indexing })
}

#[derive(Debug)]
pub enum QueryError {
    /// The service isn't installed (offer to install it).
    NotInstalled,
    /// Installed but not reachable (stopped, starting, or something else owns the pipe).
    Unavailable(String),
    /// The service understood the request but refused it (e.g. a bad regular expression).
    Rejected(String),
}

/// Asks the index service. Refuses to talk to a pipe not served by the service itself,
/// so another process can't pose as the index.
pub fn query(scope: &Scope, text: &str, limit: usize) -> Result<Results, QueryError> {
    let service_pid = service_pid()?;
    let pipe = open_pipe().map_err(|e| QueryError::Unavailable(e.to_string()))?;
    let mut server_pid = 0;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server_pid) }
        .map_err(|e| QueryError::Unavailable(e.to_string()))?;
    if server_pid != service_pid {
        return Err(QueryError::Unavailable(crate::t!("The search pipe is not owned by the index service")));
    }
    let query = text.replace(['\t', '\r', '\n'], " ");
    let (kind, path) = encode_scope(scope);
    (&pipe)
        .write_all(format!("S\t{limit}\t{kind}\t{path}\t{query}\n").as_bytes())
        .map_err(|e| QueryError::Unavailable(e.to_string()))?;
    decode_results(BufReader::new(&pipe)).map_err(|e| match e.kind() {
        io::ErrorKind::InvalidInput => QueryError::Rejected(e.to_string()),
        _ => QueryError::Unavailable(e.to_string()),
    })
}

fn open_pipe() -> io::Result<std::fs::File> {
    for _ in 0..50 {
        // Exactly the rights the pipe grants: read/write data, not "create pipe instance".
        const ACCESS: u32 = 0x0010_0083; // FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE
        match std::fs::OpenOptions::new().access_mode(ACCESS).open(PIPE_NAME) {
            Err(e) if e.raw_os_error() == Some(231) => std::thread::sleep(Duration::from_millis(20)), // ERROR_PIPE_BUSY
            r => return r,
        }
    }
    Err(io::Error::other(crate::t!("The search pipe stays busy")))
}

/// The running service's process id.
fn service_pid() -> Result<u32, QueryError> {
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_CONNECT).map_err(|e| QueryError::Unavailable(e.to_string()))?;
        let svc = OpenServiceW(scm, &HSTRING::from(SERVICE_NAME), SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(scm);
        let svc = svc.map_err(|e| match e.code() == windows::core::HRESULT::from_win32(ERROR_SERVICE_DOES_NOT_EXIST as u32) {
            true => QueryError::NotInstalled,
            false => QueryError::Unavailable(e.to_string()),
        })?;
        let mut status = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        let r = QueryServiceStatusEx(
            svc,
            SC_STATUS_PROCESS_INFO,
            Some(std::slice::from_raw_parts_mut((&raw mut status).cast(), size_of_val(&status))),
            &mut needed,
        );
        let _ = CloseServiceHandle(svc);
        r.map_err(|e| QueryError::Unavailable(e.to_string()))?;
        match status.dwProcessId {
            0 => Err(QueryError::Unavailable(crate::t!("The index service is not running"))),
            pid => Ok(pid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: u64 = (1 << 48) | ROOT_RECORD; // sequence number in the high bits

    fn sample() -> Volume {
        let mut v = Volume::new('C');
        v.upsert(100, ROOT, "Users", true);
        v.upsert(101, 100, "hp", true);
        v.upsert(102, 101, "Report 2024.docx", false);
        v.upsert(103, 101, "보고서.hwp", false);
        v.upsert(104, ROOT, "Windows", true);
        v
    }

    fn names(v: &Volume, q: &str) -> Vec<String> {
        let mut p: Vec<String> = search([v], &Scope::All, q, 100)
            .unwrap()
            .1
            .into_iter()
            .map(|h| h.path.display().to_string())
            .collect();
        p.sort();
        p
    }

    #[test]
    fn finds_by_any_part_case_insensitively() {
        let v = sample();
        assert_eq!(names(&v, "report"), [r"C:\Users\hp\Report 2024.docx"]);
        assert_eq!(names(&v, "REP 2024"), [r"C:\Users\hp\Report 2024.docx"]);
        assert_eq!(names(&v, "보고"), [r"C:\Users\hp\보고서.hwp"]);
        assert_eq!(names(&v, "users"), [r"C:\Users"]);
        assert!(names(&v, "report hwp").is_empty());
        let (total, hits) = search([&v], &Scope::All, "s", 1).unwrap(); // "Users", "Windows"
        assert_eq!((total, hits.len()), (2, 1)); // total counts beyond the limit
    }

    #[test]
    fn scopes_limit_results_to_a_folder() {
        let v = sample();
        let under = Scope::Under(PathBuf::from(r"C:\Users"));
        let found = search([&v], &under, "report", 100).unwrap().1;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, PathBuf::from(r"C:\Users\hp\Report 2024.docx"));
        // Direct children only: the file is one level deeper.
        let direct = Scope::Direct(PathBuf::from(r"C:\Users"));
        assert!(search([&v], &direct, "report", 100).unwrap().1.is_empty());
        let direct = Scope::Direct(PathBuf::from(r"C:\Users\hp"));
        assert_eq!(search([&v], &direct, "report", 100).unwrap().1.len(), 1);
        // Wildcards and regexes work through the same search.
        assert_eq!(search([&v], &Scope::All, "*.docx", 100).unwrap().1.len(), 1);
        assert_eq!(search([&v], &Scope::All, r"re:^보고", 100).unwrap().1.len(), 1);
    }

    #[test]
    fn scope_round_trips_on_the_wire() {
        for (scope, kind, path) in [
            (Scope::All, "a", ""),
            (Scope::Under(PathBuf::from(r"C:\a b")), "u", r"C:\a b"),
            (Scope::Direct(PathBuf::from(r"C:\a b")), "d", r"C:\a b"),
        ] {
            let (k, p) = encode_scope(&scope);
            assert_eq!((k, p.as_str()), (kind, path));
            assert_eq!(decode_scope(k, &p), Some(scope));
        }
        assert_eq!(decode_scope("x", ""), None);
    }

    #[test]
    fn follows_renames_moves_and_deletes() {
        let mut v = sample();
        v.apply(&Change { frn: 102, parent: 104, reason: USN_REASON_RENAME_NEW_NAME, dir: false, name: "final.docx".into() });
        assert_eq!(names(&v, "final"), [r"C:\Windows\final.docx"]);
        assert!(names(&v, "report").is_empty());
        v.apply(&Change { frn: 100, parent: ROOT, reason: USN_REASON_RENAME_NEW_NAME, dir: true, name: "People".into() });
        assert_eq!(names(&v, "보고서"), [r"C:\People\hp\보고서.hwp"]);
        v.apply(&Change { frn: 103, parent: 101, reason: USN_REASON_FILE_DELETE, dir: false, name: "보고서.hwp".into() });
        assert!(names(&v, "보고서").is_empty());
        assert_eq!(v.len(), 4);
        // Space of deleted records is reused and the name arena compacts.
        for i in 0..50_000u64 {
            v.upsert(1000 + i, ROOT, "a-fairly-long-temporary-file-name.tmp", false);
            v.remove(1000 + i);
        }
        assert!(v.names.len() < 1 << 21 && v.recs.len() < 10);
        assert_eq!(names(&v, "final"), [r"C:\Windows\final.docx"]);
    }

    #[test]
    fn parses_usn_records_and_round_trips_results() {
        // One USN_RECORD_V2 for "a.txt" (FRN 7, parent 5, directory attribute off).
        let name: Vec<u8> = "a.txt".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (60 + name.len()).next_multiple_of(8);
        let mut rec = vec![0u8; len];
        rec[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        rec[4..6].copy_from_slice(&2u16.to_le_bytes());
        rec[8..16].copy_from_slice(&7u64.to_le_bytes());
        rec[16..24].copy_from_slice(&5u64.to_le_bytes());
        rec[56..58].copy_from_slice(&(name.len() as u16).to_le_bytes());
        rec[58..60].copy_from_slice(&60u16.to_le_bytes());
        rec[60..60 + name.len()].copy_from_slice(&name);
        let mut got = Vec::new();
        parse_records(&rec, |c| got.push((c.frn, c.parent, c.name, c.dir)));
        assert_eq!(got, [(7, 5, "a.txt".to_string(), false)]);

        let hits = vec![Hit { path: r"C:\a.txt".into(), dir: false }, Hit { path: "C:\\odd\tname".into(), dir: true }];
        let mut buf = Vec::new();
        encode_results(&mut buf, &hits, 1, true).unwrap();
        let r = decode_results(&buf[..]).unwrap();
        assert_eq!((r.hits, r.indexing, r.more), (hits, 1, true));
    }
}
