//! Full in-memory search engine for serve mode — the Everything-style route:
//! queries never touch the database, they run against compact sorted arrays
//! and SIMD scans.
//!
//! Layout (~40 bytes/entry + string arenas, ≈600 MB for 4M files):
//! * one packed `Entry` per file (id, arena offsets, size, timestamps, flags)
//! * `paths` (original case, for display + CI prefix search),
//!   `names` (lowercased, for substring/glob scans),
//!   `revs` (reversed lowercased names, for suffix binary search)
//! * sorted permutations: `by_path` (CI order), `by_rev`, `by_size`,
//!   `by_mtime`, `by_ctime` — all queries reduce to two binary searches
//!   (partition points) over one of these
//!
//! All 12 query-language terms evaluate in memory. SQLite stays as the
//! persistence layer (build target, monitor sink); serve reloads this index
//! after a rescan. Disable with `fer serve --no-mem-index`.
//!
//! Known divergence: path CI ordering folds ASCII case only; non-ASCII
//! letters with Unicode case (É/Ö/ü) compare bytewise — SQLite's fallback
//! lowercases them. Extremely rare in Windows paths.

use std::cmp::Ordering;

use anyhow::Result;
use rusqlite::Connection;

use crate::query::{Query, Term};
use crate::store::Hit;

#[derive(Debug, Clone, Copy)]
struct Entry {
    id: u32,
    path_off: u32,
    path_len: u16,
    name_off: u32,
    name_len: u16,
    rev_off: u32,
    rev_len: u16,
    size: u64,
    mtime: i64,
    ctime: i64,
    flags: u8,
    is_dir: u8,
}

pub struct MemIndex {
    entries: Vec<Entry>,
    paths: Vec<u8>,
    names: Vec<u8>,
    revs: Vec<u8>,
    by_path: Vec<u32>,
    by_name: Vec<u32>,
    by_rev: Vec<u32>,
    by_size: Vec<u32>,
    by_mtime: Vec<u32>,
    by_ctime: Vec<u32>,
    dir_ids: Vec<u32>,
    file_ids: Vec<u32>,
    hidden_ids: Vec<u32>,
    system_ids: Vec<u32>,
    readonly_ids: Vec<u32>,
    reparse_ids: Vec<u32>,
}

impl MemIndex {
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT id, path, name_l, size, mtime, ctime, flags, is_dir FROM files ORDER BY id",
        )?;
        let mut rows = stmt.query([])?;
        let mut entries: Vec<Entry> = Vec::new();
        let mut paths: Vec<u8> = Vec::new();
        let mut names: Vec<u8> = Vec::new();
        let mut revs: Vec<u8> = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            if id < 0 || id > u32::MAX as i64 {
                anyhow::bail!("file id {id} out of u32 range");
            }
            let path: String = row.get(1)?;
            let name_l: String = row.get(2)?;
            let rev: String = name_l.chars().rev().collect();
            let po = paths.len() as u32;
            paths.extend_from_slice(path.as_bytes());
            let no = names.len() as u32;
            names.extend_from_slice(name_l.as_bytes());
            let ro = revs.len() as u32;
            revs.extend_from_slice(rev.as_bytes());
            entries.push(Entry {
                id: id as u32,
                path_off: po,
                path_len: path.len() as u16,
                name_off: no,
                name_len: name_l.len() as u16,
                rev_off: ro,
                rev_len: rev.len() as u16,
                size: row.get::<_, i64>(3)? as u64,
                mtime: row.get(4)?,
                ctime: row.get(5)?,
                flags: row.get::<_, i64>(6)? as u8,
                is_dir: row.get::<_, i64>(7)? as u8,
            });
        }

        let n = entries.len() as u32;
        let mut perm: Vec<u32> = (0..n).collect();

        let mut by_name = perm.clone();
        by_name.sort_unstable_by(|&a, &b| {
            name_of(&entries, &names, a)
                .cmp(name_of(&entries, &names, b))
                .then(a.cmp(&b))
        });

        let mut by_rev = perm.clone();
        by_rev.sort_unstable_by(|&a, &b| {
            rev_of(&entries, &revs, a)
                .cmp(rev_of(&entries, &revs, b))
                .then(a.cmp(&b))
        });

        let mut by_path = perm.clone();
        by_path.sort_unstable_by(|&a, &b| {
            ci_cmp(path_of(&entries, &paths, a), path_of(&entries, &paths, b)).then(a.cmp(&b))
        });

        let mut by_size = perm.clone();
        by_size.sort_unstable_by(|&a, &b| {
            (entries[a as usize].size, a).cmp(&(entries[b as usize].size, b))
        });
        let mut by_mtime = perm.clone();
        by_mtime.sort_unstable_by(|&a, &b| {
            (entries[a as usize].mtime, a).cmp(&(entries[b as usize].mtime, b))
        });
        let mut by_ctime = perm.clone();
        by_ctime.sort_unstable_by(|&a, &b| {
            (entries[a as usize].ctime, a).cmp(&(entries[b as usize].ctime, b))
        });
        perm.clear();

        let mut dir_ids = Vec::new();
        let mut file_ids = Vec::new();
        let mut hidden_ids = Vec::new();
        let mut system_ids = Vec::new();
        let mut readonly_ids = Vec::new();
        let mut reparse_ids = Vec::new();
        for e in &entries {
            if e.is_dir != 0 {
                dir_ids.push(e.id);
            } else {
                file_ids.push(e.id);
            }
            if e.flags & crate::EntryMeta::FLAG_HIDDEN != 0 {
                hidden_ids.push(e.id);
            }
            if e.flags & crate::EntryMeta::FLAG_SYSTEM != 0 {
                system_ids.push(e.id);
            }
            if e.flags & crate::EntryMeta::FLAG_READONLY != 0 {
                readonly_ids.push(e.id);
            }
            if e.flags & crate::EntryMeta::FLAG_REPARSE != 0 {
                reparse_ids.push(e.id);
            }
        }

        Ok(MemIndex {
            entries,
            paths,
            names,
            revs,
            by_path,
            by_name,
            by_rev,
            by_size,
            by_mtime,
            by_ctime,
            dir_ids,
            file_ids,
            hidden_ids,
            system_ids,
            readonly_ids,
            reparse_ids,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn memory_bytes(&self) -> usize {
        self.entries.len() * std::mem::size_of::<Entry>()
            + self.paths.len()
            + self.names.len()
            + self.revs.len()
            + (self.by_path.len()
                + self.by_name.len()
                + self.by_rev.len()
                + self.by_size.len()
                + self.by_mtime.len()
                + self.by_ctime.len())
                * 4
            + (self.dir_ids.len()
                + self.file_ids.len()
                + self.hidden_ids.len()
                + self.system_ids.len()
                + self.readonly_ids.len()
                + self.reparse_ids.len())
                * 4
    }

    /// True when the SQL path is expected to beat the in-memory engine:
    /// ≥3-char substrings ride the FTS5 trigram index (12-25 ms) while the
    /// memory engine would scan the whole name arena (~65 ms).
    pub fn prefers_sql(q: &Query) -> bool {
        let check = |t: &Term| -> bool {
            match t {
                Term::Name(s) | Term::PathSubstr(s) => {
                    s.chars().count() >= 3
                        && !s.contains('%')
                        && !s.contains('_')
                        && !s.contains('\\')
                }
                _ => false,
            }
        };
        q.include.iter().any(check) || q.exclude.iter().any(check)
    }

    /// Evaluate the whole query in memory; returns matching file ids ascending.
    pub fn search(&self, q: &Query) -> Vec<u32> {
        let mut acc: Option<Vec<u32>> = None;
        for t in &q.include {
            let ids = self.eval(t);
            acc = Some(match acc {
                None => ids,
                Some(a) => intersect(&a, &ids),
            });
            if acc.as_ref().is_some_and(|v| v.is_empty()) {
                return Vec::new();
            }
        }
        let mut acc = acc.unwrap_or_else(|| self.all_ids());
        for t in &q.exclude {
            let ids = self.eval(t);
            acc = subtract(&acc, &ids);
            if acc.is_empty() {
                break;
            }
        }
        acc
    }

    /// Build full hits for ids (order preserved), capped at `limit`.
    pub fn hits(&self, ids: &[u32], limit: usize) -> Vec<Hit> {
        let mut out = Vec::with_capacity(ids.len().min(limit));
        for &id in ids.iter().take(limit) {
            let Ok(idx) = self.entries.binary_search_by_key(&id, |e| e.id) else {
                continue;
            };
            let e = &self.entries[idx];
            let path = String::from_utf8_lossy(
                &self.paths[e.path_off as usize..(e.path_off as usize + e.path_len as usize)],
            )
            .into_owned();
            out.push(Hit {
                path,
                is_dir: e.is_dir != 0,
                size: e.size,
                mtime: e.mtime,
                ctime: e.ctime,
                flags: e.flags,
            });
        }
        out
    }

    fn all_ids(&self) -> Vec<u32> {
        self.entries.iter().map(|e| e.id).collect()
    }

    fn eval(&self, t: &Term) -> Vec<u32> {
        match t {
            Term::Name(s) => self.scan(&self.names, s.as_bytes(), true),
            Term::PathSubstr(s) => self.scan(&self.paths, s.as_bytes(), false),
            Term::Suffix(s) => {
                let rev: String = s.chars().rev().collect();
                self.range_by_rev(rev.as_bytes())
            }
            Term::Ext(list) => {
                let mut out: Vec<u32> = Vec::new();
                for e in list {
                    let rev: String = format!(".{e}").chars().rev().collect();
                    out = union(&out, &self.range_by_rev(rev.as_bytes()));
                }
                out
            }
            Term::NameWild(p) => self.scan_glob(&self.names, p, true),
            Term::PathWild(p) => self.scan_glob(&self.paths, p, false),
            Term::Size { min, max } => self.range_u64(
                &self.by_size,
                |e| e.size,
                min.unwrap_or(0),
                max.unwrap_or(u64::MAX),
            ),
            Term::Mtime { min, max } => self.range_i64(
                &self.by_mtime,
                |e| e.mtime,
                min.unwrap_or(i64::MIN),
                max.unwrap_or(i64::MAX),
            ),
            Term::Ctime { min, max } => self.range_i64(
                &self.by_ctime,
                |e| e.ctime,
                min.unwrap_or(i64::MIN),
                max.unwrap_or(i64::MAX),
            ),
            Term::IsDir(b) => {
                if *b {
                    self.dir_ids.clone()
                } else {
                    self.file_ids.clone()
                }
            }
            Term::Flag { bit, on } => {
                let list = match bit {
                    1 => &self.hidden_ids,
                    2 => &self.system_ids,
                    4 => &self.readonly_ids,
                    _ => &self.reparse_ids,
                };
                if *on {
                    list.clone()
                } else {
                    let mut out = self.all_ids();
                    out.retain(|id| !list.binary_search(id).is_ok());
                    out
                }
            }
            Term::PathPrefix(p) => self.range_by_path_prefix(p.as_bytes()),
        }
    }

    /// SIMD substring scan over an arena (ids come out ascending — we walk in
    /// entry order).
    fn scan(&self, arena: &[u8], needle: &[u8], use_names: bool) -> Vec<u32> {
        let mut out = Vec::new();
        if needle.is_empty() {
            return self.all_ids();
        }
        for (i, e) in self.entries.iter().enumerate() {
            let slice = if use_names {
                &arena[e.name_off as usize..e.name_off as usize + e.name_len as usize]
            } else {
                &arena[e.path_off as usize..e.path_off as usize + e.path_len as usize]
            };
            if memchr::memmem::find(slice, needle).is_some() {
                out.push(e.id);
            }
        }
        out
    }

    fn range_by_rev(&self, rev: &[u8]) -> Vec<u32> {
        // Both predicates must be monotone over the sorted permutation:
        // "r < rev" is false→true; "r < rev OR starts_with" is true→false.
        let lo = self
            .by_rev
            .partition_point(|&i| rev_of(&self.entries, &self.revs, i) < rev);
        let hi = self.by_rev.partition_point(|&i| {
            let r = rev_of(&self.entries, &self.revs, i);
            r < rev || (r.len() >= rev.len() && &r[..rev.len()] == rev)
        });
        let mut out: Vec<u32> = self.by_rev[lo..hi]
            .iter()
            .map(|&i| self.entries[i as usize].id)
            .collect();
        out.sort_unstable();
        out
    }

    /// CI prefix range over the CI-sorted path permutation.
    fn range_by_path_prefix(&self, prefix: &[u8]) -> Vec<u32> {
        let lo = self.by_path.partition_point(|&i| {
            ci_cmp(path_of(&self.entries, &self.paths, i), prefix) == Ordering::Less
        });
        let hi = self.by_path.partition_point(|&i| {
            let p = path_of(&self.entries, &self.paths, i);
            ci_cmp(p, prefix) == Ordering::Less || ci_starts_with(p, prefix)
        });
        let mut out: Vec<u32> = self.by_path[lo..hi]
            .iter()
            .map(|&i| self.entries[i as usize].id)
            .collect();
        out.sort_unstable();
        out
    }

    fn range_u64(&self, perm: &[u32], key: impl Fn(&Entry) -> u64, lo: u64, hi: u64) -> Vec<u32> {
        let a = perm.partition_point(|&i| key(&self.entries[i as usize]) < lo);
        let b = perm.partition_point(|&i| key(&self.entries[i as usize]) < hi);
        let mut out: Vec<u32> = perm[a..b]
            .iter()
            .map(|&i| self.entries[i as usize].id)
            .collect();
        out.sort_unstable();
        out
    }

    fn range_i64(&self, perm: &[u32], key: impl Fn(&Entry) -> i64, lo: i64, hi: i64) -> Vec<u32> {
        let a = perm.partition_point(|&i| key(&self.entries[i as usize]) < lo);
        let b = perm.partition_point(|&i| key(&self.entries[i as usize]) < hi);
        let mut out: Vec<u32> = perm[a..b]
            .iter()
            .map(|&i| self.entries[i as usize].id)
            .collect();
        out.sort_unstable();
        out
    }

    /// Glob scan with a leading-literal prefix narrowing (uses the
    /// byte-sorted name permutation; names are already lowercased).
    fn scan_glob(&self, arena: &[u8], pattern: &str, use_names: bool) -> Vec<u32> {
        let tokens = glob_tokens(pattern);
        let prefix: String = pattern
            .chars()
            .take_while(|c| *c != '*' && *c != '?')
            .flat_map(char::to_lowercase)
            .collect();
        let prefix: Vec<u8> = prefix.into_bytes();
        let mut out = Vec::new();
        if use_names && prefix.len() >= 2 {
            let lo = self
                .by_name
                .partition_point(|&i| name_of(&self.entries, &self.names, i) < prefix.as_slice());
            let hi = self.by_name.partition_point(|&i| {
                let n = name_of(&self.entries, &self.names, i);
                n < prefix.as_slice()
                    || (n.len() >= prefix.len() && &n[..prefix.len()] == prefix.as_slice())
            });
            for &i in &self.by_name[lo..hi] {
                let e = &self.entries[i as usize];
                let name = &arena[e.name_off as usize..e.name_off as usize + e.name_len as usize];
                if glob_match(&tokens, name) {
                    out.push(e.id);
                }
            }
            out.sort_unstable();
            return out;
        }
        for e in &self.entries {
            let slice = if use_names {
                &arena[e.name_off as usize..e.name_off as usize + e.name_len as usize]
            } else {
                &arena[e.path_off as usize..e.path_off as usize + e.path_len as usize]
            };
            if glob_match(&tokens, slice) {
                out.push(e.id);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// pure helpers

fn name_of<'a>(entries: &'a [Entry], names: &'a [u8], i: u32) -> &'a [u8] {
    let e = &entries[i as usize];
    &names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
}

fn rev_of<'a>(entries: &'a [Entry], revs: &'a [u8], i: u32) -> &'a [u8] {
    let e = &entries[i as usize];
    &revs[e.rev_off as usize..e.rev_off as usize + e.rev_len as usize]
}

fn path_of<'a>(entries: &'a [Entry], paths: &'a [u8], i: u32) -> &'a [u8] {
    let e = &entries[i as usize];
    &paths[e.path_off as usize..e.path_off as usize + e.path_len as usize]
}

#[inline]
fn fold(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// ASCII-case-insensitive byte comparison (non-ASCII compares raw).
fn ci_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        let c = fold(a[i]).cmp(&fold(b[i]));
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

fn ci_starts_with(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && ci_cmp(&hay[..needle.len()], needle) == Ordering::Equal
}

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn subtract(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for &x in a {
        while j < b.len() && b[j] < x {
            j += 1;
        }
        if j < b.len() && b[j] == x {
            continue;
        }
        out.push(x);
    }
    out
}

/// Glob tokens, lowercased, consecutive stars collapsed; implicit leading star
/// for substring semantics (consistent with the SQL LIKE path).
#[derive(Debug, Clone, Copy, PartialEq)]
enum GTok {
    Lit(u8),
    Any,
    Star,
}

fn glob_tokens(p: &str) -> Vec<GTok> {
    let mut out: Vec<GTok> = Vec::new();
    for c in p.chars().flat_map(char::to_lowercase) {
        match c {
            '*' => {
                if out.last() != Some(&GTok::Star) {
                    out.push(GTok::Star);
                }
            }
            '?' => out.push(GTok::Any),
            c => {
                let mut buf = [0u8; 4];
                for &b in c.encode_utf8(&mut buf).as_bytes() {
                    out.push(GTok::Lit(b));
                }
            }
        }
    }
    if out.first() != Some(&GTok::Star) {
        out.insert(0, GTok::Star);
    }
    out
}

/// Iterative glob match over bytes with ASCII case folding (no allocation).
fn glob_match(tokens: &[GTok], hay: &[u8]) -> bool {
    let n = hay.len();
    let mut prev = vec![false; n + 1];
    let mut cur = vec![false; n + 1];
    prev[0] = true;
    for &t in tokens {
        match t {
            GTok::Star => {
                cur.fill(false);
                let mut run = prev[0];
                cur[0] = run;
                for j in 1..=n {
                    if prev[j] {
                        run = true;
                    }
                    cur[j] = run;
                }
            }
            _ => {
                cur.fill(false);
                for j in 1..=n {
                    if prev[j - 1] {
                        cur[j] = match t {
                            GTok::Any => true,
                            GTok::Lit(c) => fold(hay[j - 1]) == c,
                            GTok::Star => unreachable!(),
                        };
                    }
                }
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryMeta;
    use crate::store::Store;

    fn build() -> (tempfile::TempDir, Store, MemIndex) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("t.db")).unwrap();
        let mut rb = store.begin_rebuild().unwrap();
        let now = 1_700_000_000i64;
        let rows: &[(&str, EntryMeta)] = &[
            (r"D:\docs\年度报告.md", EntryMeta { size: 100, mtime: now, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\docs\readme.txt", EntryMeta { size: 500, mtime: now - 10_000_000, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\proj\src\main.rs", EntryMeta { size: 2 << 20, mtime: now, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\proj\src\lib.rs", EntryMeta { size: 3 << 20, mtime: now - 100, ctime: now, flags: EntryMeta::FLAG_HIDDEN, ..Default::default() }),
            (r"D:\media\rs.jpg", EntryMeta { size: 9 << 20, mtime: now, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\media\sub", EntryMeta { is_dir: true, size: 0, mtime: now, ctime: now, flags: 0, ..Default::default() }),
        ];
        for (path, meta) in rows {
            rb.insert(path, *meta).unwrap();
        }
        rb.commit().unwrap();
        let mem = MemIndex::load(&store.conn()).unwrap();
        (dir, store, mem)
    }

    #[test]
    fn consistency_with_sql() {
        let (_d, store, mem) = build();
        let queries = [
            "报告",
            "rs",
            "*.rs",
            "ext:rs",
            "ext:jpg",
            "size:>1mb",
            "size:1mb-10mb",
            "type:dir",
            "type:file",
            "hidden:true",
            "dm:thismonth",
            r"parent:D:\proj",
            r"path:D:\proj\src",
            "main*",
            "*.jpg",
            "ext:rs size:>1mb",
            "!hidden:true",
            "年度报告",
        ];
        for q in queries {
            eprintln!("QUERY: {q}");
            let parsed = Query::parse(q).unwrap();
            let mem_ids = mem.search(&parsed);
            let mem_paths: Vec<String> = mem
                .hits(&mem_ids, 1000)
                .into_iter()
                .map(|h| h.path)
                .collect();
            let sql_hits = store.search_query(&parsed, None).unwrap();
            let sql_paths: Vec<String> = sql_hits.hits.into_iter().map(|h| h.path).collect();
            let mut m = mem_paths.clone();
            let mut s = sql_paths.clone();
            m.sort();
            s.sort();
            assert_eq!(m, s, "query {q}: mem vs sql mismatch");
        }
    }

    #[test]
    fn glob_tokens_and_match() {
        let t = glob_tokens("*.rs");
        assert!(glob_match(&t, b"main.rs"));
        assert!(!glob_match(&t, b"main.rss"));
        let t = glob_tokens("a?c");
        assert!(glob_match(&t, b"xxabc"));
        assert!(!glob_match(&t, b"xxabbc"));
        // case-insensitive, wildcard pattern
        let t = glob_tokens("READme*");
        assert!(glob_match(&t, b"readme.txt"));
    }

    #[test]
    fn mem_hits_fields() {
        let (_d, _store, mem) = build();
        let q = Query::parse("lib.rs").unwrap();
        let ids = mem.search(&q);
        let hits = mem.hits(&ids, 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].flags & EntryMeta::FLAG_HIDDEN != 0);
        assert_eq!(hits[0].size, 3 << 20);
        assert!(hits[0].path.ends_with("lib.rs"));
    }

    #[test]
    fn empty_query_matches_all() {
        let (_d, _store, mem) = build();
        let q = Query::parse("").unwrap();
        assert_eq!(mem.search(&q).len(), 6);
    }
}
