//! Watches the folder a tab is showing, so files created or changed by other programs
//! appear without pressing F5.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::{Owned, PCWSTR};

const BUF_BYTES: usize = 64 * 1024;
const INFINITE: u32 = u32::MAX;

/// What a listing cares about: names appearing/disappearing, and files being written to.
const FILTER: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE(
    FILE_NOTIFY_CHANGE_FILE_NAME.0
        | FILE_NOTIFY_CHANGE_DIR_NAME.0
        | FILE_NOTIFY_CHANGE_LAST_WRITE.0
        | FILE_NOTIFY_CHANGE_SIZE.0
        | FILE_NOTIFY_CHANGE_ATTRIBUTES.0
        | FILE_NOTIFY_CHANGE_CREATION.0,
);

/// Why a pending read ended.
enum ReadError {
    /// A new path was requested (or we are shutting down): drop the buffer and re-arm.
    Interrupted,
    /// The kernel buffer overflowed or the folder went away: the caller should refresh.
    Notify,
    /// The watch itself failed; retry later without bothering the UI.
    Broken,
}

struct Watch {
    dir: Owned<HANDLE>,
    event: Owned<HANDLE>,
    buf: Vec<u64>,
    overlapped: Box<OVERLAPPED>,
    path: PathBuf,
}

impl Watch {
    fn open(path: &PathBuf) -> io::Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let dir = unsafe {
            Owned::new(CreateFileW(
                PCWSTR::from_raw(wide.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )?)
        };
        let event = unsafe { Owned::new(CreateEventW(None, true, false, PCWSTR::null())?) };
        let mut overlapped = Box::<OVERLAPPED>::default();
        overlapped.hEvent = *event;
        Ok(Watch {
            dir,
            event,
            buf: vec![0u64; BUF_BYTES / 8], // 8-byte aligned, as the records require
            overlapped,
            path: path.clone(),
        })
    }

    /// Waits for one change. `ctrl` is signalled when a different path is wanted.
    fn read(&mut self, ctrl: HANDLE) -> Result<(), ReadError> {
        unsafe {
            let _ = ResetEvent(*self.event);
            ReadDirectoryChangesW(
                *self.dir,
                self.buf.as_mut_ptr().cast(),
                BUF_BYTES as u32,
                false,
                FILTER,
                None,
                Some(&mut *self.overlapped),
                None,
            )
            .map_err(|_| ReadError::Broken)?;

            let wait = WaitForMultipleObjects(&[ctrl, *self.event], false, INFINITE);
            if wait.0 == WAIT_OBJECT_0.0 {
                // Path change or shutdown: cancel and wait for the kernel to release the buffer.
                let _ = CancelIoEx(*self.dir, Some(&*self.overlapped));
                let mut got = 0u32;
                let _ = GetOverlappedResult(*self.dir, &*self.overlapped, &mut got, true);
                return Err(ReadError::Interrupted);
            }
            if wait.0 == WAIT_OBJECT_0.0 + 1 {
                let mut got = 0u32;
                return match GetOverlappedResult(*self.dir, &*self.overlapped, &mut got, false) {
                    Ok(()) => Ok(()),
                    Err(_) => Err(ReadError::Notify), // overflow (ERROR_NOTIFY_ENUM_DIR) or the folder is gone
                };
            }
            Err(ReadError::Broken)
        }
    }
}

/// One watcher per tab, on its own thread. Queues the path to watch and reports changes,
/// which the UI debounces before re-listing.
pub struct DirWatch {
    ctrl: Owned<HANDLE>,
    desired: Arc<Mutex<Option<PathBuf>>>,
    thread: Option<JoinHandle<()>>,
}

impl DirWatch {
    pub fn new(on_change: impl Fn() + Send + 'static) -> io::Result<Self> {
        let ctrl = unsafe { Owned::new(CreateEventW(None, false, false, PCWSTR::null())?) };
        let raw = ctrl.0 as isize;
        let desired = Arc::new(Mutex::new(None));
        let worker = desired.clone();
        let thread = thread::Builder::new()
            .name("fex-watch".into())
            .spawn(move || run(raw, worker, on_change))?;
        Ok(DirWatch { ctrl, desired, thread: Some(thread) })
    }

    /// Watches `path` from now on. Cheap no-op while it is already the watched folder.
    pub fn watch(&self, path: PathBuf) {
        {
            let mut desired = self.desired.lock().unwrap();
            if desired.as_ref() == Some(&path) {
                return;
            }
            *desired = Some(path);
        }
        unsafe {
            let _ = SetEvent(*self.ctrl);
        }
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        *self.desired.lock().unwrap() = None;
        unsafe {
            let _ = SetEvent(*self.ctrl);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(ctrl: isize, desired: Arc<Mutex<Option<PathBuf>>>, on_change: impl Fn()) {
    let ctrl = HANDLE(ctrl as *mut c_void);
    let mut watch: Option<Watch> = None;
    loop {
        let wanted = desired.lock().unwrap().clone();
        let Some(path) = wanted else { return };

        if watch.as_ref().is_none_or(|w| w.path != path) {
            watch = Watch::open(&path).ok();
        }
        let Some(w) = watch.as_mut() else {
            // Not watchable (yet): retry in a moment, or as soon as a new path is requested.
            idle(ctrl, 1000);
            continue;
        };

        match w.read(ctrl) {
            Ok(()) => on_change(),
            Err(ReadError::Interrupted) => {}
            Err(ReadError::Notify) => {
                on_change();
                watch = None;
                idle(ctrl, 500);
            }
            Err(ReadError::Broken) => {
                watch = None;
                idle(ctrl, 1000);
            }
        }
    }
}

/// Waits up to `ms` for a new path request; returns at once when there is one.
fn idle(ctrl: HANDLE, ms: u32) {
    unsafe {
        let _ = WaitForSingleObject(ctrl, ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn notices_files_created_by_another_writer() {
        let dir = std::env::temp_dir().join(format!("fex-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = DirWatch::new(move || {
            let _ = tx.send(());
        })
        .unwrap();
        watcher.watch(dir.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut i = 0;
        let mut seen = false;
        while Instant::now() < deadline {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
            i += 1;
            if rx.recv_timeout(Duration::from_millis(300)).is_ok() {
                seen = true;
                break;
            }
        }
        drop(watcher);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(seen, "no change notification");
    }
}
