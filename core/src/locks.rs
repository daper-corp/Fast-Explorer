//! "Who is using this file?": finds the processes holding files open (Windows Restart Manager)
//! and ends them on request.
//!
//! Restart Manager reports processes that hold *files* open. A folder is searched by its files
//! (up to [`FILE_CAP`]); a process that merely has a folder as its working directory is not seen.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME, HANDLE};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows::core::{PCWSTR, PWSTR};

/// Most files registered for one search; keeps huge folders from stalling the query.
pub const FILE_CAP: usize = 20_000;

#[derive(Clone, Debug)]
pub struct Locker {
    pub pid: u32,
    /// Process start time, to be sure a later kill hits the same process and not a reused PID.
    pub started: u64,
    pub name: String,
    pub exe: Option<PathBuf>,
    /// A Windows service (shown, but ending it usually needs administrator rights).
    pub service: bool,
}

/// The files to ask about: the paths themselves, plus everything below each folder.
/// Returns the list and whether it was cut at [`FILE_CAP`].
pub fn expand(paths: &[PathBuf]) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    for p in paths {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => stack.push(p.clone()),
            _ => files.push(p.clone()),
        }
    }
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if files.len() >= FILE_CAP {
                return (files, true);
            }
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                if !t.is_symlink() {
                    stack.push(e.path());
                }
            } else {
                files.push(e.path());
            }
        }
    }
    (files, false)
}

fn filetime(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// Processes holding any of `files` open, other than this one.
pub fn find(files: &[PathBuf]) -> io::Result<Vec<Locker>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    unsafe {
        let mut session = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        let r = RmStartSession(&mut session, None, PWSTR(key.as_mut_ptr()));
        if r != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(r.0 as i32));
        }
        let result = (|| {
            let wide: Vec<Vec<u16>> =
                files.iter().map(|f| f.as_os_str().encode_wide().chain(std::iter::once(0)).collect()).collect();
            let ptrs: Vec<PCWSTR> = wide.iter().map(|w| PCWSTR(w.as_ptr())).collect();
            let r = RmRegisterResources(session, Some(&ptrs), None, None);
            if r != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(r.0 as i32));
            }
            let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
            let mut needed = 0u32;
            loop {
                let mut count = infos.len() as u32;
                let mut reasons = 0u32;
                let ptr = if infos.is_empty() { None } else { Some(infos.as_mut_ptr()) };
                let r = RmGetList(session, &mut needed, &mut count, ptr, &mut reasons);
                if r == ERROR_SUCCESS {
                    infos.truncate(count as usize);
                    return Ok(infos);
                }
                if r != ERROR_MORE_DATA {
                    return Err(io::Error::from_raw_os_error(r.0 as i32));
                }
                infos = vec![std::mem::zeroed(); needed as usize + 4]; // a few spare: the list can grow between calls
            }
        })();
        let _ = RmEndSession(session);
        let me = GetCurrentProcessId();
        Ok(result?
            .into_iter()
            .filter(|i| i.Process.dwProcessId != me)
            .map(|i| {
                let len = i.strAppName.iter().position(|&c| c == 0).unwrap_or(i.strAppName.len());
                let pid = i.Process.dwProcessId;
                Locker {
                    pid,
                    started: filetime(i.Process.ProcessStartTime),
                    name: String::from_utf16_lossy(&i.strAppName[..len]),
                    exe: exe_of(pid),
                    service: i.ApplicationType.0 == 3 /* RmService */,
                }
            })
            .collect())
    }
}

fn exe_of(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// Ends the process `locker` describes. Refuses if the PID now belongs to another process.
pub fn kill(locker: &Locker) -> io::Result<()> {
    if locker.pid <= 4 || locker.pid == unsafe { GetCurrentProcessId() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "이 프로세스는 종료할 수 없음"));
    }
    unsafe {
        let h: HANDLE = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, locker.pid)?;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let same = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok() && filetime(c) == locker.started;
        let r = if same {
            TerminateProcess(h, 1).map_err(io::Error::from)
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "프로세스가 이미 종료되었거나 바뀜"))
        };
        let _ = CloseHandle(h);
        r
    }
}

/// Convenience for callers that only have paths.
pub fn find_paths(paths: &[PathBuf]) -> io::Result<(Vec<Locker>, bool)> {
    let (files, capped) = expand(paths);
    Ok((find(&files)?, capped))
}
