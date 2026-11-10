//! Update check against the latest GitHub release. Uses the curl.exe that ships with Windows 10+,
//! so no HTTP dependency. Nothing is installed silently: the caller asks, then runs the installer.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const REPO: &str = "daper-corp/Fast-Explorer";
const CHECK_EVERY: u64 = 24 * 3600;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub struct Release {
    pub version: String,
    pub url: String,
}

fn curl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("curl.exe")
        .args(["-sSLf", "-m", "20", "-H", "User-Agent: fast-explorer"])
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The string value of `"key": "..."` in a flat JSON object.
fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let rest = &json[json.find(&format!("\"{key}\""))? + key.len() + 2..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(&rest[..rest.find('"')?])
}

/// `latest.json`, published with every release: `{"version": "0.2.0", "url": "https://..."}`.
pub fn parse(json: &str) -> Option<Release> {
    let version = field(json, "version")?.trim_start_matches('v');
    let valid = !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.');
    valid.then(|| Release { version: version.to_string(), url: field(json, "url").unwrap_or("").to_string() })
}

/// Newest release, if it is newer than this build. `releases/latest/download/` is a plain
/// redirect, not the rate-limited REST API (60 requests/hour per IP without a token).
pub fn check() -> Result<Option<Release>, String> {
    let json = curl(&[&format!("https://github.com/{REPO}/releases/latest/download/latest.json")])?;
    let rel = parse(&json).ok_or("업데이트 정보를 읽을 수 없습니다")?;
    Ok(newer(&rel.version, CURRENT).then_some(rel))
}

fn newer(a: &str, b: &str) -> bool {
    let v = |s: &str| s.split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    v(a) > v(b)
}

/// Downloads the installer to the temp folder. Only our own repository's release assets.
pub fn download(rel: &Release) -> Result<PathBuf, String> {
    if !rel.url.starts_with(&format!("https://github.com/{REPO}/releases/download/")) {
        return Err("예상하지 못한 다운로드 주소".into());
    }
    let file = std::env::temp_dir().join(format!("FastExplorer-Setup-{}.exe", rel.version));
    curl(&["-o", &file.to_string_lossy(), &rel.url])?;
    Ok(file)
}

fn stamp_file() -> Option<PathBuf> {
    fex_core::shell::roaming_appdata().ok().map(|d| d.join("Fast Explorer").join("update.txt"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `update.txt`: (last check in unix seconds, automatic checks on).
fn load() -> (u64, bool) {
    let text = stamp_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
    let last = text.lines().find_map(|l| l.strip_prefix("last=")).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
    (last, !text.lines().any(|l| l.trim() == "auto=0"))
}

fn save(last: u64, auto: bool) {
    let Some(file) = stamp_file() else { return };
    let _ = std::fs::create_dir_all(file.parent().unwrap());
    let _ = std::fs::write(file, format!("last={last}\r\nauto={}\r\n", auto as u8));
}

pub fn auto_enabled() -> bool {
    load().1
}

pub fn set_auto(on: bool) {
    save(load().0, on);
}

/// Automatic checks are on and the last one was a day or more ago.
pub fn auto_due() -> bool {
    let (last, auto) = load();
    auto && now().saturating_sub(last) >= CHECK_EVERY
}

pub fn mark_checked() {
    save(now(), auto_enabled());
}

fn session_file() -> Option<PathBuf> {
    fex_core::shell::roaming_appdata().ok().map(|d| d.join("Fast Explorer").join("session.txt"))
}

/// Remembers the open tabs (active index first, then one folder per line) across an update.
pub fn save_session(active: usize, tabs: &[PathBuf]) {
    let Some(file) = session_file() else { return };
    let _ = std::fs::create_dir_all(file.parent().unwrap());
    let body: String = tabs.iter().map(|p| format!("{}
", p.display())).collect();
    let _ = std::fs::write(file, format!("{active}
{body}"));
}

/// The tabs saved by `save_session`, consumed so it applies to one launch only.
pub fn take_session() -> Option<(usize, Vec<PathBuf>)> {
    let file = session_file()?;
    let text = std::fs::read_to_string(&file).ok()?;
    let _ = std::fs::remove_file(&file);
    let mut lines = text.lines();
    let active = lines.next()?.trim().parse().ok()?;
    let tabs: Vec<PathBuf> = lines.filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    (!tabs.is_empty()).then_some((active, tabs))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_latest_json_and_compares_versions() {
        let r = super::parse(r#"{ "version": "v0.2.1", "url": "https://github.com/x/y/releases/download/v0.2.1/FastExplorer-Setup-0.2.1.exe" }"#).unwrap();
        assert_eq!(r.version, "0.2.1");
        assert!(r.url.ends_with("FastExplorer-Setup-0.2.1.exe"));
        assert!(super::parse(r#"{"version": "..\evil", "url": ""}"#).is_none());
        assert!(super::newer("0.10.0", "0.9.9") && !super::newer("0.1.0", "0.1.0"));
    }
}
