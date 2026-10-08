//! End-to-end test for the directory-walk fallback and in-memory search.

use file_engine_rust::mem::MemBuilder;
use file_engine_rust::query::Query;

#[test]
fn walk_index_then_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub").join("deep")).unwrap();
    std::fs::write(dir.path().join("hello_world.rs"), "fn main() {}").unwrap();
    std::fs::write(
        dir.path().join("sub").join("deep").join("年度报告.md"),
        "# 报告",
    )
    .unwrap();
    std::fs::write(dir.path().join("sub").join("Report2026.txt"), "x").unwrap();

    let mut builder = MemBuilder::default();
    let mut files = 0;
    let mut dirs = 0;
    let skipped = file_engine_rust::walk::scan_tree(
        dir.path().to_str().unwrap(),
        |path: &str, meta: file_engine_rust::EntryMeta| {
            if meta.is_dir {
                dirs += 1;
            } else {
                files += 1;
            }
            builder.push(path, meta);
        },
    );
    let mem = builder.finish();

    assert_eq!(skipped, 0);
    assert_eq!(files, 3);
    assert!(dirs >= 2);

    let report = mem.search(&Query::parse("report").unwrap());
    let report_paths: Vec<_> = mem
        .hits(&report, 100)
        .into_iter()
        .map(|h| h.path)
        .collect();
    assert_eq!(report_paths.len(), 1);
    assert!(report_paths[0].ends_with("Report2026.txt"));

    let chinese = mem.search(&Query::parse("报告").unwrap());
    assert_eq!(chinese.len(), 1);
    assert!(mem.hits(&chinese, 1)[0].path.ends_with("年度报告.md"));

    let rust = mem.search(&Query::parse("*.rs").unwrap());
    assert_eq!(rust.len(), 1);
    assert!(mem.hits(&rust, 1)[0].path.ends_with("hello_world.rs"));

    let nested = mem.search(&Query::parse("sub\\deep").unwrap());
    assert!(mem
        .hits(&nested, 100)
        .iter()
        .any(|h| h.path.ends_with("年度报告.md")));
}
