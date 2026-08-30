//! Full in-memory search engine for serve mode — the Everything-style route:
//! queries never touch the database, they run against compact sorted arrays
//! and SIMD scans.
//!
//! Layout (56-byte packed `Entry` + string arenas, ≈1 GB for 4M files):
//! * one packed `Entry` per file (id, arena offsets, size, timestamps, flags)
//! * `paths` (original case, for display + CI prefix search),
//!   `names` (lowercased, for substring/glob scans),
//!   `revs` (reversed lowercased names, for suffix binary search)
//! * sorted permutations: `by_path` (CI order), `by_rev`, `by_size`,
//!   `by_mtime`, `by_ctime` — all queries reduce to two binary searches
//!   (partition points) over one of these
//!
//! All 12 query-language terms evaluate in memory. SQLite survives only as a
//! dev/test oracle behind the `sqlite` feature — production queries (CLI,
//! serve, monitor) never touch it.
//!
//! Known divergence: path CI ordering folds ASCII case only; non-ASCII
//! letters with Unicode case (É/Ö/ü) compare bytewise — SQLite's fallback
//! lowercases them. Extremely rare in Windows paths.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
#[cfg(feature = "sqlite")]
use rusqlite::Connection;

use crate::EntryMeta;
use crate::Hit;
use crate::basename;
use crate::fold_lower;
use crate::lower_rev;
use crate::query::{Query, Term};

/// Dump file magic + format version. v2 pads every section to 8-byte
/// alignment so the file can be memory-mapped and viewed in place (zero-copy
/// loading); v1 had unaligned sections and is rejected.
const MAGIC: &[u8; 8] = b"FERIDX01";
const FORMAT_VERSION: u32 = 2;

/// Fixed dump section order: entries, paths, names, revs, the six sorted
/// permutations, then the six id lists. The header stores byte offsets for
/// each section plus the total file length (SEC+1 table entries), followed by
/// the six id-list element counts — logical lengths come from these, so
/// inter-section alignment padding never leaks into the data.
const SEC: usize = 16;
const HDR_LEN: usize = 32 + (SEC + 1) * 8 + 6 * 4; // 192

/// Packed, dump-stable 56-byte record. Field order groups the u64s so there
/// is no interior padding; the dump is written/read as raw little-endian
/// sections, so this layout is part of the on-disk format.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Entry {
    size: u64,
    mtime: i64,
    ctime: i64,
    frn: u64,
    id: u32,
    path_off: u32,
    name_off: u32,
    rev_off: u32,
    path_len: u16,
    name_len: u16,
    rev_len: u16,
    flags: u8,
    is_dir: u8,
}

const _: () = assert!(std::mem::size_of::<Entry>() == 56);

/// Streaming builder: feeds (path, meta) into the compact structure; IDs are
/// assigned sequentially and FRNs carried for the monitor.
#[derive(Default)]
pub struct MemBuilder {
    entries: Vec<Entry>,
    paths: Vec<u8>,
    names: Vec<u8>,
    revs: Vec<u8>,
}

impl MemBuilder {
    pub fn push(&mut self, path: &str, meta: EntryMeta) {
        let id = self.entries.len() as u32;
        let name = basename(path);
        let name_l = fold_lower(name);
        let rev = lower_rev(&name_l);
        let po = self.paths.len() as u32;
        self.paths.extend_from_slice(path.as_bytes());
        let no = self.names.len() as u32;
        self.names.extend_from_slice(name_l.as_bytes());
        let ro = self.revs.len() as u32;
        self.revs.extend_from_slice(rev.as_bytes());
        self.entries.push(Entry {
            id,
            path_off: po,
            path_len: path.len() as u16,
            name_off: no,
            name_len: name_l.len() as u16,
            rev_off: ro,
            rev_len: rev.len() as u16,
            size: meta.size,
            mtime: meta.mtime,
            ctime: meta.ctime,
            flags: meta.flags,
            is_dir: meta.is_dir as u8,
            frn: meta.frn.unwrap_or(0),
        });
    }

    pub fn finish(self) -> MemIndex {
        finalize(self.entries, self.paths, self.names, self.revs)
    }
}

pub struct MemIndex {
    _keep: Keep,
    sec: Sections,
}

/// What keeps the section memory alive. `Owned` holds the Vecs produced by
/// finalize (their heap buffers are stable across moves, so the raw pointers
/// in `Sections` stay valid); `Mapped` holds the mmap of a dump file.
/// (Payloads are ownership anchors only — the live data is reached through the
/// `Sections` views — hence the dead_code allow; one MemIndex holds exactly
/// one variant, so the large-variant lint is moot here.)
#[allow(dead_code, clippy::large_enum_variant)]
enum Keep {
    Owned(OwnedData),
    Mapped(memmap2::Mmap),
}

#[derive(Default)]
struct OwnedData {
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

/// Immutable pointer+len slice view. Backed either by the owned Vecs or by the
/// mmap'd dump; read-only after construction. Send/Sync are implemented
/// manually because raw pointers are !Send — soundness rests on the sections
/// never being mutated or reallocated once published (MemIndex is read-only).
#[derive(Clone, Copy)]
struct View<T> {
    ptr: *const T,
    len: usize,
}

unsafe impl<T: Send + Sync> Send for View<T> {}
unsafe impl<T: Send + Sync> Sync for View<T> {}

impl<T> View<T> {
    fn from_slice(s: &[T]) -> Self {
        View { ptr: s.as_ptr(), len: s.len() }
    }
    fn slice(&self) -> &[T] {
        // SAFETY: the pointer/len were captured from a live allocation (owned
        // Vec or mapping) that `Keep` still owns, and are never mutated.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<T> std::ops::Deref for View<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.slice()
    }
}

impl<'a, T> IntoIterator for &'a View<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.slice().iter()
    }
}

/// The sixteen dump sections as views; `MemIndex` derefs to this, so query
/// code reads `self.entries` etc. exactly as if they were owned slices.
/// (`pub` only because `Deref` exposes it in the public interface.)
#[derive(Clone, Copy)]
pub struct Sections {
    entries: View<Entry>,
    paths: View<u8>,
    names: View<u8>,
    revs: View<u8>,
    by_path: View<u32>,
    by_name: View<u32>,
    by_rev: View<u32>,
    by_size: View<u32>,
    by_mtime: View<u32>,
    by_ctime: View<u32>,
    dir_ids: View<u32>,
    file_ids: View<u32>,
    hidden_ids: View<u32>,
    system_ids: View<u32>,
    readonly_ids: View<u32>,
    reparse_ids: View<u32>,
}

impl std::ops::Deref for MemIndex {
    type Target = Sections;
    fn deref(&self) -> &Sections {
        &self.sec
    }
}

impl MemIndex {
    #[cfg(feature = "sqlite")]
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT id, path, name_l, size, mtime, ctime, flags, is_dir, frn FROM files ORDER BY id",
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
            let rev = lower_rev(&name_l);
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
                frn: row.get::<_, Option<i64>>(8)?.map(|f| f as u64).unwrap_or(0),
            });
        }
        Ok(finalize(entries, paths, names, revs))
    }

    /// Serialize the finished index (packed entries + arenas + permutations)
    /// as one contiguous dump. Written to a temp file then renamed, so a crash
    /// either leaves the previous dump or no dump at all.
    pub fn save(&self, path: &Path) -> Result<()> {
        let n = self.entries.len();
        anyhow::ensure!(n <= u32::MAX as usize, "too many entries for the dump format");
        let mut tmp = std::ffi::OsString::from(path.as_os_str());
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let file = File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 21, file);
        let mut offs = [0u64; SEC + 1];
        w.write_all(&[0u8; HDR_LEN])?; // header placeholder, rewritten below
        let mut off = HDR_LEN as u64;
        let secs: [&[u8]; SEC] = [
            pod_bytes(&self.entries),
            &self.paths,
            &self.names,
            &self.revs,
            pod_bytes(&self.by_path),
            pod_bytes(&self.by_name),
            pod_bytes(&self.by_rev),
            pod_bytes(&self.by_size),
            pod_bytes(&self.by_mtime),
            pod_bytes(&self.by_ctime),
            pod_bytes(&self.dir_ids),
            pod_bytes(&self.file_ids),
            pod_bytes(&self.hidden_ids),
            pod_bytes(&self.system_ids),
            pod_bytes(&self.readonly_ids),
            pod_bytes(&self.reparse_ids),
        ];
        for (i, bytes) in secs.into_iter().enumerate() {
            offs[i] = off;
            w.write_all(bytes)?;
            off += bytes.len() as u64;
            // pad to 8-byte alignment so the dump can be mmap-viewed in place
            let pad = (8 - (off as usize) % 8) % 8;
            w.write_all(&[0u8; 8][..pad])?;
            off += pad as u64;
        }
        offs[SEC] = off;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut hdr = Vec::with_capacity(HDR_LEN);
        hdr.extend_from_slice(MAGIC);
        hdr.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        hdr.extend_from_slice(&(n as u32).to_le_bytes());
        hdr.extend_from_slice(&now.to_le_bytes()); // created (informational)
        hdr.extend_from_slice(&0i64.to_le_bytes()); // reserved
        for o in offs {
            hdr.extend_from_slice(&o.to_le_bytes());
        }
        for c in [
            self.dir_ids.len(),
            self.file_ids.len(),
            self.hidden_ids.len(),
            self.system_ids.len(),
            self.readonly_ids.len(),
            self.reparse_ids.len(),
        ] {
            hdr.extend_from_slice(&(c as u32).to_le_bytes());
        }
        let mut file = w.into_inner()?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&hdr)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load a dump written by [`MemIndex::save`] — zero-copy: the file is
    /// memory-mapped and every section becomes a view over the mapping, so
    /// queries page in only the pages they touch (a ~1 GB index loads in
    /// ~1 ms). The mapping is kept alive by the returned index and unmapped
    /// when it is dropped.
    pub fn load_dump(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        let buf: &[u8] = &mmap;
        if buf.len() < HDR_LEN || buf[0..8] != *MAGIC {
            bail!("not a FERIDX dump: {}", path.display());
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            bail!(
                "dump format v{version} unsupported (want v{FORMAT_VERSION}) — re-run `fer index`"
            );
        }
        let n = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let mut offs = [0u64; SEC + 1];
        for (i, o) in offs.iter_mut().enumerate() {
            *o = u64::from_le_bytes(buf[32 + i * 8..40 + i * 8].try_into().unwrap());
        }
        let mut counts = [0usize; 6];
        for (i, c) in counts.iter_mut().enumerate() {
            *c = u32::from_le_bytes(
                buf[32 + (SEC + 1) * 8 + i * 4..32 + (SEC + 1) * 8 + (i + 1) * 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
        }
        anyhow::ensure!(
            offs[0] == HDR_LEN as u64
                && offs[SEC] == len
                && offs.windows(2).all(|w| w[0] <= w[1])
                && offs.iter().all(|o| o % 8 == 0)
                && (offs[1] - offs[0]) as usize == n * std::mem::size_of::<Entry>()
                && counts[0] + counts[1] == n,
            "dump layout corrupt: {}",
            path.display()
        );
        let view = |i: usize| -> &[u8] { &buf[offs[i] as usize..offs[i + 1] as usize] };
        let view_at = |start: u64, len: usize| -> &[u8] {
            &buf[start as usize..start as usize + len]
        };
        // SAFETY (view_of): each section starts 8-byte aligned; Entry/u32/u8
        // have no invalid bit patterns.
        fn view_of<T>(b: &[u8]) -> View<T> {
            View { ptr: b.as_ptr().cast::<T>(), len: b.len() / std::mem::size_of::<T>() }
        }
        let entry_bytes = n * std::mem::size_of::<Entry>();
        let perm_bytes = n * 4;
        let sec = Sections {
            entries: view_of(view_at(offs[0], entry_bytes)),
            paths: view_of(view(1)),
            names: view_of(view(2)),
            revs: view_of(view(3)),
            by_path: view_of(view_at(offs[4], perm_bytes)),
            by_name: view_of(view_at(offs[5], perm_bytes)),
            by_rev: view_of(view_at(offs[6], perm_bytes)),
            by_size: view_of(view_at(offs[7], perm_bytes)),
            by_mtime: view_of(view_at(offs[8], perm_bytes)),
            by_ctime: view_of(view_at(offs[9], perm_bytes)),
            dir_ids: view_of(view_at(offs[10], counts[0] * 4)),
            file_ids: view_of(view_at(offs[11], counts[1] * 4)),
            hidden_ids: view_of(view_at(offs[12], counts[2] * 4)),
            system_ids: view_of(view_at(offs[13], counts[3] * 4)),
            readonly_ids: view_of(view_at(offs[14], counts[4] * 4)),
            reparse_ids: view_of(view_at(offs[15], counts[5] * 4)),
        };
        Ok(MemIndex { _keep: Keep::Mapped(mmap), sec })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of directory entries (from the precomputed id lists, no scan).
    pub fn dir_count(&self) -> usize {
        self.dir_ids.len()
    }

    /// Number of file entries (from the precomputed id lists, no scan).
    pub fn file_count(&self) -> usize {
        self.file_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Path of entry `i` reconstructed from the arena (display casing).
    pub fn path_at(&self, i: usize) -> String {
        let e = &self.entries[i];
        String::from_utf8_lossy(
            &self.paths[e.path_off as usize..(e.path_off as usize + e.path_len as usize)],
        )
        .into_owned()
    }

    /// Metadata of entry `i` (monitor / dupes / flush use).
    pub fn meta_at(&self, i: usize) -> EntryMeta {
        let e = &self.entries[i];
        EntryMeta {
            is_dir: e.is_dir != 0,
            size: e.size,
            mtime: e.mtime,
            ctime: e.ctime,
            flags: e.flags,
            frn: (e.frn != 0).then_some(e.frn),
        }
    }

    /// FRN → entry-index map for the monitor's delete-by-FRN fast path.
    pub fn frn_map(&self) -> HashMap<u64, u32> {
        let mut m = HashMap::with_capacity(self.entries.len());
        for (i, e) in self.entries.iter().enumerate() {
            if e.frn != 0 {
                m.insert(e.frn, i as u32);
            }
        }
        m
    }

    /// Exact (ASCII-CI) path lookup — the monitor dedupes create/rename events
    /// against existing entries via the CI-sorted path permutation.
    pub fn find_path_idx(&self, path: &str) -> Option<usize> {
        let needle = path.as_bytes();
        let lo = self.by_path.partition_point(|&i| {
            ci_cmp(path_of(&self.entries, &self.paths, i), needle) == Ordering::Less
        });
        let hi = self.by_path.partition_point(|&i| {
            let p = path_of(&self.entries, &self.paths, i);
            ci_cmp(p, needle) == Ordering::Less || ci_starts_with(p, needle)
        });
        self.by_path[lo..hi]
            .iter()
            .find(|&&i| ci_cmp(path_of(&self.entries, &self.paths, i), needle) == Ordering::Equal)
            .map(|&i| i as usize)
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

    /// Evaluate the whole query in memory; returns matching file ids ascending.
    pub fn search(&self, q: &Query) -> Vec<u32> {
        let mut acc: Option<IdSet<'_>> = None;
        for t in &q.include {
            let ids = self.eval(t);
            acc = Some(match acc {
                None => ids,
                Some(a) => IdSet::Owned(intersect(a.as_slice(), ids.as_slice())),
            });
            if acc.as_ref().is_some_and(|s| s.as_slice().is_empty()) {
                return Vec::new();
            }
        }
        let mut acc = acc.unwrap_or_else(|| IdSet::Owned(self.all_ids()));
        for t in &q.exclude {
            let ids = self.eval(t);
            acc = IdSet::Owned(subtract(acc.as_slice(), ids.as_slice()));
            if acc.as_slice().is_empty() {
                break;
            }
        }
        acc.into_owned()
    }

    /// Build full hits for ids (order preserved), capped at `limit`.
    pub fn hits(&self, ids: &[u32], limit: usize) -> Vec<Hit> {
        let mut out = Vec::with_capacity(ids.len().min(limit));
        for &id in ids.iter().take(limit) {
            // Binary search rather than id-as-index: dump/mem ids are 0..n
            // sequential, but the SQL-loaded oracle index carries SQLite
            // rowids (1-based, possibly with gaps).
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

    fn eval(&self, t: &Term) -> IdSet<'_> {
        match t {
            Term::Name(s) => IdSet::Owned(self.scan_names(s.as_bytes())),
            Term::PathSubstr(s) => IdSet::Owned(self.scan_paths_ci(s.as_bytes())),
            Term::Suffix(s) => {
                let rev: String = s.chars().rev().collect();
                IdSet::Owned(self.range_by_rev(rev.as_bytes()))
            }
            Term::Ext(list) => {
                let mut out: Vec<u32> = Vec::new();
                for e in list {
                    let rev: String = format!(".{e}").chars().rev().collect();
                    out = union(&out, &self.range_by_rev(rev.as_bytes()));
                }
                IdSet::Owned(out)
            }
            Term::NameWild(p) => IdSet::Owned(self.scan_glob(&self.names, p, true)),
            Term::PathWild(p) => IdSet::Owned(self.scan_glob(&self.paths, p, false)),
            Term::Size { min, max } => IdSet::Owned(self.range_u64(
                &self.by_size,
                |e| e.size,
                min.unwrap_or(0),
                max.unwrap_or(u64::MAX),
            )),
            Term::Mtime { min, max } => IdSet::Owned(self.range_i64(
                &self.by_mtime,
                |e| e.mtime,
                min.unwrap_or(i64::MIN),
                max.unwrap_or(i64::MAX),
            )),
            Term::Ctime { min, max } => IdSet::Owned(self.range_i64(
                &self.by_ctime,
                |e| e.ctime,
                min.unwrap_or(i64::MIN),
                max.unwrap_or(i64::MAX),
            )),
            Term::IsDir(b) => {
                if *b {
                    IdSet::Borrowed(self.dir_ids.slice())
                } else {
                    IdSet::Borrowed(self.file_ids.slice())
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
                    IdSet::Borrowed(list.slice())
                } else {
                    IdSet::Owned(self.all_minus(list))
                }
            }
            Term::PathPrefix(p) => IdSet::Owned(self.range_by_path_prefix(p.as_bytes())),
        }
    }

    /// All ids minus `list` (both sorted) via one two-pointer sweep — avoids
    /// materializing the full id array for `!flag:` terms.
    fn all_minus(&self, list: &[u32]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut j = 0;
        for e in &self.entries {
            while j < list.len() && list[j] < e.id {
                j += 1;
            }
            if j < list.len() && list[j] == e.id {
                continue;
            }
            out.push(e.id);
        }
        out
    }

    /// Substring scan over the (lowercased) name arena; ids come out ascending
    /// because we walk in entry order.
    fn scan_names(&self, needle: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        if needle.is_empty() {
            return self.all_ids();
        }
        for e in &self.entries {
            let slice =
                &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            if memchr::memmem::find(slice, needle).is_some() {
                out.push(e.id);
            }
        }
        out
    }

    /// ASCII-case-insensitive substring scan over the (original-case) path
    /// arena: memchr locates candidates by the folded first byte (both case
    /// variants for ASCII letters), then the tail is verified folded.
    fn scan_paths_ci(&self, needle: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        if needle.is_empty() {
            return self.all_ids();
        }
        let f = fold(needle[0]);
        let alt = if f.is_ascii_lowercase() {
            Some(f - 32)
        } else if f.is_ascii_uppercase() {
            Some(f + 32)
        } else {
            None
        };
        for e in &self.entries {
            let path = &self.paths[e.path_off as usize..e.path_off as usize + e.path_len as usize];
            if path.len() < needle.len() {
                continue;
            }
            let limit = path.len() - needle.len();
            let scan = &path[..=limit];
            let mut found = false;
            for pos in memchr::memchr_iter(f, scan) {
                if ci_eq_at(path, pos, needle) {
                    found = true;
                    break;
                }
            }
            if !found
                && let Some(a) = alt
            {
                for pos in memchr::memchr_iter(a, scan) {
                    if ci_eq_at(path, pos, needle) {
                        found = true;
                        break;
                    }
                }
            }
            if found {
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

/// Build the query permutations (sorted id arrays) from the packed entries.
/// The six sorts and the attribute filter pass are independent — they run on
/// scoped threads, so finalizing a 4M-entry index takes a few hundred
/// milliseconds instead of a serial second-plus.
fn finalize(entries: Vec<Entry>, paths: Vec<u8>, names: Vec<u8>, revs: Vec<u8>) -> MemIndex {
    let n = entries.len();
    let seq = (0..n as u32).collect::<Vec<u32>>();
    let mut by_path = seq.clone();
    let mut by_name = seq.clone();
    let mut by_rev = seq.clone();
    let mut by_size = seq;
    let mut by_mtime = (0..n as u32).collect::<Vec<u32>>();
    let mut by_ctime = (0..n as u32).collect::<Vec<u32>>();
    let mut dir_ids: Vec<u32> = Vec::new();
    let mut file_ids: Vec<u32> = Vec::new();
    let mut hidden_ids: Vec<u32> = Vec::new();
    let mut system_ids: Vec<u32> = Vec::new();
    let mut readonly_ids: Vec<u32> = Vec::new();
    let mut reparse_ids: Vec<u32> = Vec::new();
    let e: &[Entry] = &entries;
    std::thread::scope(|s| {
        s.spawn(|| {
            by_path.sort_unstable_by(|&a, &b| {
                ci_cmp(path_of(e, &paths, a), path_of(e, &paths, b))
            });
        });
        s.spawn(|| {
            by_name.sort_unstable_by(|&a, &b| {
                name_of(e, &names, a).cmp(name_of(e, &names, b))
            });
        });
        s.spawn(|| {
            by_rev.sort_unstable_by(|&a, &b| {
                rev_of(e, &revs, a).cmp(rev_of(e, &revs, b))
            });
        });
        s.spawn(|| by_size.sort_unstable_by_key(|&i| e[i as usize].size));
        s.spawn(|| {
            by_mtime.sort_unstable_by_key(|&i| e[i as usize].mtime);
            by_ctime.sort_unstable_by_key(|&i| e[i as usize].ctime);
        });
        s.spawn(|| {
            for t in e {
                let id = t.id;
                if t.is_dir != 0 {
                    dir_ids.push(id);
                } else {
                    file_ids.push(id);
                }
                if t.flags & 1 != 0 {
                    hidden_ids.push(id);
                }
                if t.flags & 2 != 0 {
                    system_ids.push(id);
                }
                if t.flags & 4 != 0 {
                    readonly_ids.push(id);
                }
                if t.flags & 8 != 0 {
                    reparse_ids.push(id);
                }
            }
        });
    });
    MemIndex::from_owned(OwnedData {
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

impl MemIndex {
    /// Publish owned sections as views. The Vec heap buffers are stable across
    /// moves, so the captured pointers stay valid after this — provided the
    /// Vecs are never mutated again (MemIndex is read-only by contract).
    fn from_owned(o: OwnedData) -> MemIndex {
        let sec = Sections {
            entries: View::from_slice(&o.entries),
            paths: View::from_slice(&o.paths),
            names: View::from_slice(&o.names),
            revs: View::from_slice(&o.revs),
            by_path: View::from_slice(&o.by_path),
            by_name: View::from_slice(&o.by_name),
            by_rev: View::from_slice(&o.by_rev),
            by_size: View::from_slice(&o.by_size),
            by_mtime: View::from_slice(&o.by_mtime),
            by_ctime: View::from_slice(&o.by_ctime),
            dir_ids: View::from_slice(&o.dir_ids),
            file_ids: View::from_slice(&o.file_ids),
            hidden_ids: View::from_slice(&o.hidden_ids),
            system_ids: View::from_slice(&o.system_ids),
            readonly_ids: View::from_slice(&o.readonly_ids),
            reparse_ids: View::from_slice(&o.reparse_ids),
        };
        MemIndex { _keep: Keep::Owned(o), sec }
    }
}

/// Byte view of a POD slice — Entry/u32/u8 are plain integers with no invalid
/// bit patterns and no padding hazards, so raw sections are dump-stable.
fn pod_bytes<T>(v: &[T]) -> &[u8] {
    // SAFETY: any byte pattern is a valid value of these integer types; the
    // length is scaled by size_of::<T>.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), std::mem::size_of_val(v)) }
}

/// Dump file path companion to the SQLite db ("index.db" → "index.db.feridx").
pub fn dump_path(db: &Path) -> PathBuf {
    let mut p = std::ffi::OsString::from(db.as_os_str());
    p.push(".feridx");
    PathBuf::from(p)
}

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

/// Folded comparison of `hay[pos..pos+len]` against the (lowercased) needle.
#[inline]
fn ci_eq_at(hay: &[u8], pos: usize, needle: &[u8]) -> bool {
    hay[pos..pos + needle.len()]
        .iter()
        .zip(needle)
        .all(|(h, n)| fold(*h) == *n)
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

/// Term-evaluation result: borrowed when the term maps directly onto a
/// precomputed id list (no copy), owned when computed. Multi-term queries
/// then pay one intersect allocation instead of a full-list copy per term.
enum IdSet<'a> {
    Borrowed(&'a [u32]),
    Owned(Vec<u32>),
}

impl IdSet<'_> {
    fn as_slice(&self) -> &[u32] {
        match self {
            IdSet::Borrowed(s) => s,
            IdSet::Owned(v) => v,
        }
    }
    fn into_owned(self) -> Vec<u32> {
        match self {
            IdSet::Borrowed(s) => s.to_vec(),
            IdSet::Owned(v) => v,
        }
    }
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

    fn rows() -> Vec<(&'static str, EntryMeta)> {
        let now = 1_700_000_000i64;
        vec![
            (r"D:\docs\年度报告.md", EntryMeta { size: 100, mtime: now, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\docs\readme.txt", EntryMeta { size: 500, mtime: now - 10_000_000, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\proj\src\main.rs", EntryMeta { size: 2 << 20, mtime: now, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\proj\src\lib.rs", EntryMeta { size: 3 << 20, mtime: now - 100, ctime: now, flags: EntryMeta::FLAG_HIDDEN, ..Default::default() }),
            (r"D:\media\rs.jpg", EntryMeta { size: 9 << 20, mtime: now, ctime: now, flags: 0, ..Default::default() }),
            (r"D:\media\sub", EntryMeta { is_dir: true, size: 0, mtime: now, ctime: now, flags: 0, ..Default::default() }),
        ]
    }

    fn build_mem() -> MemIndex {
        let mut b = MemBuilder::default();
        for (path, meta) in rows() {
            b.push(path, meta);
        }
        b.finish()
    }

    #[cfg(feature = "sqlite")]
    fn build_sql() -> (tempfile::TempDir, crate::store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("t.db")).unwrap();
        let mut rb = store.begin_rebuild().unwrap();
        for (path, meta) in rows() {
            rb.insert(path, meta).unwrap();
        }
        rb.commit().unwrap();
        (dir, store)
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn consistency_with_sql() {
        let (_d, store) = build_sql();
        let mem = MemIndex::load(store.conn()).unwrap();
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
            r"D:\proj\src", // bare path token → CI path-substring scan
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
        let mem = build_mem();
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
        let mem = build_mem();
        let q = Query::parse("").unwrap();
        assert_eq!(mem.search(&q).len(), 6);
    }
}
