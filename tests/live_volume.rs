//! Real-volume integration tests (require an elevated shell).
//! Run explicitly:
//!   cargo test --test live_volume -- --ignored --nocapture
//! Set FER_TEST_DRIVE to pick another drive letter (default C).

use std::time::Instant;

use file_engine_rust::indexer::{self, Method};
use file_engine_rust::mft::MftScanner;
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
    let (report, mem) = indexer::build(&vols, Method::Mft).unwrap();
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
            .any(|h| h.path.eq_ignore_ascii_case("c:\\windows\\system32\\drivers\\etc\\hosts")),
        "hosts not found by search"
    );
    assert!(
        r.iter()
            .any(|h| h.path.eq_ignore_ascii_case("c:\\windows\\system32\\ntdll.dll")),
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

/// Raw `$MFT` metadata quality.
///
/// NTFS keeps `$STANDARD_INFORMATION` in the *base* record while
/// `$ATTRIBUTE_LIST` moves `$FILE_NAME`/`$DATA` attributes of a full record
/// into extension records (every WinSxS `.cat` catalog does this — dozens of
/// hard links). Parsing each record on its own therefore used to report
/// `mtime = 0` for ~9.8k entries on C: and a stale `$FILE_NAME` size of 0 for
/// ~280 more, all silently wrong in `dm:`/`size:` results.
///
/// Run explicitly (needs an elevated shell):
///   cargo test --test live_volume -- --ignored --nocapture live_mft_metadata_quality
#[test]
#[ignore]
fn live_mft_metadata_quality() {
    let d = drive();
    let scanner = match MftScanner::open(d) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SKIP (not admin?): {e:#}");
            return;
        }
    };
    let mut files = 0u64;
    let mut dirs = 0u64;
    // Any entry (file or directory) that lost its $STANDARD_INFORMATION.
    let mut mtime0 = 0u64;
    // Files only: directories legitimately carry no $DATA at all.
    let mut size0 = 0u64;
    scanner
        .scan(|e| {
            if e.is_dir {
                dirs += 1;
            } else {
                files += 1;
                if e.size == 0 {
                    size0 += 1;
                }
            }
            if e.mtime == 0 {
                mtime0 += 1;
            }
        })
        .unwrap();
    eprintln!("[{d}:] {files} files / {dirs} dirs; mtime=0: {mtime0}; size=0 files: {size0}");
    let total = files + dirs;
    assert!(total > 0, "nothing scanned on {d}:");
    // A file may legitimately have FILETIME 0 in $STANDARD_INFORMATION (cygwin's
    // rebase cache is such a case: the disk itself reports 1970-01-01), so the
    // bound is not zero. It is tight enough to catch the regression: before the
    // fix 9,851/1,058,353 entries on C: (0.93%) and 45,149/3,022,746 on D:
    // (1.49%) reported mtime 0 because their $FILE_NAME had been moved into an
    // extension record and the base record's timestamps were never read.
    assert!(
        mtime0 * 400 < total,
        "{mtime0}/{total} entries lost their $STANDARD_INFORMATION timestamps"
    );
    // Zero-length files are real (1.9% of C:\'s files), so this is a sanity
    // bound only; the sharp checks for the spilled-$DATA size are the synthetic
    // unit tests and a disk comparison of the index (see AGENTS.md).
    assert!(
        size0 * 10 < files,
        "implausibly many zero-size files: {size0}/{files}"
    );
}
