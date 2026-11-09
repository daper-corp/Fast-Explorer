//! Batch rename ("PowerRename"-style): a find/replace rule applied to many names, with a
//! preview that flags what cannot be applied. Pure logic; the file system is only read to
//! detect name clashes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};

/// Which part of the name the rule edits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Part {
    /// Name without the extension (folders: the whole name).
    #[default]
    Stem,
    Ext,
    Full,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    #[default]
    Keep,
    Lower,
    Upper,
}

#[derive(Clone, Debug, Default)]
pub struct Rule {
    pub find: String,
    /// With `regex`, `$1`/`${name}` refer to groups. `{n}` / `{n:3}` is the item number (1-based, padded).
    pub replace: String,
    pub regex: bool,
    pub match_case: bool,
    /// Replace every match instead of only the first.
    pub all: bool,
    pub part: Part,
    pub case: Case,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Unchanged,
    Ok,
    /// Why this item cannot be renamed.
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Planned {
    pub from: PathBuf,
    pub new_name: String,
    pub status: Status,
}

const INVALID: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Splits "a.tar.gz" into ("a.tar", "gz"); folders and dotfiles have no extension.
fn split(name: &str, dir: bool) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && !dir => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// Compiles the rule's search text. An empty search matches nothing (only case changes apply).
pub fn compile(rule: &Rule) -> Result<Option<Regex>, String> {
    if rule.find.is_empty() {
        return Ok(None);
    }
    let pat = if rule.regex { rule.find.clone() } else { regex::escape(&rule.find) };
    RegexBuilder::new(&pat)
        .case_insensitive(!rule.match_case)
        .build()
        .map(Some)
        .map_err(|e| format!("정규식 오류: {}", e.to_string().lines().last().unwrap_or("")))
}

/// Expands `{n}` and `{n:W}` in the replacement text.
fn expand_counter(text: &str, n: usize) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("{n") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let (width, used) = match tail.strip_prefix(':') {
            Some(t) => {
                let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
                (digits.parse::<usize>().unwrap_or(0).min(12), 1 + digits.len())
            }
            None => (0, 0),
        };
        if tail[used..].starts_with('}') {
            out.push_str(&format!("{n:0width$}"));
            rest = &tail[used + 1..];
        } else {
            out.push_str("{n");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// "$1_x" -> "${1}_x": the regex crate would read `$1_` as a group named "1_".
fn braced_groups(rep: &str) -> String {
    let mut out = String::new();
    let mut chars = rep.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
        } else if chars.peek() == Some(&'$') {
            out.push_str("$$");
            chars.next();
        } else if chars.peek().is_some_and(char::is_ascii_digit) {
            let mut n = String::new();
            while let Some(d) = chars.next_if(char::is_ascii_digit) {
                n.push(d);
            }
            out.push_str(&format!("${{{n}}}"));
        } else {
            out.push('$');
        }
    }
    out
}

fn edit(re: &Option<Regex>, rule: &Rule, text: &str, n: usize) -> String {
    let mut s = match re {
        None => text.to_string(),
        Some(re) => {
            let rep = expand_counter(&rule.replace, n);
            let rep = if rule.regex { braced_groups(&rep) } else { rep.replace('$', "$$") };
            if rule.all { re.replace_all(text, rep.as_str()).into_owned() } else { re.replace(text, rep.as_str()).into_owned() }
        }
    };
    match rule.case {
        Case::Keep => {}
        Case::Lower => s = s.to_lowercase(),
        Case::Upper => s = s.to_uppercase(),
    }
    s
}

/// The new name for `name` under `rule` (`n` is the 1-based item number).
pub fn new_name(re: &Option<Regex>, rule: &Rule, name: &str, dir: bool, n: usize) -> String {
    let (stem, ext) = split(name, dir);
    match rule.part {
        Part::Stem => {
            let s = edit(re, rule, stem, n);
            if ext.is_empty() && stem.len() == name.len() { s } else { format!("{s}.{ext}") }
        }
        Part::Ext => {
            if ext.is_empty() {
                return name.to_string();
            }
            let e = edit(re, rule, ext, n);
            if e.is_empty() { stem.to_string() } else { format!("{stem}.{e}") }
        }
        Part::Full => edit(re, rule, name, n),
    }
}

fn key(name: &str) -> String {
    name.to_lowercase()
}

/// Previews `rule` over `items` (path, is-folder). `exists` tells whether a path is taken on disk.
pub fn plan(rule: &Rule, items: &[(PathBuf, bool)], exists: impl Fn(&Path) -> bool) -> Result<Vec<Planned>, String> {
    let re = compile(rule)?;
    let mut out: Vec<Planned> = items
        .iter()
        .enumerate()
        .map(|(i, (path, dir))| {
            let old = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let new = new_name(&re, rule, &old, *dir, i + 1);
            let status = if new == old {
                Status::Unchanged
            } else if new.trim().is_empty() {
                Status::Error("이름이 비어 있음".into())
            } else if new.contains(INVALID) {
                Status::Error("이름에 \\ / : * ? \" < > | 를 쓸 수 없음".into())
            } else if new.ends_with(['.', ' ']) {
                Status::Error("이름 끝에 점이나 공백을 쓸 수 없음".into())
            } else {
                Status::Ok
            };
            Planned { from: path.clone(), new_name: new, status }
        })
        .collect();

    // Clashes: two items to one name, or a name already on disk that is not itself being renamed away.
    let mut by_target: HashMap<(PathBuf, String), usize> = HashMap::new();
    for p in out.iter().filter(|p| p.status == Status::Ok) {
        *by_target.entry((p.from.parent().unwrap_or(Path::new("")).to_path_buf(), key(&p.new_name))).or_default() += 1;
    }
    let moving: std::collections::HashSet<(PathBuf, String)> = out
        .iter()
        .filter(|p| p.status == Status::Ok)
        .map(|p| (p.from.parent().unwrap_or(Path::new("")).to_path_buf(), key(&p.from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())))
        .collect();
    for p in out.iter_mut().filter(|p| p.status == Status::Ok) {
        let k = (p.from.parent().unwrap_or(Path::new("")).to_path_buf(), key(&p.new_name));
        if by_target[&k] > 1 {
            p.status = Status::Error("같은 이름이 둘 이상 생김".into());
        } else if !moving.contains(&k) && exists(&p.from.with_file_name(&p.new_name)) {
            p.status = Status::Error("같은 이름이 이미 있음".into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Vec<(PathBuf, bool)> {
        names.iter().map(|n| (PathBuf::from(format!("/d/{n}")), false)).collect()
    }
    fn rule(find: &str, replace: &str) -> Rule {
        Rule { find: find.into(), replace: replace.into(), all: true, ..Default::default() }
    }
    fn names(p: &[Planned]) -> Vec<&str> {
        p.iter().map(|p| p.new_name.as_str()).collect()
    }

    #[test]
    fn plain_replace_keeps_extension() {
        let p = plan(&rule("img", "photo"), &items(&["IMG_1.jpg", "a.txt"]), |_| false).unwrap();
        assert_eq!(names(&p), ["photo_1.jpg", "a.txt"]);
        assert_eq!(p[1].status, Status::Unchanged);
    }

    #[test]
    fn literal_dollar_and_regex_groups() {
        let p = plan(&rule("a", "$1"), &items(&["a.txt"]), |_| false).unwrap();
        assert_eq!(names(&p), ["$1.txt"]);
        let r = Rule { regex: true, ..rule(r"(\d+)-(\w+)", "$2_$1") };
        let p = plan(&r, &items(&["12-ab.txt"]), |_| false).unwrap();
        assert_eq!(names(&p), ["ab_12.txt"]);
    }

    #[test]
    fn counter_and_parts() {
        let r = Rule { regex: true, ..rule("^.*$", "pic{n:3}") };
        let p = plan(&r, &items(&["x.png", "y.png"]), |_| false).unwrap();
        assert_eq!(names(&p), ["pic001.png", "pic002.png"]);
        let r = Rule { part: Part::Ext, ..rule("jpeg", "jpg") };
        assert_eq!(names(&plan(&r, &items(&["a.jpeg"]), |_| false).unwrap()), ["a.jpg"]);
        let r = Rule { case: Case::Upper, part: Part::Full, ..Default::default() };
        assert_eq!(names(&plan(&r, &items(&["a.txt"]), |_| false).unwrap()), ["A.TXT"]);
        assert_eq!(new_name(&None, &rule("", ""), ".gitignore", false, 1), ".gitignore");
    }

    #[test]
    fn first_only_and_case_sensitivity() {
        let r = Rule { all: false, ..rule("a", "b") };
        assert_eq!(names(&plan(&r, &items(&["aaa.txt"]), |_| false).unwrap()), ["baa.txt"]);
        let r = Rule { match_case: true, ..rule("A", "b") };
        assert_eq!(plan(&r, &items(&["aaa.txt"]), |_| false).unwrap()[0].status, Status::Unchanged);
    }

    #[test]
    fn flags_problems() {
        let p = plan(&rule("x", "?"), &items(&["x.txt"]), |_| false).unwrap();
        assert!(matches!(p[0].status, Status::Error(_)));
        let r = Rule { regex: true, part: Part::Full, ..rule(".*", "") };
        let p = plan(&r, &items(&["x.txt"]), |_| false).unwrap();
        assert!(matches!(p[0].status, Status::Error(_)));
        let p = plan(&rule("[ab]", "z"), &items(&["a.txt", "b.txt"]), |_| false).unwrap();
        assert!(p.iter().all(|p| p.status == Status::Unchanged)); // not regex: literal "[ab]" matches nothing
        let r = Rule { regex: true, ..rule("[ab]", "z") };
        let p = plan(&r, &items(&["a.txt", "b.txt"]), |_| false).unwrap();
        assert!(p.iter().all(|p| matches!(p.status, Status::Error(_))));
        let p = plan(&rule("a", "b"), &items(&["a.txt"]), |p| p.ends_with("b.txt")).unwrap();
        assert!(matches!(p[0].status, Status::Error(_)));
        // b -> c while a -> b is fine: b is being renamed away.
        let p = plan(&Rule { regex: true, ..rule("^a$|^b$", "x") }, &items(&["a"]), |_| false).unwrap();
        assert_eq!(p[0].status, Status::Ok);
        assert!(plan(&Rule { regex: true, ..rule("(", "") }, &items(&["a"]), |_| false).is_err());
    }
}
