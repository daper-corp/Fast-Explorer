//! Against the installed service (run with --ignored after `fex-index install`).

use std::time::{Duration, Instant};

use fex_core::index::query;

#[test]
#[ignore]
fn live_service() {
    let t = Instant::now();
    loop {
        match query("system32", 10) {
            Ok(r) if r.indexing == 0 => break,
            Ok(_) => {}
            Err(e) => eprintln!("waiting: {e:?}"),
        }
        assert!(t.elapsed() < Duration::from_secs(300), "index not ready in 5 min");
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("index ready after {:?} (since this test started)", t.elapsed());
    for q in ["system32", "notepad.exe", "보고서", "a", "fex index", "zzzz-no-such-name-zzzz"] {
        let t = Instant::now();
        let r = query(q, 2000).unwrap();
        eprintln!("{q:>24}: {:>8} matches, {:>4} returned, {:?}", r.total, r.hits.len(), t.elapsed());
    }
    let r = query("notepad.exe", 5).unwrap();
    for h in &r.hits {
        eprintln!("    {}", h.path.display());
    }
    // A new file shows up through the USN journal.
    let dir = std::env::temp_dir();
    let name = format!("fex-live-{}.tmp", std::process::id());
    std::fs::write(dir.join(&name), "x").unwrap();
    let t = Instant::now();
    while query(&name, 5).unwrap().hits.is_empty() {
        assert!(t.elapsed() < Duration::from_secs(10), "new file not indexed");
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("new file searchable after {:?}", t.elapsed());
    std::fs::remove_file(dir.join(&name)).unwrap();
    let t = Instant::now();
    while !query(&name, 5).unwrap().hits.is_empty() {
        assert!(t.elapsed() < Duration::from_secs(10), "deleted file still indexed");
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("deleted file gone after {:?}", t.elapsed());
}
