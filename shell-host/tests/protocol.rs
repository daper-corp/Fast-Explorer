//! Drives the real host process through the client in fex_core::menu.

use std::time::{Duration, Instant};

use fex_core::menu::{MenuHost, Request};

#[test]
fn serves_menus_and_recovers_after_being_killed() {
    let dir = std::env::temp_dir().join(format!("fex-host-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let host = MenuHost::new(env!("CARGO_BIN_EXE_fex-shell-host").into(), move |seq, r| {
        let _ = done_tx.send((seq, r));
    });
    let wait = Duration::from_secs(15);

    let t = Instant::now();
    host.send(&Request::Items { seq: 1, paths: vec![dir.join("a.txt")] }).unwrap();
    let items = host.wait(1, wait).expect("answered").expect("menu built");
    eprintln!("item menu (cold host): {} items in {:?}", items.len(), t.elapsed());
    assert!(items.iter().any(|i| i.verb == "properties"), "{items:#?}");

    let t = Instant::now();
    host.send(&Request::Items { seq: 2, paths: vec![dir.join("a.txt")] }).unwrap();
    let items = host.wait(2, wait).expect("answered").expect("menu built");
    eprintln!("item menu (warm host): {:?}", t.elapsed());

    // Running a command from the menu: "바로 가기 만들기" (verb "link") makes a .lnk next to a.txt.
    let link = items.iter().find(|i| i.verb == "link").expect("link item");
    host.send(&Request::Invoke { seq: 2, id: link.id }).unwrap();
    assert_eq!(done_rx.recv_timeout(wait).unwrap(), (2, Ok(())));
    let lnk = std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.path().extension().is_some_and(|x| x == "lnk"));
    assert!(lnk, "shortcut created");

    host.send(&Request::Background { seq: 3, folder: dir.clone() }).unwrap();
    let bg = host.wait(3, wait).expect("answered").expect("menu built");
    assert!(bg.iter().any(|i| i.submenu), "background menu has a submenu (새로 만들기): {bg:#?}");

    // A crashed host is replaced on the next request.
    let pid = host.pid().unwrap();
    std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    host.send(&Request::Items { seq: 4, paths: vec![dir.join("a.txt")] }).unwrap();
    assert!(host.wait(4, wait).expect("answered").is_ok());
    assert_ne!(host.pid(), Some(pid));

    drop(host);
    std::fs::remove_dir_all(&dir).unwrap();
}
