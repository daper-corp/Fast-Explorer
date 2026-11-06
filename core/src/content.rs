//! Searching inside files (ripgrep's engine: `ignore` walks, `grep-searcher` reads).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};

/// Larger files are skipped (logs, disk images, ...).
const MAX_FILE: u64 = 64 << 20;

/// Calls `found` (from several threads) with every file under `root` whose text contains
/// `query`, case-insensitively, until it returns false. Reads UTF-8, UTF-16 with a BOM, and
/// for non-ASCII queries also CP949 (older Korean text files). Binary files are skipped, and
/// unlike ripgrep nothing is hidden by .gitignore or the hidden attribute: it's a file explorer.
pub fn search(root: &Path, query: &str, found: impl Fn(PathBuf) -> bool + Sync) -> Result<(), String> {
    let matcher = matcher(query)?;
    let stop = AtomicBool::new(false);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    WalkBuilder::new(root)
        .standard_filters(false)
        .max_filesize(Some(MAX_FILE))
        .threads(threads)
        .build_parallel()
        .run(|| {
            let mut searcher = SearcherBuilder::new().binary_detection(BinaryDetection::quit(0)).line_number(false).build();
            let (matcher, stop, found) = (&matcher, &stop, &found);
            Box::new(move |entry| {
                if stop.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else { return WalkState::Continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let mut hit = FirstMatch(false);
                let _ = searcher.search_path(matcher, entry.path(), &mut hit);
                if hit.0 && !found(entry.into_path()) {
                    stop.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                WalkState::Continue
            })
        });
    Ok(())
}

/// Notes that a file matched and stops reading it (no line counting needed).
struct FirstMatch(bool);

impl Sink for FirstMatch {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.0 = true;
        Ok(false)
    }
}

/// The query as a literal, case-insensitive; plus its CP949 bytes when it isn't ASCII.
fn matcher(query: &str) -> Result<RegexMatcher, String> {
    if query.is_empty() {
        return Err("검색어가 비어 있음".into());
    }
    let mut pattern = String::from("(?i:");
    for c in query.chars() {
        if regex_meta(c) {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push(')');
    if !query.is_ascii() {
        let (bytes, _, unmappable) = encoding_rs::EUC_KR.encode(query);
        if !unmappable {
            pattern.push_str("|(?-u:");
            for b in bytes.iter() {
                let _ = write!(pattern, "\\x{b:02X}");
            }
            pattern.push(')');
        }
    }
    RegexMatcherBuilder::new().build(&pattern).map_err(|e| e.to_string())
}

fn regex_meta(c: char) -> bool {
    "\\.+*?()|[]{}^$#&-~".contains(c) || c.is_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn finds_text_in_utf8_utf16_and_cp949_but_not_binary() {
        let dir = std::env::temp_dir().join(format!("fex-content-{}", std::process::id()));
        let sub = dir.join("deep").join("er");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("a.txt"), "Hello World (v1.0) 보고서 초안").unwrap();
        std::fs::write(sub.join("b.md"), "nothing here").unwrap();
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("UTF-16 보고서".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        std::fs::write(sub.join("c.txt"), utf16).unwrap();
        let (cp949, _, _) = encoding_rs::EUC_KR.encode("옛날 보고서 파일");
        std::fs::write(dir.join("d.txt"), &cp949[..]).unwrap();
        std::fs::write(dir.join("e.bin"), b"\0\0\x01binary hello world").unwrap();
        std::fs::write(dir.join(".hidden.txt"), "hello world").unwrap();

        let find = |q: &str| {
            let hits = Mutex::new(Vec::new());
            search(&dir, q, |p| {
                hits.lock().unwrap().push(p.file_name().unwrap().to_string_lossy().into_owned());
                true
            })
            .unwrap();
            let mut v = hits.into_inner().unwrap();
            v.sort();
            v
        };
        assert_eq!(find("hello WORLD"), [".hidden.txt", "a.txt"]);
        assert_eq!(find("(v1.0)"), ["a.txt"]); // regex characters are literal
        assert_eq!(find("보고서"), ["a.txt", "c.txt", "d.txt"]);
        assert!(find("zzz-none").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
