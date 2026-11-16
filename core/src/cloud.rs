//! Cloud sync activity (OneDrive, Synology Drive, ...): is a file uploading or downloading, and how far.
//! The provider reports it through shell properties, which Explorer's sync icons read too.
//! Call from a worker thread: binding a property store can block.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Storage::CloudFilters::{CF_SYNC_ROOT_BASIC_INFO, CF_SYNC_ROOT_INFO_BASIC, CfGetSyncRootInfoByPath};
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantGetElementCount, PropVariantGetUInt32Elem, PropVariantToUInt32,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::PropertiesSystem::{GPS_DEFAULT, IPropertyStore, SHGetPropertyStoreFromParsingName};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::core::{GUID, PCWSTR};

// System.StorageProviderState / System.StorageProviderTransferProgress (propkey.h).
const FMTID: GUID = GUID::from_u128(0xE77E90DF_6271_4F5B_834F_2DD1F245DDA4);
const STATE: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID, pid: 3 };
const PROGRESS: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID, pid: 4 };

// STORAGEPROVIDERSTATE_* values.
const PENDING_UPLOAD: u32 = 4;
const PENDING_DOWNLOAD: u32 = 5;
const TRANSFERRING: u32 = 6;

/// FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE: the data is still on the server.
const ONLINE_ONLY: u32 = 0x40_0000 | 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Upload,
    Download,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transfer {
    pub kind: Kind,
    /// 0..=1, or `None` while the provider has not reported progress (queued, or no progress support).
    pub fraction: Option<f32>,
}

/// Whether `path` lies inside a registered cloud sync root; only those can be transferring.
pub fn in_sync_root(path: &Path) -> bool {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut info = CF_SYNC_ROOT_BASIC_INFO::default();
    unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(wide.as_ptr()),
            CF_SYNC_ROOT_INFO_BASIC,
            (&raw mut info).cast(),
            size_of::<CF_SYNC_ROOT_BASIC_INFO>() as u32,
            None,
        )
        .is_ok()
    }
}

/// Enables the shell calls below on this thread.
pub fn init_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// The file's current transfer, or `None` when it is idle (or not a cloud file).
pub fn transfer(path: &Path, attrs: u32) -> Option<Transfer> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreFromParsingName(PCWSTR(wide.as_ptr()), None, GPS_DEFAULT).ok()?;
        let state = PropVariantToUInt32(&store.GetValue(&STATE).ok()?).ok()?;
        if !matches!(state, PENDING_UPLOAD | PENDING_DOWNLOAD | TRANSFERRING) {
            return None;
        }
        let progress = store.GetValue(&PROGRESS).ok().and_then(|v| {
            (PropVariantGetElementCount(&v) == 2)
                .then(|| Some((PropVariantGetUInt32Elem(&v, 0).ok()?, PropVariantGetUInt32Elem(&v, 1).ok()?)))
                .flatten()
        });
        classify(state, attrs, progress)
    }
}

/// `progress` is the provider's pair of values, documented as (max, current); a swapped pair still works.
fn classify(state: u32, attrs: u32, progress: Option<(u32, u32)>) -> Option<Transfer> {
    let kind = match state {
        PENDING_UPLOAD => Kind::Upload,
        PENDING_DOWNLOAD => Kind::Download,
        // The state doesn't say which way; data still on the server means it is being fetched.
        TRANSFERRING if attrs & ONLINE_ONLY != 0 => Kind::Download,
        TRANSFERRING => Kind::Upload,
        _ => return None,
    };
    let fraction = progress.and_then(|(a, b)| {
        let total = a.max(b);
        (total > 0).then(|| a.min(b) as f32 / total as f32)
    });
    Some(Transfer { kind, fraction })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_states() {
        let up = |s, a, p| classify(s, a, p).unwrap();
        assert_eq!(up(PENDING_UPLOAD, 0, None), Transfer { kind: Kind::Upload, fraction: None });
        assert_eq!(up(PENDING_DOWNLOAD, 0, None).kind, Kind::Download);
        assert_eq!(up(TRANSFERRING, 0x20, Some((200, 50))).fraction, Some(0.25));
        assert_eq!(up(TRANSFERRING, 0x20, Some((50, 200))).fraction, Some(0.25)); // order doesn't matter
        assert_eq!(up(TRANSFERRING, 0x40_0020, None).kind, Kind::Download);
        assert_eq!(up(TRANSFERRING, 0x20, Some((0, 0))).fraction, None);
        assert!(classify(2, 0, None).is_none()); // in sync
    }

    /// Logs what a cloud folder reports while files transfer; only files that look active are polled.
    /// `FEX_CLOUD_DIR=<dir> FEX_CLOUD_SECS=240 cargo test -p fex-core cloud::tests::live_watch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_watch() {
        use std::collections::HashMap;
        use std::os::windows::fs::MetadataExt;
        use std::time::{Duration, Instant};
        use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;

        fn pk(fmt: u128, pid: u32) -> PROPERTYKEY {
            PROPERTYKEY { fmtid: GUID::from_u128(fmt), pid }
        }
        fn walk(dir: &Path, depth: u32, out: &mut Vec<(std::path::PathBuf, u32, bool)>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let Ok(m) = e.metadata() else { continue };
                let (p, a) = (e.path(), m.file_attributes());
                if m.is_dir() {
                    if depth > 0 && !e.file_name().to_string_lossy().starts_with('.') {
                        walk(&p, depth - 1, out);
                    }
                } else {
                    out.push((p, a, m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_secs() < 900)));
                }
            }
        }
        init_thread();
        let dir = std::path::PathBuf::from(std::env::var("FEX_CLOUD_DIR").expect("FEX_CLOUD_DIR"));
        let secs: u64 = std::env::var("FEX_CLOUD_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
        println!("in_sync_root = {}", in_sync_root(&dir));
        let keys = [
            ("state", pk(0xE77E90DF_6271_4F5B_834F_2DD1F245DDA4, 3)),
            ("progress", pk(0xE77E90DF_6271_4F5B_834F_2DD1F245DDA4, 4)),
            ("syncTransfer", pk(0xFCEFF153_E839_4CF3_A9E7_EA22832094B8, 103)),
            ("providerStatus", pk(0xFCEFF153_E839_4CF3_A9E7_EA22832094B8, 110)),
            ("offlineAvail", pk(0xFCEFF153_E839_4CF3_A9E7_EA22832094B8, 100)),
        ];
        let start = Instant::now();
        let mut last: HashMap<std::path::PathBuf, String> = HashMap::new();
        while start.elapsed() < Duration::from_secs(secs) {
            let mut cands = vec![];
            walk(&dir, 3, &mut cands);
            for (p, a, recent) in cands {
                if !(recent || a & 0x400 == 0) {
                    continue;
                }
                let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
                let mut line = format!("attrs=0x{a:X}");
                unsafe {
                    match SHGetPropertyStoreFromParsingName::<_, _, IPropertyStore>(PCWSTR(wide.as_ptr()), None, GPS_DEFAULT) {
                        Err(e) => line += &format!(" bind-err {e}"),
                        Ok(store) => {
                            for (n, k) in &keys {
                                let v = store.GetValue(k);
                                let txt = match v {
                                    Err(e) => format!("err {e}"),
                                    Ok(v) => match PropVariantToStringAlloc(&v) {
                                        Ok(w) => format!("{:?}/{}", v.vt(), w.to_string().unwrap_or_default()),
                                        Err(_) => format!("{:?}/-", v.vt()),
                                    },
                                };
                                line += &format!(" {n}=[{txt}]");
                            }
                        }
                    }
                }
                if last.insert(p.clone(), line.clone()).as_ref() != Some(&line) {
                    println!("[{:>4}s] {} {line}", start.elapsed().as_secs(), p.display());
                }
            }
            std::thread::sleep(Duration::from_millis(700));
        }
    }
}
