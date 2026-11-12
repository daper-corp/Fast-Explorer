//! Search patterns and scopes shared by the UI and the index service.
//!
//! A plain word matches anywhere in a file name. `*` and `?` turn a word into a
//! case-insensitive wildcard pattern, and `re:<regex>` or `/regex/` matches the whole query
//! as a regular expression. A query that starts with a path (`C:\dir\*.txt`, `sub\a*`) scopes
//! the search to that folder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::{WalkBuilder, WalkState};
use regex::{Regex, RegexBuilder};

/// Which part of the file system a search covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every indexed volume.
    All,
    /// Everything below this folder.
    Under(PathBuf),
    /// Only the direct children of this folder.
    Direct(PathBuf),
}

impl Scope {
    /// The folder this scope is rooted at (None for every drive).
    pub fn root(&self) -> Option<&Path> {
        match self {
            Scope::All => None,
            Scope::Under(p) | Scope::Direct(p) => Some(p),
        }
    }

    /// Whether `path` (a file system path) belongs to this scope.
    pub fn includes(&self, path: &Path) -> bool {
        match self {
            Scope::All => true,
            Scope::Under(root) => below(path, root).is_some_and(|rest| !rest.is_empty()),
            Scope::Direct(root) => below(path, root).is_some_and(|rest| rest.len() == 1),
        }
    }

    /// Whether a volume with this drive letter can contain matches (paths on UNC drives are
    /// always allowed through; the full path check decides).
    pub fn covers_volume(&self, letter: char) -> bool {
        let Some(root) = self.root() else { return true };
        let s = root.to_string_lossy();
        let b = s.as_bytes();
        match b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            true => b[0].eq_ignore_ascii_case(&(letter as u8)),
            false => true,
        }
    }
}

/// Components of `path` below `root`, or None when `path` isn't under it. Windows paths
/// compare case-insensitively; the components are returned as they appear in `path`.
pub fn below(path: &Path, root: &Path) -> Option<Vec<OsString>> {
    let p: Vec<_> = path.components().collect();
    let r: Vec<_> = root.components().collect();
    if p.len() < r.len() {
        return None;
    }
    for (a, b) in p.iter().zip(&r) {
        if a.as_os_str().to_string_lossy().to_lowercase() != b.as_os_str().to_string_lossy().to_lowercase() {
            return None;
        }
    }
    Some(p[r.len()..].iter().map(|c| c.as_os_str().to_os_string()).collect())
}

/// The root of the volume holding `path` (`C:\` for files, the share for UNC paths).
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().filter(|p| !p.as_os_str().is_empty()).last().map(Path::to_path_buf)
}

/// A compiled search query.
#[derive(Debug)]
pub struct Pattern(Inner);

#[derive(Debug)]
enum Inner {
    /// The whole query is one regular expression (case-insensitive unless it says otherwise).
    Regex(Regex),
    /// Semicolon-separated OR groups; every word in a group must match.
    Groups(Vec<Group>),
}

/// One OR group: words that must all match, and `!` words that must not.
#[derive(Debug)]
struct Group {
    pos: Vec<Term>,
    neg: Vec<Term>,
}

/// A word, matched against the name, or against the whole path when it has a separator.
#[derive(Debug)]
struct Term {
    word: Word,
    path: bool,
}

impl Term {
    fn new(text: &str) -> Self {
        if !text.contains(['\\', '/']) {
            return Term { word: word(text), path: false };
        }
        let mut t = text.replace('/', "\\").replace("\\**\\", "\\");
        // A path glob may start anywhere in the path unless it names a drive or share.
        if t.contains(['*', '?']) && !is_absolute_prefix(&t) && !t.starts_with('*') {
            t.insert(0, '*');
        }
        while t.contains("**") {
            t = t.replace("**", "*");
        }
        Term { word: word(&t), path: true }
    }
}

impl Pattern {
    /// Compiles a query. Errors are user-facing (bad regular expression, empty query).
    pub fn compile(query: &str) -> Result<Self, String> {
        Self::compile_mode(query, false)
    }

    /// `regex` makes the whole query one regular expression (semicolons stay literal).
    /// Otherwise `re:` and `/.../` still opt in, and `;` separates OR groups.
    pub fn compile_mode(query: &str, regex: bool) -> Result<Self, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err(crate::t!("Search query is empty"));
        }
        if regex {
            let source = match query.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("re:")) {
                true => &query[3..],
                false => query,
            };
            if source.trim().is_empty() {
                return Err(crate::t!("Search query is empty"));
            }
            let re = RegexBuilder::new(source)
                .case_insensitive(true)
                .build()
                .map_err(|e| crate::t!("Regex error: {}", e))?;
            return Ok(Pattern(Inner::Regex(re)));
        }
        if let Some(source) = regex_source(query) {
            if source.trim().is_empty() {
                return Err(crate::t!("Search query is empty"));
            }
            let re = RegexBuilder::new(source)
                .case_insensitive(true)
                .build()
                .map_err(|e| crate::t!("Regex error: {}", e))?;
            return Ok(Pattern(Inner::Regex(re)));
        }
        let groups: Vec<Group> = query
            .split(';')
            .map(|g| {
                let mut group = Group { pos: Vec::new(), neg: Vec::new() };
                for w in g.split_whitespace() {
                    match w.strip_prefix('!').filter(|r| !r.is_empty()) {
                        Some(rest) => group.neg.push(Term::new(rest)),
                        None => group.pos.push(Term::new(w)),
                    }
                }
                group
            })
            .filter(|g| !g.pos.is_empty() || !g.neg.is_empty())
            .collect();
        match groups.is_empty() {
            true => Err(crate::t!("Search query is empty")),
            false => Ok(Pattern(Inner::Groups(groups))),
        }
    }

    /// Checks `name` alone. When [`needs_path`](Self::needs_path) is true this is only a quick
    /// prefilter (path words and `!` words are skipped); confirm hits with `matches_path`.
    pub fn matches(&self, name: &str) -> bool {
        match &self.0 {
            Inner::Regex(re) => re.is_match(name),
            Inner::Groups(groups) => groups.iter().any(|g| g.pos.iter().all(|t| t.path || t.word.matches(name))),
        }
    }

    /// Whether a full path is needed to decide (path words or `!` words are present).
    pub fn needs_path(&self) -> bool {
        match &self.0 {
            Inner::Regex(_) => false,
            Inner::Groups(groups) => groups.iter().any(|g| !g.neg.is_empty() || g.pos.iter().any(|t| t.path)),
        }
    }

    /// The full check for `path`. `root` is the searched folder: `!` words look only at the
    /// part of the path below it, so a folder you are inside of is never excluded.
    pub fn matches_path(&self, path: &str, root: Option<&Path>) -> bool {
        let Inner::Groups(groups) = &self.0 else {
            return self.matches(path.rsplit(['\\', '/']).next().unwrap_or(path));
        };
        let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
        let rel = &path[rel_start(path, root)..];
        groups.iter().any(|g| {
            g.pos.iter().all(|t| t.word.matches(if t.path { path } else { name }))
                && !g.neg.iter().any(|t| match t.path {
                    true => t.word.matches(path),
                    false => rel.split(['\\', '/']).any(|c| t.word.matches(c)),
                })
        })
    }

    /// Whether a folder with this name can be skipped entirely (every OR group excludes it).
    pub fn prunes(&self, dir_name: &str) -> bool {
        match &self.0 {
            Inner::Regex(_) => false,
            Inner::Groups(groups) => groups.iter().all(|g| g.neg.iter().any(|t| !t.path && t.word.matches(dir_name))),
        }
    }
}

/// Byte offset in `path` where the part below `root` starts (0 without a root).
fn rel_start(path: &str, root: Option<&Path>) -> usize {
    let Some(len) = root.map(|r| r.as_os_str().len()) else { return 0 };
    if len > path.len() || !path.is_char_boundary(len) {
        return 0;
    }
    match path[len..].starts_with(['\\', '/']) {
        true => len + 1,
        false => len,
    }
}

fn word(text: &str) -> Word {
    match text.contains(['*', '?']) {
        true => Word::Glob(Glob::new(text)),
        false => Word::Sub(Substring::new(text)),
    }
}

/// `re:...` / `RE:...`, or a whole `/.../` query.
fn regex_source(query: &str) -> Option<&str> {
    if query.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("re:")) {
        return Some(&query[3..]);
    }
    (query.len() >= 2 && query.starts_with('/') && query.ends_with('/')).then(|| &query[1..query.len() - 1])
}

#[derive(Debug)]
enum Word {
    Sub(Substring),
    Glob(Glob),
}

impl Word {
    fn matches(&self, name: &str) -> bool {
        match self {
            Word::Sub(s) => s.matches(name),
            Word::Glob(g) => g.matches(name),
        }
    }
}

/// A word matched anywhere in the name, case-insensitively.
#[derive(Debug)]
struct Substring {
    lower: String,
    kind: SubstringKind,
}

#[derive(Debug)]
enum SubstringKind {
    /// ASCII bytes never occur inside UTF-8 multi-byte sequences, so a byte-wise
    /// ASCII-case-insensitive search is exact.
    Ascii,
    /// No letter in it has case (Hangul, digits, ...): a plain substring search is exact.
    Caseless,
    Other,
}

impl Substring {
    fn new(t: &str) -> Self {
        let lower = t.to_lowercase();
        let kind = if t.is_ascii() {
            SubstringKind::Ascii
        } else if t.chars().all(|c| c.is_lowercase() == c.is_uppercase()) {
            SubstringKind::Caseless
        } else {
            SubstringKind::Other
        };
        Substring { lower, kind }
    }

    fn matches(&self, name: &str) -> bool {
        match self.kind {
            SubstringKind::Ascii => {
                let (h, n) = (name.as_bytes(), self.lower.as_bytes());
                n.len() <= h.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
            }
            SubstringKind::Caseless => name.contains(&self.lower),
            SubstringKind::Other => name.to_lowercase().contains(&self.lower),
        }
    }
}

/// An Explorer-style wildcard pattern over the whole name.
#[derive(Debug)]
struct Glob {
    /// The pattern, lowercased unless it is pure ASCII (compared case-insensitively then).
    pat: Vec<char>,
    ascii: bool,
}

impl Glob {
    fn new(pattern: &str) -> Self {
        match pattern.is_ascii() {
            true => Glob { pat: pattern.chars().collect(), ascii: true },
            false => Glob { pat: pattern.to_lowercase().chars().collect(), ascii: false },
        }
    }

    fn matches(&self, name: &str) -> bool {
        let eq = |p: char, t: char| match self.ascii {
            true => p.eq_ignore_ascii_case(&t),
            false => p == t,
        };
        // Common shapes avoid collecting every name into a char vector.
        if self.pat.as_slice() == ['*'] {
            return true;
        }
        let lower;
        let text = match self.ascii {
            true => name,
            false => {
                lower = name.to_lowercase();
                &lower
            }
        };
        if !self.pat.contains(&'?') && self.pat.iter().filter(|&&c| c == '*').count() == 1 {
            if self.pat.first() == Some(&'*') {
                return ends_with(text, &self.pat[1..], eq);
            }
            if self.pat.last() == Some(&'*') {
                return starts_with(text, &self.pat[..self.pat.len() - 1], eq);
            }
        }
        glob(&self.pat, &text.chars().collect::<Vec<_>>(), eq)
    }
}

fn starts_with(name: &str, pat: &[char], eq: impl Fn(char, char) -> bool) -> bool {
    let mut chars = name.chars();
    for &p in pat {
        match chars.next() {
            Some(c) if eq(p, c) => {}
            _ => return false,
        }
    }
    true
}

fn ends_with(name: &str, pat: &[char], eq: impl Fn(char, char) -> bool) -> bool {
    let mut chars = name.chars().rev();
    for &p in pat.iter().rev() {
        match chars.next() {
            Some(c) if eq(p, c) => {}
            _ => return false,
        }
    }
    true
}

/// Classic two-pointer wildcard match with backtracking for `*`.
fn glob(pat: &[char], text: &[char], eq: impl Fn(char, char) -> bool) -> bool {
    let (mut p, mut t) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        if p < pat.len() && (pat[p] == '?' || (pat[p] != '*' && eq(pat[p], text[t]))) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            p += 1;
            mark = t;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// A search query split into the folder to search and the pattern for names in it.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub scope: Scope,
    pub pattern: String,
}

/// Builds a plan from a typed query, the current folder, and the chosen scope.
///
/// A query with a path in it (`C:\dir\*.txt`, `sub\report`) searches that folder instead of
/// the chosen scope; the last path component is the name pattern (`*` when empty).
pub fn plan(query: &str, cwd: &Path, scope: &Scope) -> Result<Plan, String> {
    plan_mode(query, cwd, scope, false)
}

/// Like [`plan`], with `regex` forcing the whole query to be one regular expression; a regex
/// is never treated as a path, even when it contains slashes.
pub fn plan_mode(query: &str, cwd: &Path, scope: &Scope, regex: bool) -> Result<Plan, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err(crate::t!("Search query is empty"));
    }
    // A regular expression is never a path, even when it contains slashes.
    if regex || regex_source(query).is_some() {
        return Ok(Plan { scope: scope.clone(), pattern: query.to_string() });
    }
    // The whole query may be an existing folder (including names with spaces); check it
    // before splitting at the last separator. UNC paths are left to the split, since
    // `is_dir` could stall on an unreachable server.
    if query.contains(['\\', '/']) {
        let folder = match is_absolute_prefix(query) {
            true => PathBuf::from(query),
            false => cwd.join(query),
        };
        let folder = normalize(&folder);
        if is_drive_path(&folder) && folder.is_dir() {
            return Ok(Plan {
                scope: match scope {
                    Scope::Direct(_) => Scope::Direct(folder),
                    _ => Scope::Under(folder),
                },
                pattern: "*".into(),
            });
        }
    }
    let Some(pos) = query.rfind(['\\', '/']) else {
        return Ok(Plan { scope: scope.clone(), pattern: query.to_string() });
    };
    let (prefix, name) = (&query[..pos], query[pos + 1..].trim());
    let mut root = if prefix.is_empty() {
        cwd.to_path_buf()
    } else if is_absolute_prefix(prefix) {
        let mut p = PathBuf::from(prefix);
        if is_drive_prefix(prefix) {
            p.push("\\");
        }
        p
    } else {
        cwd.join(prefix)
    };
    root = normalize(&root);
    // Checking a UNC path could stall on an unreachable server; only local drives are checked.
    let local = is_drive_path(&root);
    if local && !root.is_dir() {
        if is_absolute_prefix(prefix) {
            return Err(crate::t!("Folder not found: {}", root.display()));
        }
        // Not a folder here: the separators belong to path words (`src\ui`, `a;b\c`, `*\logs\*`).
        return Ok(Plan { scope: scope.clone(), pattern: query.to_string() });
    }
    // Typing an existing folder path means "everything inside it"; a trailing name is the pattern.
    let mut pattern = name.to_string();
    if pattern.is_empty()
        || (local && !pattern.contains(['*', '?']) && root.join(&pattern).is_dir())
    {
        if !pattern.is_empty() {
            root = root.join(&pattern);
        }
        pattern = "*".into();
    }
    let direct = matches!(scope, Scope::Direct(_));
    Ok(Plan {
        scope: match direct {
            true => Scope::Direct(root),
            false => Scope::Under(root),
        },
        pattern,
    })
}

fn is_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn is_drive_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Resolves `.` and `..` without touching the file system, so a typed path matches the index's
/// canonical path strings.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut prefix = PathBuf::new();
    let mut parts: Vec<OsString> = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => prefix.push(p.as_os_str()),
            Component::RootDir => prefix.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(n) => parts.push(n.to_os_string()),
        }
    }
    for part in parts {
        prefix.push(part);
    }
    prefix
}

fn is_absolute_prefix(s: &str) -> bool {
    is_drive_prefix(s) || Path::new(s).is_absolute() || s.starts_with(r"\\") || s.starts_with("//")
}

/// Calls `found` (from several threads) for every name below `root` that matches `pattern`,
/// until it returns false. Hidden files are included; ignored files are not skipped.
pub fn walk_names(root: &Path, pattern: &Pattern, found: impl Fn(PathBuf, bool) -> bool + Sync) -> Result<(), String> {
    let stop = AtomicBool::new(false);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    WalkBuilder::new(root)
        .standard_filters(false)
        .threads(threads)
        .build_parallel()
        .run(|| {
            let (pattern, found, stop) = (pattern, &found, &stop);
            Box::new(move |entry| {
                if stop.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else { return WalkState::Continue };
                if entry.depth() == 0 {
                    return WalkState::Continue;
                }
                let Some(kind) = entry.file_type() else { return WalkState::Continue };
                let name = entry.file_name().to_string_lossy();
                if kind.is_dir() && pattern.prunes(&name) {
                    return WalkState::Skip;
                }
                let hit = pattern.matches(&name)
                    && (!pattern.needs_path() || pattern.matches_path(&entry.path().to_string_lossy(), Some(root)));
                if hit && !found(entry.into_path(), kind.is_dir()) {
                    stop.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                WalkState::Continue
            })
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, name: &str) -> bool {
        Pattern::compile(pattern).unwrap().matches(name)
    }

    #[test]
    fn plain_words_match_anywhere() {
        assert!(matches("report", "Report 2024.docx"));
        assert!(matches("REP 2024", "Report 2024.docx"));
        assert!(matches("보고", "보고서.hwp"));
        assert!(!matches("report hwp", "Report 2024.docx"));
        assert_eq!(Pattern::compile("").unwrap_err(), crate::t!("Search query is empty"));
    }

    #[test]
    fn wildcards_cover_the_whole_name() {
        assert!(matches("*.txt", "notes.txt"));
        assert!(matches("*.TXT", "NOTES.txt"));
        assert!(!matches("*.txt", "notes.txt.bak"));
        assert!(matches("report?.docx", "report1.docx"));
        assert!(!matches("report?.docx", "report10.docx"));
        assert!(matches("보고*", "보고서.hwp"));
        assert!(matches("*보고*", "최종 보고서.hwp"));
        assert!(matches("*", "anything"));
        assert!(matches("a*", "abc"));
        assert!(matches("*c", "abc"));
        assert!(!matches("a*c", "ab"));
    }

    #[test]
    fn semicolons_are_or_groups() {
        assert!(matches("*.txt;*.md", "notes.txt"));
        assert!(matches("*.txt;*.md", "notes.MD"));
        assert!(!matches("*.txt;*.md", "notes.doc"));
        assert!(matches("보고서;초안", "최종 보고서.hwp"));
        assert!(matches("보고서;초안", "초안.hwp"));
        assert!(!matches("보고서;초안", "메모.hwp"));
        // Whitespace inside a group is still AND.
        assert!(matches("보고서 2024", "2024-보고서.hwp"));
        assert!(!matches("보고서 2024", "보고서.hwp"));
        assert!(!matches("보고서 2024", "2024.hwp"));
        // a OR (b AND c)
        assert!(matches("a;b c", "a.txt"));
        assert!(!matches("a;b c", "b.txt"));
        assert!(matches("a;b c", "bc.txt"));
        assert!(Pattern::compile(";;").is_err());
    }

    fn path_hit(query: &str, path: &str) -> bool {
        let p = Pattern::compile(query).unwrap();
        let name = path.rsplit('\\').next().unwrap();
        p.matches(name) && p.matches_path(path, Some(Path::new(r"D:\work")))
    }

    #[test]
    fn path_words_and_exclusions() {
        // Path words match the whole path; `**` and `/` work.
        assert!(path_hit(r"src\ui main", r"D:\work\src\ui\main.rs"));
        assert!(!path_hit(r"src\ui main", r"D:\work\src\core\main.rs"));
        assert!(path_hit(r"docs\**\*.md", r"D:\work\docs\a\b\x.md"));
        assert!(path_hit(r"docs\**\*.md", r"D:\work\docs\x.md"));
        assert!(path_hit("docs/*.md", r"D:\work\docs\x.md"));
        assert!(!path_hit(r"docs\*.md", r"D:\work\docs\x.txt"));
        // `!` excludes by any folder or file name below the search root.
        assert!(!path_hit("main !target", r"D:\work\target\debug\main.rs"));
        assert!(path_hit("main !target", r"D:\work\src\main.rs"));
        assert!(!path_hit("main !*.rs", r"D:\work\src\main.rs"));
        assert!(path_hit("!tmp", r"D:\work\a.txt"));
        // Folders above the search root are never excluded.
        assert!(path_hit("main !work", r"D:\work\src\main.rs"));
        // `!` applies per OR group; a lone or embedded `!` is literal.
        assert!(path_hit("a !x;b", r"D:\work\x\b.txt"));
        assert!(!path_hit("a !x;b", r"D:\work\x\a.txt"));
        assert!(path_hit("a!b", r"D:\work\a!b.txt"));
        // Only a folder every group excludes is pruned.
        assert!(Pattern::compile("a !old;b !old").unwrap().prunes("old"));
        assert!(!Pattern::compile("a !old;b").unwrap().prunes("old"));
        // Separators inside OR groups no longer break planning.
        let cwd = std::env::temp_dir();
        let p = plan(r"a;b\c", &cwd, &Scope::All).unwrap();
        assert_eq!(p.pattern, r"a;b\c");
        assert!(path_hit(&p.pattern, r"D:\work\x\b\c.txt"));
        assert!(plan(r"C:\no-such-folder-xyz\a", &cwd, &Scope::All).is_err());
    }

    #[test]
    fn regex_mode_is_explicit() {
        assert!(Pattern::compile_mode(r"^report.*\.docx$", true).unwrap().matches("Report 2024.docx"));
        assert!(!Pattern::compile_mode(r"^report.*\.docx$", true).unwrap().matches("old-report.docx"));
        // In regex mode a semicolon is literal, not an OR separator.
        assert!(Pattern::compile_mode("a;b", true).unwrap().matches("xa;by"));
        assert!(Pattern::compile_mode("^a;b$", true).unwrap().matches("a;b"));
        assert!(!Pattern::compile_mode("^a;b$", false).unwrap().matches("a;b"));
        // The legacy prefix is stripped when the toggle is already on.
        assert!(Pattern::compile_mode("re:^a", true).unwrap().matches("abc"));
        assert!(Pattern::compile_mode("re:(", true).unwrap_err().starts_with(&crate::t!("Regex error: {}", "")));
    }

    #[test]
    fn regex_mode_plan_keeps_slashes() {
        let cwd = PathBuf::from(r"C:\Users\hp");
        let scope = Scope::Under(cwd.clone());
        let p = plan_mode(r"^C:/Users/.*", &cwd, &scope, true).unwrap();
        assert_eq!(p.scope, scope);
        assert_eq!(p.pattern, r"^C:/Users/.*");
    }

    #[test]
    fn regex_forms() {
        assert!(matches(r"re:^report.*\.docx$", "Report 2024.docx"));
        assert!(matches(r"/^report.*\.docx$/", "Report 2024.docx"));
        assert!(!matches(r"re:^report.*\.docx$", "old-report.docx"));
        assert!(Pattern::compile("re:(").unwrap_err().starts_with(&crate::t!("Regex error: {}", "")));
        assert!(Pattern::compile("re:").is_err());
    }

    #[test]
    fn scopes_compare_paths_case_insensitively() {
        let root = Path::new(r"C:\Users\hp");
        let under = Scope::Under(root.to_path_buf());
        assert!(under.includes(Path::new(r"c:\users\HP\Documents\a.txt")));
        assert!(!under.includes(root));
        let direct = Scope::Direct(root.to_path_buf());
        assert!(direct.includes(Path::new(r"C:\USERS\hp\a.txt")));
        assert!(!direct.includes(Path::new(r"C:\Users\hp\Documents\a.txt")));
        assert!(Scope::All.includes(Path::new(r"D:\anything")));
        assert!(under.covers_volume('c'));
        assert!(!under.covers_volume('D'));
        assert!(Scope::All.covers_volume('D'));
    }

    #[test]
    fn normalizes_dot_segments() {
        assert_eq!(normalize(Path::new(r"C:\foo\..\bar")), PathBuf::from(r"C:\bar"));
        assert_eq!(normalize(Path::new(r"C:\foo\.\bar")), PathBuf::from(r"C:\foo\bar"));
        assert_eq!(normalize(Path::new(r"C:\..\..\bar")), PathBuf::from(r"C:\bar"));
        assert_eq!(normalize(Path::new(r"\\server\share\foo\..\bar")), PathBuf::from(r"\\server\share\bar"));
    }

    #[test]
    fn walks_a_tree_for_matching_names() {
        use std::sync::Mutex;
        let dir = std::env::temp_dir().join(format!("fex-walk-{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        for (path, text) in [("a.txt", "a"), ("b.log", "b"), ("sub\\c.TXT", "c")] {
            std::fs::write(dir.join(path), text).unwrap();
        }
        let hits = Mutex::new(Vec::new());
        let pattern = Pattern::compile("*.txt").unwrap();
        walk_names(&dir, &pattern, |path, dir| {
            assert!(!dir);
            hits.lock().unwrap().push(path.file_name().unwrap().to_string_lossy().into_owned());
            true
        })
        .unwrap();
        let mut hits = hits.into_inner().unwrap();
        hits.sort();
        assert_eq!(hits, ["a.txt", "c.TXT"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plan_splits_folder_queries() {
        let cwd = std::env::temp_dir().join(format!("fex-plan-{}", std::process::id()));
        let sub = cwd.join("sub");
        let spaced = cwd.join("Program Files");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&spaced).unwrap();
        let all = Scope::All;
        let q = format!(r"{}\*.txt", sub.display());
        let p = plan(&q, &cwd, &all).unwrap();
        assert_eq!(p.scope, Scope::Under(sub.clone()));
        assert_eq!(p.pattern, "*.txt");
        let p = plan(r"sub\report", &cwd, &Scope::Direct(cwd.clone())).unwrap();
        assert_eq!(p.scope, Scope::Direct(sub.clone()));
        assert_eq!(p.pattern, "report");
        let p = plan(r"sub\..\*.txt", &cwd, &Scope::Under(cwd.clone())).unwrap();
        assert_eq!(p.scope, Scope::Under(cwd.clone()));
        assert_eq!(p.pattern, "*.txt");
        // A path naming an existing folder searches everything in it.
        let q = format!(r"{}\{}", cwd.display(), "sub");
        let p = plan(&q, &cwd, &all).unwrap();
        assert_eq!(p.scope, Scope::Under(sub.clone()));
        assert_eq!(p.pattern, "*");
        // Folder names with spaces work without a trailing separator.
        let p = plan(&spaced.display().to_string(), &cwd, &all).unwrap();
        assert_eq!(p.scope, Scope::Under(spaced.clone()));
        assert_eq!(p.pattern, "*");
        let q = format!(r"{}\*.txt", spaced.display());
        let p = plan(&q, &cwd, &all).unwrap();
        assert_eq!(p.scope, Scope::Under(spaced.clone()));
        assert_eq!(p.pattern, "*.txt");
        // Regular expressions keep their slashes.
        let p = plan(r"/^2024.*\.txt$/", &cwd, &all).unwrap();
        assert_eq!(p.scope, Scope::All);
        assert_eq!(p.pattern, r"/^2024.*\.txt$/");
        std::fs::remove_dir_all(&cwd).unwrap();
    }
}
