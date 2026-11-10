//! UI text: English is the source language, Korean the translation. Every user-visible Rust
//! string goes through `t!("English text {}", arg)`; Slint text uses `@tr("...")` (ui/lang/ko).
//! Translations are tab-separated `English<TAB>Korean` lines in `core/i18n/*.tsv` (`\n` = newline).
//! `FEX_LANG=ko|en` overrides the Windows display language.

use std::collections::HashMap;
use std::sync::OnceLock;

use windows::Win32::Globalization::GetUserDefaultUILanguage;

const TABLES: [&str; 3] = [
    include_str!("../i18n/core.tsv"),
    include_str!("../i18n/ui_main.tsv"),
    include_str!("../i18n/ui_other.tsv"),
];

/// Language chosen at runtime: 0 = automatic, 1 = English, 2 = Korean.
static CHOICE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Picks the UI language ("en", "ko", anything else = automatic). Takes effect on the next `t!`.
pub fn set_lang(lang: &str) {
    let v = match lang {
        "en" => 1,
        "ko" => 2,
        _ => 0,
    };
    CHOICE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// True when the UI language is Korean.
pub fn korean() -> bool {
    match CHOICE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    static KO: OnceLock<bool> = OnceLock::new();
    *KO.get_or_init(|| match std::env::var("FEX_LANG") {
        Ok(v) => v.eq_ignore_ascii_case("ko"),
        // Primary language id 0x12 = Korean.
        Err(_) => (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == 0x12,
    })
}

/// Parses translation tables into (English, Korean) pairs.
pub fn pairs(table: &str) -> impl Iterator<Item = (String, String)> + '_ {
    table.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(|l| {
        let (en, ko) = l.split_once('\t')?;
        Some((en.replace("\\n", "\n"), ko.replace("\\n", "\n")))
    })
}

/// Korean text for `en` (or `en` itself when English is active or no translation exists).
pub fn tr(en: &str) -> String {
    if !korean() {
        return en.to_string();
    }
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    let map = MAP.get_or_init(|| TABLES.iter().flat_map(|t| pairs(t)).collect());
    map.get(en).cloned().unwrap_or_else(|| en.to_string())
}

/// Fills `{}` placeholders in order.
pub fn fmt(mut s: String, args: &[String]) -> String {
    for a in args {
        match s.find("{}") {
            Some(i) => s.replace_range(i..i + 2, a),
            None => break,
        }
    }
    s
}

/// `t!("text {}", x)`: translate the literal, then fill the `{}`s. Arguments need `Display`.
#[macro_export]
macro_rules! t {
    ($s:literal $(, $a:expr)* $(,)?) => {
        $crate::i18n::fmt($crate::i18n::tr($s), &[$($a.to_string()),*])
    };
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Every `t!("...")` / `tr("...")` literal in the sources must have a Korean entry.
    #[test]
    fn every_literal_is_translated() {
        let known: std::collections::HashSet<String> = super::TABLES.iter().flat_map(|t| super::pairs(t)).map(|p| p.0).collect();
        let re = regex::Regex::new(r#"\b(?:t!|tr)\(\s*"((?:[^"\\]|\\.)*)""#).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut missing = Vec::new();
        for dir in ["core/src", "ui/src", "shell-host/src", "index-service/src"] {
            for e in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "rs") || p.ends_with("i18n.rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&p).unwrap();
                for c in re.captures_iter(&src) {
                    let s = c[1].replace("\\n", "\n").replace("\\\"", "\"").replace("\\\\", "\\");
                    if !known.contains(&s) {
                        missing.push(format!("{}: {}", p.display(), &c[1]));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "untranslated:\n{}", missing.join("\n"));
    }

    /// Every `@tr("...")` in app.slint needs a non-empty Korean msgstr in ui/lang/ko.
    #[test]
    fn slint_texts_are_translated() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let slint = std::fs::read_to_string(root.join("ui/app.slint")).unwrap();
        let po = std::fs::read_to_string(root.join("ui/lang/ko/LC_MESSAGES/fex.po")).unwrap();
        let unq = |s: &str| s.replace("\\\"", "\"").replace("\\\\", "\\");
        let mut done = std::collections::HashSet::new();
        let mut lines = po.lines().peekable();
        while let Some(l) = lines.next() {
            if let (Some(id), Some(Some(ko))) = (l.strip_prefix("msgid \""), lines.peek().map(|n| n.strip_prefix("msgstr \""))) {
                if ko.len() > 1 {
                    done.insert(unq(id.trim_end_matches('"')));
                }
            }
        }
        let re = regex::Regex::new(r#"@tr\("((?:[^"\\]|\\.)*)""#).unwrap();
        let missing: Vec<_> = re.captures_iter(&slint).map(|c| unq(&c[1])).filter(|s| !done.contains(s)).collect();
        assert!(missing.is_empty(), "untranslated in app.slint: {missing:?}");
    }

    #[test]
    fn tables_are_wellformed() {
        for t in super::TABLES {
            for l in t.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let (en, ko) = l.split_once('\t').unwrap_or_else(|| panic!("no tab: {l}"));
                assert!(!en.is_empty() && !ko.is_empty() && !ko.contains('\t'), "bad line: {l}");
                assert_eq!(en.matches("{}").count(), ko.matches("{}").count(), "placeholder mismatch: {l}");
            }
        }
    }
}
