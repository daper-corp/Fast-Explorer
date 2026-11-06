//! fex-index: Fast Explorer's index service.
//!
//!   fex-index install     (admin) copy to Program Files, register and start the service
//!   fex-index uninstall   (admin) stop and remove it
//!   fex-index run         (admin) serve from this console, for debugging
//!
//! Indexes every fixed NTFS volume from its MFT, follows the USN journal, and answers name
//! searches on a named pipe that only the installing user (plus SYSTEM/Administrators) can open.

use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use fex_core::index::{self, Hit, PIPE_NAME, SERVICE_NAME, Volume};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{RevertToSelf, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers, OPEN_EXISTING, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, ImpersonateNamedPipeClient, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, Owned, PWSTR};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

type Res<T> = Result<T, Box<dyn Error>>;

/// Most hits sent per query.
const MAX_LIMIT: usize = 20_000;

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        Some("run") => {
            let sid = current_user_sid()?;
            println!("indexing {:?}; serving {PIPE_NAME} for {sid}", fixed_ntfs_volumes());
            start(sid)?;
            thread::park();
            Ok(())
        }
        Some("service") => Ok(service_dispatcher::start(SERVICE_NAME, ffi_service_main)?),
        _ => {
            eprintln!("usage: fex-index install | uninstall | run");
            std::process::exit(2);
        }
    }
}

// ---------- the Windows service ----------

define_windows_service!(ffi_service_main, service_main);

fn service_main(_: Vec<OsString>) {
    if let Err(e) = run_service() {
        eprintln!("{e}");
    }
}

fn run_service() -> Res<()> {
    // Installed as: fex-index.exe service --user <SID of the installing user>
    let args: Vec<String> = std::env::args().collect();
    let sid = match args.iter().position(|a| a == "--user").and_then(|i| args.get(i + 1)) {
        Some(sid) if is_sid(sid) => sid.clone(),
        _ => return Err("missing or invalid --user SID".into()),
    };
    let (stop_tx, stop_rx) = mpsc::channel();
    let handle = service_control_handler::register(SERVICE_NAME, move |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let status = |state, accept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    };
    handle.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN))?;
    start(sid)?;
    let _ = stop_rx.recv();
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(()) // returning ends the process, index threads included
}

/// "S-1-5-21-..." and nothing else, since it goes into the pipe's SDDL.
fn is_sid(s: &str) -> bool {
    s.starts_with("S-1-") && s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'S')
}

// ---------- indexing ----------

struct Slot {
    index: RwLock<Volume>,
    ready: AtomicBool,
}

fn start(user_sid: String) -> Res<()> {
    let slots: Arc<Vec<Slot>> = Arc::new(
        fixed_ntfs_volumes()
            .into_iter()
            .map(|l| Slot { index: RwLock::new(Volume::new(l)), ready: AtomicBool::new(false) })
            .collect(),
    );
    for i in 0..slots.len() {
        let slots = slots.clone();
        thread::spawn(move || keep_indexed(&slots[i]));
    }
    let pipe_security = pipe_security(&user_sid)?;
    thread::spawn(move || serve(slots, pipe_security));
    Ok(())
}

/// Builds the volume's index, then follows its journal; rebuilds whenever the journal can't be
/// followed (wrapped, recreated, volume dismounted).
fn keep_indexed(slot: &Slot) {
    let letter = slot.index.read().unwrap().letter();
    loop {
        slot.ready.store(false, Ordering::Relaxed);
        let built = index::open_volume(letter).and_then(|vol| Ok((index::build(*vol, letter)?, vol)));
        let ((fresh, mut pos), vol) = match built {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{letter}: {e}");
                thread::sleep(Duration::from_secs(60));
                continue;
            }
        };
        eprintln!("{letter}: {} names", fresh.len());
        *slot.index.write().unwrap() = fresh;
        slot.ready.store(true, Ordering::Relaxed);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut changes = Vec::new();
            if index::read_changes(*vol, &mut pos, &mut buf, |c| changes.push(c)).is_err() {
                break;
            }
            let mut v = slot.index.write().unwrap();
            for c in &changes {
                v.apply(c);
            }
        }
    }
}

fn fixed_ntfs_volumes() -> Vec<char> {
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

// ---------- the pipe ----------

/// Security attributes for the pipe: SYSTEM and Administrators full access; the installing user
/// may read and write data (0x100083: FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES |
/// SYNCHRONIZE) but not create pipe instances of their own; nobody else. Leaked on purpose: it lives as long as the process.
struct PipeSecurity(SECURITY_ATTRIBUTES);
unsafe impl Send for PipeSecurity {}

fn pipe_security(user_sid: &str) -> Res<PipeSecurity> {
    let sddl = HSTRING::from(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x100083;;;{user_sid})"));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None)? };
    Ok(PipeSecurity(SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    }))
}

fn serve(slots: Arc<Vec<Slot>>, security: PipeSecurity) {
    let mut first = true;
    loop {
        // The first instance claims the name, so no other process can have created it before us.
        let mut mode = PIPE_ACCESS_DUPLEX;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let pipe = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(PIPE_NAME),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                4 * 1024,
                0,
                Some(&security.0),
            )
        };
        if pipe.is_invalid() {
            eprintln!("pipe: {:?}", unsafe { GetLastError() });
            thread::sleep(Duration::from_secs(5));
            continue;
        }
        first = false;
        let pipe = unsafe { File::from_raw_handle(pipe.0) };
        let connected = unsafe { ConnectNamedPipe(HANDLE(pipe.as_raw_handle()), None) }.is_ok()
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if connected {
            let slots = slots.clone();
            thread::spawn(move || answer(pipe, &slots));
        }
    }
}

/// One request per connection: `S <limit> <query>`.
fn answer(pipe: File, slots: &[Slot]) {
    let mut line = String::new();
    if BufReader::new(&pipe).take(4096).read_line(&mut line).is_err() {
        return;
    }
    let mut f = line.trim_end_matches(['\r', '\n']).splitn(3, '\t');
    let mut out = BufWriter::new(&pipe);
    let _ = match (f.next(), f.next().and_then(|n| n.parse::<usize>().ok()), f.next()) {
        (Some("S"), Some(limit), Some(query)) => {
            let want = limit.clamp(1, MAX_LIMIT);
            // Fetch extra: some hits may be hidden from this client below.
            let fetch = want * 4;
            let guards: Vec<_> = slots.iter().map(|s| s.index.read().unwrap()).collect();
            let (matched, hits) = index::search(guards.iter().map(|g| &**g), query, fetch);
            drop(guards);
            let checked_all = matched <= fetch;
            let visible = visible_to_client(HANDLE(pipe.as_raw_handle()), hits, if checked_all { usize::MAX } else { want + 1 });
            // Only ever report what this client may see: the unfiltered `matched` count would tell
            // it whether hidden files with a given name exist. `more` likewise comes from visible
            // hits alone (so it can miss visible matches beyond the fetched ones, never leak).
            let more = visible.len() > want;
            let hits = &visible[..visible.len().min(want)];
            let indexing = slots.iter().filter(|s| !s.ready.load(Ordering::Relaxed)).count();
            index::encode_results(&mut out, hits, indexing, more)
        }
        _ => writeln!(out, "ERR\t알 수 없는 요청"),
    };
    let _ = out.flush();
    unsafe {
        let _ = FlushFileBuffers(HANDLE(pipe.as_raw_handle())); // let the client read it all
    }
}

/// The service sees every name on the volume; the client gets only hits it could have found by
/// browsing: every folder from the drive root down to the hit must be listable by the client
/// (checked as the client, so other users' private folders, and the names of folders inside
/// them, stay private). Fails closed: no impersonation, no hits. Stops after `keep` visible hits.
fn visible_to_client(pipe: HANDLE, hits: Vec<Hit>, keep: usize) -> Vec<Hit> {
    if unsafe { ImpersonateNamedPipeClient(pipe) }.is_err() {
        return Vec::new();
    }
    let mut listable: HashMap<PathBuf, bool> = HashMap::new();
    let kept = hits
        .into_iter()
        .filter(|h| h.path.parent().is_some_and(|dir| browsable(dir, &mut listable)))
        .take(keep)
        .collect();
    // Never keep serving with the client's identity.
    unsafe { RevertToSelf() }.expect("RevertToSelf");
    kept
}

/// `dir` and all its ancestors are listable; cached per folder (siblings share their ancestors).
fn browsable(dir: &Path, cache: &mut HashMap<PathBuf, bool>) -> bool {
    if let Some(&known) = cache.get(dir) {
        return known;
    }
    let ok = dir.parent().is_none_or(|parent| browsable(parent, cache)) && can_list(dir);
    cache.insert(dir.to_path_buf(), ok);
    ok
}

fn can_list(dir: &Path) -> bool {
    unsafe {
        CreateFileW(
            &HSTRING::from(dir.as_os_str()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .map(|h| drop(Owned::new(h)))
        .is_ok()
    }
}

// ---------- install ----------

fn install_dir() -> Res<PathBuf> {
    Ok(fex_core::shell::install_dir()?)
}

/// Copies this exe to Program Files (a SYSTEM service must not run from a user-writable
/// folder) and registers it for the calling user.
fn install() -> Res<()> {
    let sid = current_user_sid()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    remove_service(&manager)?; // reinstalling replaces the old one

    let dir = install_dir()?;
    std::fs::create_dir_all(&dir)?;
    let exe = dir.join("fex-index.exe");
    std::fs::copy(std::env::current_exe()?, &exe)?;

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Fast Explorer Index".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec!["service".into(), "--user".into(), sid.into()],
        dependencies: vec![],
        account_name: None, // LocalSystem: needed to read volumes
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::START | ServiceAccess::CHANGE_CONFIG)?;
    service.set_description("Fast Explorer의 즉시 파일 이름 검색 (NTFS MFT / USN 저널 색인)")?;
    service.start::<&str>(&[])?;
    println!("installed and started {SERVICE_NAME}");
    Ok(())
}

fn uninstall() -> Res<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    remove_service(&manager)?;
    let dir = install_dir()?;
    let _ = std::fs::remove_file(dir.join("fex-index.exe"));
    let _ = std::fs::remove_dir(dir);
    println!("removed {SERVICE_NAME}");
    Ok(())
}

fn remove_service(manager: &ServiceManager) -> Res<()> {
    let Ok(service) = manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)
    else {
        return Ok(());
    };
    let _ = service.stop();
    for _ in 0..50 {
        if service.query_status()?.current_state == ServiceState::Stopped {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    service.delete()?;
    drop(service);
    thread::sleep(Duration::from_millis(500)); // let the SCM release the old process and its exe
    Ok(())
}

/// The SID of the user running this process (the same user when elevated through UAC).
fn current_user_sid() -> Res<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = Owned::new(token);
        let mut len = 0;
        let _ = GetTokenInformation(*token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(*token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len)?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s)?;
        let sid = s.to_string()?;
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        Ok(sid)
    }
}
