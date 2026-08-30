//! Real-volume integration tests (require an elevated shell).
//! Run explicitly:
//!   cargo test --test live_volume -- --ignored --nocapture
//! Set FER_TEST_DRIVE to pick another drive letter (default C).

use std::time::Instant;

use file_engine_rust::indexer::{self, Method};
use file_engine_rust::store::Store;
use file_engine_rust::usn::UsnVolume;

fn drive() -> char {
    std::env::var("FER_TEST_DRIVE")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
}

#[test]
#[ignore]
fn usn_enumeration_live() {
    let d = drive();
    let vol = match UsnVolume::open(d) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("SKIP (not admin?): {e:#}");
            return;
        }
    };
    let t = Instant::now();
    let mut records = 0u64;
    let mut max_usn = 0i64;
    let mut found = false;
    vol.enumerate(|r| {
        records += 1;
        max_usn = max_usn.max(r.usn);
        if r.name.eq_ignore_ascii_case("ntdll.dll") {
            found = true;
        }
    })
    .unwrap();
    eprintln!(
        "[{d}:] enumerated {records} MFT records in {} ms, max_usn={max_usn}",
        t.elapsed().as_millis()
    );
    assert!(records > 100_000, "unexpectedly few MFT records: {records}");
    assert!(found, "ntdll.dll not found on {d}:");
}

#[test]
#[ignore]
fn live_build_and_instant_search() {
    let d = drive();
    if UsnVolume::open(d).is_err() {
        eprintln!("SKIP (not admin?)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join("live.db")).unwrap();
    let vols = indexer::resolve_volumes(&d.to_string());
    assert_eq!(vols.len(), 1);

    let t = Instant::now();
    let report = indexer::build(&mut store, &vols, Method::Usn).unwrap();
    let build_ms = t.elapsed().as_millis();

    let t2 = Instant::now();
    let r = store.search("ntdll.dll", false, None).unwrap();
    let search_ms = t2.elapsed().as_millis();
    // `hosts` is a real (non-hardlink) file — hardlink aliases such as
    // System32\ntdll.dll point to WinSxS and only expose their primary
    // name via FSCTL_ENUM_USN_DATA (see README, known limitations).
    let r2 = store.search("hosts", false, None).unwrap();

    eprintln!(
        "[{d}:] build: {build_ms} ms -> {} files + {} dirs (skipped {})",
        report.files, report.dirs, report.skipped
    );
    eprintln!("[{d}:] search 'ntdll.dll': {} hits in {search_ms} ms", r.hits.len());

    assert!(report.files > 100_000, "unexpectedly few files indexed");
    assert!(r.hits.len() >= 6, "expected several ntdll.dll hits, got {}", r.hits.len());
    assert!(
        r2.hits
            .iter()
            .any(|h| h.path.to_ascii_lowercase() == "c:\\windows\\system32\\drivers\\etc\\hosts"),
        "hosts not found by search"
    );
    assert!(search_ms < 1000, "search took {search_ms} ms — not instant enough");
}
