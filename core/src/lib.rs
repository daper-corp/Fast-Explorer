//! Fex core: UI-independent engine (directory listing, sorting, formatting, shell actions).

pub mod icons;
pub mod index;
pub mod locks;
pub mod menu;
pub mod rename;
pub mod search;
pub mod shell;
pub mod watch;

use std::cmp::Ordering;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_INFORMATION, FileDirectoryInformation, NtQueryDirectoryFile,
};
use windows::Win32::Foundation::{FILETIME, STATUS_NO_MORE_FILES, STATUS_NO_SUCH_FILE, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Shell::StrCmpLogicalW;
use windows::core::{Owned, PCWSTR};

/// Entries handed to the UI per batch.
pub const BATCH: usize = 4096;
const BUF_BYTES: usize = 1 << 20;

#[derive(Clone)]
pub struct Entry {
    /// UTF-16, NUL-terminated so it can go straight to StrCmpLogicalW.
    name: Box<[u16]>,
    pub size: u64,
    /// Raw FILETIME (100ns ticks since 1601, UTC).
    pub modified: i64,
    pub attrs: u32,
}

impl Entry {
    /// An entry that didn't come from a listing, e.g. a search hit.
    pub fn new(name: &std::ffi::OsStr, size: u64, modified: i64, attrs: u32) -> Self {
        Entry { name: name.encode_wide().chain(Some(0)).collect(), size, modified, attrs }
    }

    /// Display name (lossy). Build paths with [`Entry::os_name`], which keeps the exact name.
    pub fn name(&self) -> String {
        String::from_utf16_lossy(&self.name[..self.name.len() - 1])
    }

    pub fn os_name(&self) -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&self.name[..self.name.len() - 1])
    }

    pub fn is_dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    }

    /// Extension without the dot, NUL-terminated (empty for folders / no dot).
    fn ext_wide(&self) -> &[u16] {
        let end = self.name.len() - 1;
        match self.name[..end].iter().rposition(|&c| c == '.' as u16) {
            Some(i) if i > 0 && !self.is_dir() => &self.name[i + 1..],
            _ => &self.name[end..],
        }
    }

    pub fn extension(&self) -> String {
        let e = self.ext_wide();
        String::from_utf16_lossy(&e[..e.len() - 1])
    }
}

/// Lists `dir` with NtQueryDirectoryFile and a 1 MB buffer, handing entries to `on_batch`
/// in chunks of up to [`BATCH`]. Size/date/attributes come straight from the listing.
/// Return `false` from `on_batch` to cancel.
pub fn list_dir(dir: &Path, mut on_batch: impl FnMut(Vec<Entry>) -> bool) -> io::Result<()> {
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        Owned::new(CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )?)
    };

    let mut buf = vec![0u64; BUF_BYTES / 8]; // 8-byte aligned, as the records require
    let mut batch = Vec::with_capacity(BATCH);
    loop {
        let mut iosb = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtQueryDirectoryFile(
                *handle,
                None,
                None,
                None,
                &mut iosb,
                buf.as_mut_ptr().cast(),
                BUF_BYTES as u32,
                FileDirectoryInformation,
                false,
                None,
                false,
            )
        };
        if status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE {
            break;
        }
        status.ok()?;

        let base = buf.as_ptr().cast::<u8>();
        let mut off = 0usize;
        loop {
            // Raw pointers derived from `base` so reading past the declared [u16; 1] name is sound.
            let p = unsafe { base.add(off) }.cast::<FILE_DIRECTORY_INFORMATION>();
            let (next, name) = unsafe {
                let len = (*p).FileNameLength as usize / 2;
                let name = std::slice::from_raw_parts(std::ptr::addr_of!((*p).FileName).cast::<u16>(), len);
                ((*p).NextEntryOffset as usize, name)
            };
            if name != [46] && name != [46, 46] {
                let mut n = Vec::with_capacity(name.len() + 1);
                n.extend_from_slice(name);
                n.push(0);
                unsafe {
                    batch.push(Entry {
                        name: n.into_boxed_slice(),
                        size: (*p).EndOfFile as u64,
                        modified: (*p).LastWriteTime,
                        attrs: (*p).FileAttributes,
                    });
                }
                if batch.len() == BATCH && !on_batch(std::mem::replace(&mut batch, Vec::with_capacity(BATCH))) {
                    return Ok(());
                }
            }
            if next == 0 {
                break;
            }
            off += next;
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SortKey {
    Name,
    Modified,
    Type,
    Size,
}

/// Returns a display order (indices into `entries`). Folders first; names compare like Explorer.
// ponytail: StrCmpLogicalW per comparison, ~0.5 s for 100k names (off the UI thread).
// Precomputed sort keys or a parallel sort if that becomes noticeable.
pub fn sort_order(entries: &[Entry], key: SortKey, descending: bool) -> Vec<u32> {
    let mut order: Vec<u32> = (0..entries.len() as u32).collect();
    order.sort_unstable_by(|&a, &b| {
        let (a, b) = (&entries[a as usize], &entries[b as usize]);
        b.is_dir().cmp(&a.is_dir()).then_with(|| {
            let ord = match key {
                SortKey::Name => Ordering::Equal,
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Type => logical_cmp(a.ext_wide(), b.ext_wide()),
            }
            .then_with(|| logical_cmp(&a.name, &b.name));
            if descending { ord.reverse() } else { ord }
        })
    });
    order
}

/// Explorer's natural, case-insensitive order. Both slices must be NUL-terminated.
fn logical_cmp(a: &[u16], b: &[u16]) -> Ordering {
    unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) }.cmp(&0)
}

/// FILETIME -> "YYYY-MM-DD HH:MM" in local time.
pub fn format_filetime(ft: i64) -> String {
    if ft <= 0 {
        return String::new(); // unknown
    }
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return String::new();
        }
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}

/// Explorer-style size: KB rounded up, with thousands separators ("1,234 KB").
pub fn format_size(bytes: u64) -> String {
    let digits = bytes.div_ceil(1024).to_string();
    let mut out = String::with_capacity(digits.len() + 4);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out + " KB"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn names(dir: &Path, key: SortKey) -> Vec<String> {
        let mut all = Vec::new();
        list_dir(dir, |b| {
            all.extend(b);
            true
        })
        .unwrap();
        sort_order(&all, key, false).iter().map(|&i| all[i as usize].name()).collect()
    }

    #[test]
    fn lists_and_sorts_like_explorer() {
        let dir = std::env::temp_dir().join(format!("fex-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("zdir")).unwrap();
        for f in ["a10.txt", "a2.txt", "B.log"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        assert_eq!(names(&dir, SortKey::Name), ["zdir", "a2.txt", "a10.txt", "B.log"]);
        assert_eq!(names(&dir, SortKey::Type), ["zdir", "B.log", "a2.txt", "a10.txt"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system32_matches_std_and_reports_timing() {
        let dir = Path::new(r"C:\Windows\System32");
        let t = Instant::now();
        let (mut n, mut batches) = (0, 0);
        list_dir(dir, |b| {
            n += b.len();
            batches += 1;
            true
        })
        .unwrap();
        let elapsed = t.elapsed();
        assert_eq!(n, std::fs::read_dir(dir).unwrap().count());
        eprintln!("System32: {n} entries in {batches} batches, {elapsed:?}");
    }

    #[test]
    fn formats_size() {
        assert_eq!(format_size(0), "0 KB");
        assert_eq!(format_size(1), "1 KB");
        assert_eq!(format_size(1_263_616), "1,234 KB");
    }
}
