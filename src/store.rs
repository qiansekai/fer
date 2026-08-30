//! SQLite + FTS5(trigram) persistent index with millisecond queries.
//!
//! * substring >= 3 chars → FTS5 trigram `MATCH`
//! * substring 1-2 chars  → `instr` on the lowercased column
//! * `*.rs`-style suffix  → reversed column index range
//! * wildcards            → `LIKE` (glob `*`/`?` translated to `%`/`_`)
//! * structured query language → see [`crate::query`]

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::EntryMeta;
use crate::basename;
use crate::matcher::has_wildcards;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id      INTEGER PRIMARY KEY,
    path    TEXT NOT NULL UNIQUE,
    path_l  TEXT NOT NULL,
    name    TEXT NOT NULL,
    name_l  TEXT NOT NULL,
    name_r  TEXT NOT NULL DEFAULT '',
    path_r  TEXT NOT NULL DEFAULT '',
    is_dir  INTEGER NOT NULL DEFAULT 0,
    size    INTEGER NOT NULL DEFAULT 0,
    mtime   INTEGER NOT NULL DEFAULT 0,
    ctime   INTEGER NOT NULL DEFAULT 0,
    flags   INTEGER NOT NULL DEFAULT 0,
    frn     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_files_name_l ON files(name_l);
CREATE INDEX IF NOT EXISTS idx_files_name_r ON files(name_r);
CREATE INDEX IF NOT EXISTS idx_files_path_r ON files(path_r);
CREATE INDEX IF NOT EXISTS idx_files_path_l ON files(path_l);
CREATE INDEX IF NOT EXISTS idx_files_frn ON files(frn);
CREATE INDEX IF NOT EXISTS idx_files_mtime ON files(mtime);
CREATE INDEX IF NOT EXISTS idx_files_size ON files(size);
CREATE INDEX IF NOT EXISTS idx_files_is_dir ON files(is_dir);
CREATE INDEX IF NOT EXISTS idx_flags_hidden ON files(flags) WHERE (flags & 1) != 0;
CREATE INDEX IF NOT EXISTS idx_flags_system ON files(flags) WHERE (flags & 2) != 0;
CREATE INDEX IF NOT EXISTS idx_flags_readonly ON files(flags) WHERE (flags & 4) != 0;
CREATE INDEX IF NOT EXISTS idx_flags_reparse ON files(flags) WHERE (flags & 8) != 0;
CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(path_l, name_l, tokenize='trigram');
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

/// Column set expected by the current code; anything older triggers the
/// (destructive) migration below.
const MIGRATION_COLS: &[&str] = &["name_r", "mtime", "ctime", "flags"];

const CREATE_FILES_DDL: &str = r#"
CREATE TABLE files (
    id      INTEGER PRIMARY KEY,
    path    TEXT NOT NULL UNIQUE,
    path_l  TEXT NOT NULL,
    name    TEXT NOT NULL,
    name_l  TEXT NOT NULL,
    name_r  TEXT NOT NULL DEFAULT '',
    path_r  TEXT NOT NULL DEFAULT '',
    is_dir  INTEGER NOT NULL DEFAULT 0,
    size    INTEGER NOT NULL DEFAULT 0,
    mtime   INTEGER NOT NULL DEFAULT 0,
    ctime   INTEGER NOT NULL DEFAULT 0,
    flags   INTEGER NOT NULL DEFAULT 0,
    frn     INTEGER
);
CREATE INDEX idx_files_name_l ON files(name_l);
CREATE INDEX idx_files_name_r ON files(name_r);
CREATE INDEX idx_files_path_r ON files(path_r);
CREATE INDEX idx_files_path_l ON files(path_l);
CREATE INDEX idx_files_frn ON files(frn);
CREATE INDEX idx_files_mtime ON files(mtime);
CREATE INDEX idx_files_size ON files(size);
CREATE INDEX idx_files_is_dir ON files(is_dir);
CREATE INDEX idx_flags_hidden ON files(flags) WHERE (flags & 1) != 0;
CREATE INDEX idx_flags_system ON files(flags) WHERE (flags & 2) != 0;
CREATE INDEX idx_flags_readonly ON files(flags) WHERE (flags & 4) != 0;
CREATE INDEX idx_flags_reparse ON files(flags) WHERE (flags & 8) != 0;
CREATE VIRTUAL TABLE files_fts USING fts5(path_l, name_l, tokenize='trigram');
"#;

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub flags: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub total: u64,
}

pub struct Store {
    conn: Connection,
    db_path: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // mmap the DB (OS-managed, evictable): full-scan fallbacks (short CJK
        // queries, complex wildcards) then run at memory speed after warm-up
        // instead of cold disk reads.
        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if file_size > 0 {
            let _ = conn.pragma_update(None, "mmap_size", file_size.min(1 << 30) as i64);
        }
        conn.execute_batch(SCHEMA)?;
        // Superseded fat covering index: it no longer covers the metadata
        // columns but still lures the planner away from the slim indexes.
        conn.execute_batch("DROP INDEX IF EXISTS idx_files_name_path;")?;
        // Schema migration: DBs missing any current column get their data
        // tables recreated — index is lost and the caller must reindex;
        // half-migrating 4M rows is not worth it.
        let missing = {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('files')")?;
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            MIGRATION_COLS.iter().any(|c| !cols.iter().any(|x| x == c))
        };
        if missing {
            conn.execute_batch("DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS files_fts;")?;
            conn.execute_batch(CREATE_FILES_DDL)?;
            eprintln!("[store] schema upgraded — index cleared, run `fer index`");
        }
        Ok(Store {
            conn,
            db_path: path.to_path_buf(),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Raw connection (for the in-memory index loader).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Build the serve-mode in-memory name index from the current data.
    pub fn load_mem_index(&self) -> Result<crate::mem::MemIndex> {
        crate::mem::MemIndex::load(&self.conn)
    }

    /// Fetch full hits for a set of file ids, preserving the input order
    /// (used by the serve-mode memory index).
    pub fn fetch_ids(&self, ids: &[i64]) -> Result<Vec<Hit>> {
        let row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Hit> {
            Ok(Hit {
                path: r.get(0)?,
                is_dir: r.get::<_, i64>(1)? != 0,
                size: r.get::<_, i64>(2)? as u64,
                mtime: r.get(3)?,
                ctime: r.get(4)?,
                flags: r.get::<_, i64>(5)? as u8,
            })
        };
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT path, is_dir, size, mtime, ctime, flags FROM files WHERE id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = rusqlite::params_from_iter(chunk.iter().copied());
            for h in stmt.query_map(params, row)? {
                out.push(h?);
            }
        }
        Ok(out)
    }

    /// Start a full rebuild. Old rows are wiped; inserts are buffered in one
    /// transaction with `synchronous=OFF` for speed and restored on commit.
    /// Takes `&self` (Connection is internally mutable) so the store stays
    /// usable for `set_meta` while the rebuild is in flight.
    pub fn begin_rebuild(&self) -> Result<Rebuild<'_>> {
        self.conn.execute_batch(
            "BEGIN;
             DELETE FROM files;
             DROP TABLE IF EXISTS files_fts;
             CREATE VIRTUAL TABLE files_fts USING fts5(path_l, name_l, tokenize='trigram');
             COMMIT;",
        )?;
        self.conn.pragma_update(None, "synchronous", "OFF")?;
        self.conn.execute_batch("BEGIN;")?;
        let files_stmt = self.conn.prepare(
            "INSERT OR IGNORE INTO files(path, path_l, name, name_l, name_r, path_r, is_dir, size, mtime, ctime, flags, frn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        let fts_stmt = self.conn
            .prepare("INSERT INTO files_fts(rowid, path_l, name_l) VALUES (?1, ?2, ?3)")?;
        Ok(Rebuild {
            conn: &self.conn,
            files_stmt: Some(files_stmt),
            fts_stmt: Some(fts_stmt),
            count: 0,
            committed: false,
        })
    }

    /// Search with a plain pattern. `path_mode` matches the full path instead
    /// of the basename. Delegates to [`Store::search_query`].
    pub fn search(&self, pattern: &str, path_mode: bool, limit: Option<usize>) -> Result<SearchResult> {
        use crate::query::Term;
        let has_sep = pattern.contains('\\') || pattern.contains('/');
        let term = if path_mode || has_sep {
            if has_wildcards(pattern) {
                Term::PathWild(pattern.to_lowercase())
            } else {
                Term::PathSubstr(pattern.to_lowercase())
            }
        } else if let Some(suffix) = try_suffix_literal(pattern) {
            Term::Suffix(suffix.to_lowercase())
        } else if has_wildcards(pattern) {
            Term::NameWild(pattern.to_lowercase())
        } else {
            Term::Name(pattern.to_lowercase())
        };
        let q = crate::query::Query {
            include: vec![term],
            exclude: Vec::new(),
            raw: pattern.to_string(),
        };
        self.search_query(&q, limit)
    }

    /// Search with the structured query language (see [`crate::query`]).
    pub fn search_query(&self, q: &crate::query::Query, limit: Option<usize>) -> Result<SearchResult> {
        let lim = limit.map(|l| l.min(100_000) as i64);
        let row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Hit> {
            Ok(Hit {
                path: r.get(0)?,
                is_dir: r.get::<_, i64>(1)? != 0,
                size: r.get::<_, i64>(2)? as u64,
                mtime: r.get(3)?,
                ctime: r.get(4)?,
                flags: r.get::<_, i64>(5)? as u8,
            })
        };
        let mut hit_conds: Vec<String> = Vec::with_capacity(q.include.len() + q.exclude.len());
        let mut count_conds: Vec<String> = Vec::with_capacity(q.include.len() + q.exclude.len());
        for t in &q.include {
            hit_conds.push(Self::term_sql(t, false));
            count_conds.push(Self::term_sql(t, true));
        }
        for t in &q.exclude {
            hit_conds.push(format!("NOT ({})", Self::term_sql(t, false)));
            count_conds.push(format!("NOT ({})", Self::term_sql(t, true)));
        }
        let where_sql = if hit_conds.is_empty() {
            "1=1".to_string()
        } else {
            hit_conds.join(" AND ")
        };
        let count_where = if count_conds.is_empty() {
            "1=1".to_string()
        } else {
            count_conds.join(" AND ")
        };
        let lsql = lim_sql(lim);
        let sql_hits = format!(
            "SELECT path, is_dir, size, mtime, ctime, flags FROM files WHERE {where_sql} {lsql}"
        );
        // COUNT variants avoid rowid subqueries: direct ranges run over the
        // covering slim indexes (name_r/path_l/mtime/size/is_dir/flags).
        let sql_count = format!("SELECT COUNT(*) FROM files WHERE {count_where}");
        let mut stmt = self.conn.prepare(&sql_hits)?;
        let hits: Vec<Hit> = stmt.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
        let total: u64 = if lim.is_some() {
            self.conn.query_row(&sql_count, [], |r| r.get::<_, i64>(0))? as u64
        } else {
            hits.len() as u64
        };
        Ok(SearchResult { hits, total })
    }

    /// COUNT-only variant of [`Store::search_query`] — skips the hits query
    /// entirely (halves latency for scan-bound queries like 2-char CJK).
    pub fn count_query(&self, q: &crate::query::Query) -> Result<u64> {
        let mut count_conds: Vec<String> = Vec::with_capacity(q.include.len() + q.exclude.len());
        for t in &q.include {
            count_conds.push(Self::term_sql(t, true));
        }
        for t in &q.exclude {
            count_conds.push(format!("NOT ({})", Self::term_sql(t, true)));
        }
        let count_where = if count_conds.is_empty() {
            "1=1".to_string()
        } else {
            count_conds.join(" AND ")
        };
        let sql_count = format!("SELECT COUNT(*) FROM files WHERE {count_where}");
        Ok(self.conn.query_row(&sql_count, [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Translate one query term into a SQL condition (all literals inlined and
    /// escaped; the query language is trusted input by design). `for_count`
    /// emits the rowid-subquery-free form so COUNT runs over covering indexes.
    fn term_sql(t: &crate::query::Term, for_count: bool) -> String {
        use crate::query::Term;
        match t {
            Term::Name(s) => name_substring_sql(s, "name_l", for_count),
            Term::PathSubstr(s) => name_substring_sql(s, "path_l", for_count),
            Term::Suffix(s) => suffix_sql(s, "name_r", for_count),
            Term::NameWild(p) => like_sql("name_l", p, for_count),
            Term::PathWild(p) => like_sql("path_l", p, for_count),
            Term::Ext(list) => {
                let parts: Vec<String> = list
                    .iter()
                    .map(|e| suffix_sql(&format!(".{e}"), "name_r", for_count))
                    .collect();
                format!("({})", parts.join(" OR "))
            }
            Term::Size { min, max } => {
                let mut c = Vec::new();
                if let Some(m) = min {
                    c.push(format!("size >= {m}"));
                }
                if let Some(m) = max {
                    c.push(format!("size < {m}"));
                }
                c.join(" AND ")
            }
            Term::Mtime { min, max } => time_sql("mtime", *min, *max),
            Term::Ctime { min, max } => time_sql("ctime", *min, *max),
            Term::IsDir(b) => format!("is_dir = {}", if *b { 1 } else { 0 }),
            Term::Flag { bit, on } => {
                if *on {
                    format!("(flags & {bit}) != 0")
                } else {
                    format!("(flags & {bit}) = 0")
                }
            }
            Term::PathPrefix(p) => prefix_sql("path_l", p, for_count),
        }
    }

    pub fn counts(&self) -> Result<(u64, u64)> {
        let files = self.conn
            .query_row("SELECT COUNT(*) FROM files WHERE is_dir = 0", [], |r| r.get::<_, i64>(0))? as u64;
        let dirs = self.conn
            .query_row("SELECT COUNT(*) FROM files WHERE is_dir = 1", [], |r| r.get::<_, i64>(0))? as u64;
        Ok((files, dirs))
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Delete by NTFS file reference number (monitor fast path; deletions often
    /// cannot be resolved to a path because the MFT record is already gone).
    pub fn delete_by_frn(&self, frn: u64) -> Result<()> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM files WHERE frn = ?1", [frn as i64], |r| r.get(0))
            .optional()?;
        if let Some(id) = id {
            self.conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
            self.conn
                .execute("DELETE FROM files_fts WHERE rowid = ?1", [id])?;
        }
        Ok(())
    }

    pub fn upsert(&self, path: &str, meta: EntryMeta) -> Result<()> {
        let path_l = path.to_lowercase();
        let name = basename(path).to_owned();
        let name_l = name.to_lowercase();
        let name_r: String = name_l.chars().rev().collect();
        let path_r: String = path_l.chars().rev().collect();
        let existing: Option<i64> = self
            .conn
            .query_row("SELECT id FROM files WHERE path = ?1", [path], |r| r.get(0))
            .optional()?;
        match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE files SET path_l = ?2, name = ?3, name_l = ?4, name_r = ?5, path_r = ?6, \
                     is_dir = ?7, size = ?8, mtime = ?9, ctime = ?10, flags = ?11, frn = ?12 WHERE id = ?1",
                    params![
                        id, &path_l, &name, &name_l, &name_r, &path_r,
                        meta.is_dir as i64, meta.size as i64, meta.mtime, meta.ctime,
                        meta.flags as i64, meta.frn.map(|f| f as i64)
                    ],
                )?;
                self.conn
                    .execute("DELETE FROM files_fts WHERE rowid = ?1", [id])?;
                self.conn.execute(
                    "INSERT INTO files_fts(rowid, path_l, name_l) VALUES (?1, ?2, ?3)",
                    params![id, &path_l, &name_l],
                )?;
            }
            None => {
                self.conn.execute(
                    "INSERT INTO files(path, path_l, name, name_l, name_r, path_r, is_dir, size, mtime, ctime, flags, frn)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        path, &path_l, &name, &name_l, &name_r, &path_r,
                        meta.is_dir as i64, meta.size as i64, meta.mtime, meta.ctime,
                        meta.flags as i64, meta.frn.map(|f| f as i64)
                    ],
                )?;
                let id = self.conn.last_insert_rowid();
                self.conn.execute(
                    "INSERT INTO files_fts(rowid, path_l, name_l) VALUES (?1, ?2, ?3)",
                    params![id, &path_l, &name_l],
                )?;
            }
        }
        Ok(())
    }
}

/// Active full-rebuild transaction. Rolls back on drop if not committed.
pub struct Rebuild<'a> {
    conn: &'a Connection,
    files_stmt: Option<rusqlite::Statement<'a>>,
    fts_stmt: Option<rusqlite::Statement<'a>>,
    count: u64,
    committed: bool,
}

impl Rebuild<'_> {
    pub fn insert(&mut self, path: &str, meta: EntryMeta) -> Result<()> {
        let path_l = path.to_lowercase();
        let name = basename(path).to_owned();
        let name_l = name.to_lowercase();
        // Reversed columns turn suffix wildcards (`*.rs`) into indexed prefix
        // lookups (see `search`).
        let name_r: String = name_l.chars().rev().collect();
        let path_r: String = path_l.chars().rev().collect();
        let changed = self
            .files_stmt
            .as_mut()
            .unwrap()
            .execute(params![
                path,
                &path_l,
                &name,
                &name_l,
                &name_r,
                &path_r,
                meta.is_dir as i64,
                meta.size as i64,
                meta.mtime,
                meta.ctime,
                meta.flags as i64,
                meta.frn.map(|f| f as i64)
            ])?;
        if changed == 0 {
            // OR IGNORE swallowed a duplicate path (recycled FRN during churn)
            // — skip the FTS row as well.
            return Ok(());
        }
        let id = self.conn.last_insert_rowid();
        self.fts_stmt
            .as_mut()
            .unwrap()
            .execute(params![id, &path_l, &name_l])?;
        self.count += 1;
        Ok(())
    }

    pub fn commit(mut self) -> Result<u64> {
        self.files_stmt.take();
        self.fts_stmt.take();
        self.conn.execute_batch("COMMIT;")?;
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        self.committed = true;
        Ok(self.count)
    }
}

impl Drop for Rebuild<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.conn.execute_batch("ROLLBACK;");
            let _ = self.conn.pragma_update(None, "synchronous", "NORMAL");
        }
    }
}

/// Translate a glob (`*`, `?`) into a SQL `LIKE` pattern with substring
/// semantics: `LIKE` is anchored at both ends, so a leading `%` is added
/// unless the glob already starts with `*`. `ESCAPE '\'` expected.
fn glob_to_like(glob: &str) -> String {
    let mut out = String::with_capacity(glob.len() + 8);
    if !glob.starts_with('*') {
        out.push('%');
    }
    for c in glob.chars() {
        match c {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// If the glob is a pure suffix pattern (`*` + literal, no other wildcards),
/// return the literal suffix; otherwise `None`.
pub(crate) fn try_suffix_literal(glob: &str) -> Option<String> {
    let mut chars = glob.chars();
    if chars.next() != Some('*') {
        return None;
    }
    let rest: String = chars.collect();
    if rest.is_empty() || rest.contains('*') || rest.contains('?') {
        return None;
    }
    Some(rest)
}

/// Escape a string for inlining as a SQL string literal.
fn sql_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Half-open range `[prefix, prefix+1)` for an exact string-prefix index scan.
/// `None` only if the prefix ends with U+10FFFF (impossible in practice).
fn prefix_bounds(prefix: &str) -> Option<(String, String)> {
    let mut chars: Vec<char> = prefix.chars().collect();
    let last = *chars.last()?;
    let bumped = (last as u32).checked_add(1).and_then(char::from_u32)?;
    chars.pop();
    chars.push(bumped);
    Some((prefix.to_string(), chars.into_iter().collect()))
}

/// `LIMIT n` clause for an already-inlined integer (no parameters).
fn lim_sql(lim: Option<i64>) -> String {
    match lim {
        Some(l) => format!("LIMIT {l}"),
        None => String::new(),
    }
}

/// Substring condition on a lowercased column: FTS5 trigram (>= 3 chars) or
/// `instr` (1-2 chars). The hits form wraps the scan in a rowid subquery so
/// the planner scans the slim index instead of the whole table.
fn name_substring_sql(s: &str, col: &str, for_count: bool) -> String {
    if s.chars().count() >= 3 {
        let q = format!("{col} : \"{}\"", s.replace('"', "\"\""));
        format!(
            "id IN (SELECT rowid FROM files_fts WHERE files_fts MATCH '{}')",
            sql_literal(&q)
        )
    } else if for_count {
        format!("instr({col}, '{}') > 0", sql_literal(s))
    } else {
        format!(
            "id IN (SELECT id FROM files WHERE instr({col}, '{}') > 0)",
            sql_literal(s)
        )
    }
}

/// Wildcard glob condition; same subquery trick for the hits form.
fn like_sql(col: &str, glob: &str, for_count: bool) -> String {
    let lit = sql_literal(&glob_to_like(glob));
    if for_count {
        format!("{col} LIKE '{lit}' ESCAPE '\\'")
    } else {
        format!(
            "id IN (SELECT id FROM files WHERE {col} LIKE '{lit}' ESCAPE '\\')"
        )
    }
}

/// Exact-suffix condition via the reversed column's index range. The COUNT
/// form uses the direct range (covering index), the hits form wraps it in a
/// rowid subquery so the metadata columns resolve via cheap point lookups.
fn suffix_sql(suffix: &str, col_r: &str, for_count: bool) -> String {
    let rev: String = suffix.chars().rev().collect();
    if let Some((lo, hi)) = prefix_bounds(&rev) {
        let lo = sql_literal(&lo);
        let hi = sql_literal(&hi);
        if for_count {
            format!("{col_r} >= '{lo}' AND {col_r} < '{hi}'")
        } else {
            format!(
                "id IN (SELECT id FROM files WHERE {col_r} >= '{lo}' AND {col_r} < '{hi}')"
            )
        }
    } else {
        format!("instr({col_r}, '{}') > 0", sql_literal(&rev))
    }
}

/// Full-path prefix condition (`parent:` / `path:`).
fn prefix_sql(col: &str, prefix: &str, for_count: bool) -> String {
    if let Some((lo, hi)) = prefix_bounds(prefix) {
        let lo = sql_literal(&lo);
        let hi = sql_literal(&hi);
        if for_count {
            format!("{col} >= '{lo}' AND {col} < '{hi}'")
        } else {
            format!("id IN (SELECT id FROM files WHERE {col} >= '{lo}' AND {col} < '{hi}')")
        }
    } else {
        format!("{col} >= '{}'", sql_literal(prefix))
    }
}

fn time_sql(col: &str, min: Option<i64>, max: Option<i64>) -> String {
    let mut c = Vec::new();
    if let Some(m) = min {
        c.push(format!("{col} >= {m}"));
    }
    if let Some(m) = max {
        c.push(format!("{col} < {m}"));
    }
    if c.is_empty() {
        "1=1".to_string()
    } else {
        c.join(" AND ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    fn meta(is_dir: bool) -> EntryMeta {
        EntryMeta { is_dir, ..Default::default() }
    }

    fn seed(store: &mut Store) {
        let mut rb = store.begin_rebuild().unwrap();
        for (path, is_dir) in [
            (r"D:\proj\src\main.rs", false),
            (r"D:\proj\src\lib.rs", false),
            (r"D:\docs\AnnualReport2026.pdf", false),
            (r"D:\docs\年度报告.md", false),
            (r"D:\docs", true),
        ] {
            rb.insert(path, meta(is_dir)).unwrap();
        }
        rb.commit().unwrap();
    }

    #[test]
    fn substring_fts5_ascii() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let r = store.search("report", false, None).unwrap();
        assert_eq!(r.total, 1);
        assert!(r.hits[0].path.ends_with("AnnualReport2026.pdf"));
    }

    #[test]
    fn substring_fts5_cjk() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let r = store.search("报告", false, None).unwrap(); // 2 chars → instr fallback
        assert_eq!(r.total, 1);
        assert!(r.hits[0].path.ends_with("年度报告.md"));
        let r = store.search("年度报告", false, None).unwrap(); // 4 chars → trigram
        assert_eq!(r.total, 1);
    }

    #[test]
    fn substring_short_fallback() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let r = store.search("rs", false, None).unwrap(); // 2 chars
        assert_eq!(r.total, 2); // main.rs + lib.rs
    }

    #[test]
    fn case_insensitive() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let r = store.search("REPORT", false, None).unwrap();
        assert_eq!(r.total, 1);
    }

    #[test]
    fn wildcard_like() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let r = store.search("*.rs", false, None).unwrap();
        assert_eq!(r.total, 2);
        let r = store.search("a?c*", false, None).unwrap();
        assert_eq!(r.total, 0);
    }

    #[test]
    fn suffix_path_with_special_chars() {
        let (_d, mut store) = test_store();
        {
            let mut rb = store.begin_rebuild().unwrap();
            rb.insert(r"D:\x\100%.txt", meta(false)).unwrap();
            rb.insert(r"D:\x\file_(1).rs", meta(false)).unwrap();
            rb.insert(r"D:\x\plain.rs", meta(false)).unwrap();
            rb.commit().unwrap();
        }
        // suffix with literal '%' must match only the real 100%.txt
        let r = store.search("*.txt", false, None).unwrap();
        assert_eq!(r.total, 1);
        // suffix with '_' is a literal, not a single-char wildcard
        let r = store.search("*_(1).rs", false, None).unwrap();
        assert_eq!(r.total, 1);
        assert!(r.hits[0].path.ends_with("file_(1).rs"));
        // mixed wildcard ('*a*' is not a pure suffix) falls back to LIKE,
        // and only plain.rs contains 'a'
        let r = store.search("*a*.rs", false, None).unwrap();
        assert_eq!(r.total, 1);
        assert!(r.hits[0].path.ends_with("plain.rs"));
    }

    #[test]
    fn path_mode() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let r = store.search("src", true, None).unwrap();
        assert_eq!(r.total, 2);
        let r = store.search("src\\main*", false, None).unwrap(); // auto path mode
        assert_eq!(r.total, 1);
        assert!(r.hits[0].path.ends_with("main.rs"));
    }

    #[test]
    fn limit_and_total() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let r = store.search("rs", false, Some(1)).unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.total, 2);
    }

    #[test]
    fn delete_by_frn_and_upsert() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        // seed used frn=None; exercise the frn paths explicitly
        store.upsert(r"D:\tmp\newfile.txt", EntryMeta { is_dir: false, frn: Some(777), ..Default::default() }).unwrap();
        assert_eq!(store.search("newfile", false, None).unwrap().total, 1);
        store.delete_by_frn(777).unwrap();
        assert_eq!(store.search("newfile", false, None).unwrap().total, 0);
        // upsert over existing path
        store.upsert(r"D:\tmp\newfile.txt", EntryMeta { is_dir: false, frn: Some(778), ..Default::default() }).unwrap();
        store.upsert(r"D:\tmp\newfile.txt", EntryMeta { is_dir: true, frn: Some(778), ..Default::default() }).unwrap();
        let r = store.search("newfile", false, None).unwrap();
        assert_eq!(r.total, 1);
        assert!(r.hits[0].is_dir);
    }

    #[test]
    fn counts() {
        let (_d, mut store) = test_store();
        seed(&mut store);
        let (files, dirs) = store.counts().unwrap();
        assert_eq!(files, 4);
        assert_eq!(dirs, 1);
    }

    #[test]
    fn query_language_filters() {
        let (_d, mut store) = test_store();
        {
            let mut rb = store.begin_rebuild().unwrap();
            let now = chrono::Local::now().timestamp();
            rb.insert(r"D:\photos\a.jpg", EntryMeta { is_dir: false, size: 2 << 20, mtime: now, ctime: now - 100, frn: None, flags: EntryMeta::FLAG_HIDDEN }).unwrap();
            rb.insert(r"D:\photos\b.png", EntryMeta { is_dir: false, size: 500, mtime: now - 999_999, ctime: now, frn: None, flags: 0 }).unwrap();
            rb.insert(r"D:\photos\sub", EntryMeta { is_dir: true, size: 0, mtime: now, ctime: 0, frn: None, flags: 0 }).unwrap();
            rb.insert(r"D:\photos\sub\c.jpg", EntryMeta { is_dir: false, size: 9 << 20, mtime: now, ctime: now, frn: None, flags: 0 }).unwrap();
            rb.commit().unwrap();
        }
        let q = crate::query::Query::parse("ext:jpg").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 2);
        let q = crate::query::Query::parse("ext:jpg size:>1mb").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 2);
        let q = crate::query::Query::parse("ext:jpg size:5mb-20mb").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 1);
        let q = crate::query::Query::parse("type:dir").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 1);
        assert!(crate::query::Query::parse("hidden:").is_err());
        let q = crate::query::Query::parse("hidden:true").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 1);
        let q = crate::query::Query::parse("dm:thisweek").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 3); // a.jpg, sub, c.jpg
        let q = crate::query::Query::parse(r"parent:D:\photos").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 4); // subtree incl. sub\c.jpg
        let q = crate::query::Query::parse("ext:jpg !hidden:true").unwrap();
        assert_eq!(store.search_query(&q, None).unwrap().total, 1); // only sub\c.jpg
    }
}
