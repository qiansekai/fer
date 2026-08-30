//! Compact in-memory name index for serve mode.
//!
//! 1-2 char substring queries cannot use the FTS5 trigram index and fall back
//! to an `instr` scan (~0.4 s). This module keeps every lowercased name in one
//! arena with flat parallel arrays (~25 bytes/entry, ≈120 MB for 4M files) and
//! answers short substrings with a single SIMD pass — the Everything-style
//! trade-off of ~100 MB resident for ~100 ms latency.
//!
//! Only the serve process loads it (`fer serve`); one-shot CLI queries keep
//! using SQL so a single search never pays the load cost.

use anyhow::Result;
use rusqlite::Connection;

pub struct MemIndex {
    ids: Vec<i64>,
    off: Vec<u32>,
    len: Vec<u16>,
    arena: Vec<u8>,
}

impl MemIndex {
    pub fn load(conn: &Connection) -> Result<Self> {
        // ORDER BY id: without it the planner may answer via the covering
        // name_l index (name order); we want rowid order so scans stay sorted.
        let mut stmt = conn.prepare("SELECT id, name_l FROM files ORDER BY id")?;
        let mut rows = stmt.query([])?;
        let mut ids = Vec::new();
        let mut off = Vec::new();
        let mut len = Vec::new();
        let mut arena: Vec<u8> = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            ids.push(id);
            off.push(arena.len() as u32);
            len.push(name.len() as u16);
            arena.extend_from_slice(name.as_bytes());
        }
        Ok(MemIndex { ids, off, len, arena })
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn memory_bytes(&self) -> usize {
        self.ids.len() * 8 + self.off.len() * 4 + self.len.len() * 2 + self.arena.len()
    }

    fn name(&self, i: usize) -> &[u8] {
        let o = self.off[i] as usize;
        &self.arena[o..o + self.len[i] as usize]
    }

    /// SIMD substring scan. `needle` must be lowercased. An empty needle
    /// matches everything. Returns matching file ids in ascending order.
    pub fn find_substr(&self, needle: &str) -> Vec<i64> {
        if needle.is_empty() {
            return self.ids.clone();
        }
        let n = needle.as_bytes();
        let mut out = Vec::new();
        for i in 0..self.ids.len() {
            if memchr::memmem::find(self.name(i), n).is_some() {
                out.push(self.ids[i]);
            }
        }
        out
    }

    /// True when the query is exactly one short bare name term (no negation) —
    /// the case this index accelerates.
    pub fn handles_query(q: &crate::query::Query) -> bool {
        if !q.exclude.is_empty() || q.include.len() != 1 {
            return false;
        }
        matches!(&q.include[0], crate::query::Term::Name(s) if s.chars().count() < 3)
    }

    /// The needle for a query that [`MemIndex::handles_query`] accepted.
    pub fn needle<'a>(&self, q: &'a crate::query::Query) -> &'a str {
        match &q.include[0] {
            crate::query::Term::Name(s) => s,
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::EntryMeta;

    fn build() -> (tempfile::TempDir, Store, MemIndex) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("t.db")).unwrap();
        let mut rb = store.begin_rebuild().unwrap();
        for path in [
            r"D:\docs\年度报告.md",
            r"D:\docs\readme.txt",
            r"D:\proj\main.rs",
            r"D:\proj\lib.rs",
            r"D:\media\rs.jpg",
        ] {
            rb.insert(path, EntryMeta::default()).unwrap();
        }
        rb.commit().unwrap();
        let mem = MemIndex::load(&store.conn()).unwrap();
        (dir, store, mem)
    }

    #[test]
    fn mem_two_char_cjk() {
        let (_d, _store, mem) = build();
        let q = crate::query::Query::parse("报告").unwrap();
        assert!(MemIndex::handles_query(&q));
        let ids = mem.find_substr(mem.needle(&q));
        assert_eq!(ids.len(), 1);
        // ids must agree with the SQL path
        let sql_ids = {
            let r = _store.search("报告", false, None).unwrap();
            r.hits.len()
        };
        assert_eq!(ids.len(), sql_ids);
    }

    #[test]
    fn mem_two_char_ascii() {
        let (_d, _store, mem) = build();
        let q = crate::query::Query::parse("rs").unwrap();
        let ids = mem.find_substr(mem.needle(&q));
        assert_eq!(ids.len(), 3); // main.rs, lib.rs, rs.jpg
    }

    #[test]
    fn mem_empty_matches_all() {
        let (_d, _store, mem) = build();
        assert_eq!(mem.find_substr("").len(), 5);
    }

    #[test]
    fn mem_ids_ascending_and_fetch_roundtrip() {
        let (_d, store, mem) = build();
        let q = crate::query::Query::parse("rs").unwrap();
        let ids = mem.find_substr(mem.needle(&q));
        eprintln!("ids = {ids:?}");
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids not ascending: {ids:?}");
        let hits = store.fetch_ids(&ids).unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|h| h.path.to_lowercase().contains("rs")));
    }

    #[test]
    fn handles_query_dispatch() {
        assert!(MemIndex::handles_query(&crate::query::Query::parse("报告").unwrap()));
        assert!(MemIndex::handles_query(&crate::query::Query::parse("rs").unwrap()));
        // empty query parses to zero terms → SQL path (also instant)
        assert!(!MemIndex::handles_query(&crate::query::Query::parse("").unwrap()));
        assert!(!MemIndex::handles_query(&crate::query::Query::parse("foo").unwrap())); // 3 chars
        assert!(!MemIndex::handles_query(&crate::query::Query::parse("*.rs").unwrap()));
        assert!(!MemIndex::handles_query(&crate::query::Query::parse("rs ext:jpg").unwrap()));
        assert!(!MemIndex::handles_query(&crate::query::Query::parse("rs !tmp").unwrap()));
        assert!(!MemIndex::handles_query(&crate::query::Query::parse(r"D:\rs").unwrap()));
    }
}
