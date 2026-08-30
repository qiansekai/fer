//! End-to-end tests on a synthetic temp tree (no admin required).

use file_engine_rust::store::Store;

#[test]
fn walk_index_then_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub").join("deep")).unwrap();
    std::fs::write(dir.path().join("hello_world.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("sub").join("deep").join("年度报告.md"), "# 报告").unwrap();
    std::fs::write(dir.path().join("sub").join("Report2026.txt"), "x").unwrap();

    // Keep the index DB OUTSIDE the scanned tree (it would otherwise be indexed).
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("idx.db");
    let mut store = Store::open(&db).unwrap();
    let mut rb = store.begin_rebuild().unwrap();
    let mut files = 0;
    let mut dirs = 0;
    let skipped = file_engine_rust::walk::scan_tree(dir.path().to_str().unwrap(), |path: &str, is_dir: bool, _size: u64| {
        if is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
        rb.insert(path, is_dir, 0, None).unwrap();
    });
    rb.commit().unwrap();

    assert_eq!(skipped, 0);
    assert_eq!(files, 3);
    assert!(dirs >= 2);

    // >= 3 chars → FTS5 trigram
    let r = store.search("report", false, None).unwrap();
    assert_eq!(r.total, 1);
    assert!(r.hits[0].path.ends_with("Report2026.txt"));

    // 2 chars CJK → instr fallback
    let r = store.search("报告", false, None).unwrap();
    assert_eq!(r.total, 1);
    assert!(r.hits[0].path.ends_with("年度报告.md"));

    // wildcard → LIKE
    let r = store.search("*.rs", false, None).unwrap();
    assert_eq!(r.total, 1);
    assert!(r.hits[0].path.ends_with("hello_world.rs"));

    // path mode: hits the directory itself AND the file inside it
    let r = store.search("sub\\deep", false, None).unwrap();
    assert_eq!(r.total, 2);
    assert!(r.hits.iter().any(|h| h.path.ends_with("年度报告.md")));

    // delete + upsert keep FTS consistent
    store.upsert("D:\\tmp\\zzz.txt", false, Some(1)).unwrap();
    assert_eq!(store.search("zzz", false, None).unwrap().total, 1);
    store.delete_by_frn(1).unwrap();
    assert_eq!(store.search("zzz", false, None).unwrap().total, 0);
}
