//! Real-volume integration tests (require an elevated shell).
//! Run explicitly:
//!   cargo test --test live_volume -- --ignored --nocapture
//! Set FER_TEST_DRIVE to pick another drive letter (default C).

use std::time::Instant;

use file_engine_rust::indexer::{self, Method};
use file_engine_rust::mem::MemIndex;
use file_engine_rust::query::Query;
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
    let vols = indexer::resolve_volumes(&d.to_string());
    assert_eq!(vols.len(), 1);

    let t = Instant::now();
    let (report, mem, _max_usns) = indexer::build(&vols, Method::Mft).unwrap();
    let build_ms = t.elapsed().as_millis();

    // dump roundtrip: save → zero-copy load → query
    let dump = dir.path().join("live.db");
    mem.save(&dump).unwrap();
    let loaded = MemIndex::load_dump(&dump).unwrap();
    let t2 = Instant::now();
    let q = Query::parse("ntdll.dll").unwrap();
    let r = loaded.hits(&loaded.search(&q), 100);
    let search_ms = t2.elapsed().as_millis();
    let q2 = Query::parse("hosts").unwrap();
    let r2 = loaded.hits(&loaded.search(&q2), 100);

    eprintln!(
        "[{d}:] build: {build_ms} ms -> {} files + {} dirs (skipped {})",
        report.files, report.dirs, report.skipped
    );
    eprintln!("[{d}:] search 'ntdll.dll': {} hits in {search_ms} ms", r.len());

    assert!(report.files > 100_000, "unexpectedly few files indexed");
    assert!(r.len() >= 6, "expected several ntdll.dll hits, got {}", r.len());
    assert!(
        r2.iter()
            .any(|h| h.path.to_ascii_lowercase() == "c:\\windows\\system32\\drivers\\etc\\hosts"),
        "hosts not found by search"
    );
    assert!(
        r.iter()
            .any(|h| h.path.to_ascii_lowercase() == "c:\\windows\\system32\\ntdll.dll"),
        "hard-link alias System32\\ntdll.dll not resolved by raw MFT scan"
    );
    // metadata sanity: ntdll.dll hits carry real sizes
    let with_size = r.iter().filter(|h| h.size > 0).count();
    assert!(
        with_size >= r.len().saturating_sub(2),
        "expected real sizes from the MFT scan: {with_size}/{}",
        r.len()
    );
    assert!(search_ms < 1000, "search took {search_ms} ms — not instant enough");
}
