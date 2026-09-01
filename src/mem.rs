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
//!   `by_mtime`, `by_ctime`, `by_frn` (FRN order — the monitor's delete
//!   lookup) — all queries reduce to two binary searches
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
/// loading); v1 had unaligned sections and is rejected. v3 adds the `by_frn`
/// permutation section (FRN → entry binary search for the monitor). v4 adds
/// the `name_offs` / `path_offs` accelerator arrays (arena-offset → entry
/// mapping, enabling whole-arena SIMD scans). v5 adds the trigram index
/// (`trigrams` / `trig_offs` / `trig_posts`) for O(posting) substring
/// candidate generation. Older dumps stay loadable: v3 rebuilds the two
/// accelerator arrays in memory, v3/v4 simply fall back to arena scans.
const MAGIC: &[u8; 8] = b"FERIDX01";
const FORMAT_VERSION: u32 = 5;

/// Fixed dump section order: entries, paths, names, revs, the six sorted
/// permutations, the six id lists, `by_frn`, `name_offs`, `path_offs`, then
/// the trigram index (`trigrams` sorted-unique u32, `trig_offs` cumulative
/// u32 posting offsets, `trig_posts` ascending entry indices). The header
/// stores byte offsets for each section plus the total file length (SEC+1
/// table entries), followed by the six id-list element counts — logical
/// lengths come from these, so inter-section alignment padding never leaks
/// into the data.
const SEC: usize = 22;
const HDR_LEN: usize = 32 + (SEC + 1) * 8 + 6 * 4; // 240
/// v3 layout constants (no accelerator/trigram sections) for
/// backward-compatible loading: existing dumps keep working without an
/// immediate rebuild.
const SEC_V3: usize = 17;
const HDR_LEN_V3: usize = 32 + (SEC_V3 + 1) * 8 + 6 * 4; // 200
/// v4 layout (accelerator arrays, no trigram index): loads with mapped
/// accelerators and arena-scan fallback for substrings.
const SEC_V4: usize = 19;
const HDR_LEN_V4: usize = 32 + (SEC_V4 + 1) * 8 + 6 * 4; // 216

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

    /// Append an entry whose arena strings are already materialized (the
    /// monitor's flush fast path): no basename extraction, no case
    /// folding/reversal, no String allocations — bytes are copied straight
    /// out of the old index's arenas.
    pub(crate) fn push_arena(&mut self, path: &[u8], name_l: &[u8], rev: &[u8], meta: EntryMeta) {
        let id = self.entries.len() as u32;
        let po = self.paths.len() as u32;
        self.paths.extend_from_slice(path);
        let no = self.names.len() as u32;
        self.names.extend_from_slice(name_l);
        let ro = self.revs.len() as u32;
        self.revs.extend_from_slice(rev);
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
}

pub struct MemIndex {
    _keep: Keep,
    sec: Sections,
}

/// What keeps the section memory alive. `Owned` holds the Vecs produced by
/// finalize (their heap buffers are stable across moves, so the raw pointers
/// in `Sections` stay valid); `Mapped` holds the mmap of a dump file plus, for
/// v3 dumps, the accelerator arrays rebuilt in memory (their heap buffers are
/// likewise stable). (Payloads are ownership anchors only — the live data is
/// reached through the `Sections` views — hence the dead_code allow.)
#[allow(dead_code, clippy::large_enum_variant)]
enum Keep {
    Owned(OwnedData),
    Mapped(MappedData),
}

/// v3 compatibility payload: the two accelerator arrays derived from the
/// id-ordered entries section (arena offsets are monotone there), plus —
/// after `fer upgrade` — the trigram index rebuilt in memory.
#[derive(Default)]
struct AuxAccel {
    name_offs: Vec<u32>,
    path_offs: Vec<u32>,
    trig: Option<(Vec<u32>, Vec<u32>, Vec<u32>)>,
}

/// Ownership anchor for dump-backed indexes: the mmap keeps every mapped
/// section alive, and `aux` (v3 dumps only) keeps the rebuilt accelerator
/// arrays alive. Neither field is read directly — the live data is reached
/// through the `Sections` views.
#[allow(dead_code)]
struct MappedData {
    mmap: memmap2::Mmap,
    aux: Option<AuxAccel>,
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
    by_frn: Vec<u32>,
    dir_ids: Vec<u32>,
    file_ids: Vec<u32>,
    hidden_ids: Vec<u32>,
    system_ids: Vec<u32>,
    readonly_ids: Vec<u32>,
    reparse_ids: Vec<u32>,
    name_offs: Vec<u32>,
    path_offs: Vec<u32>,
    trigrams: Vec<u32>,
    trig_offs: Vec<u32>,
    trig_posts: Vec<u32>,
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
    by_frn: View<u32>,
    name_offs: View<u32>,
    path_offs: View<u32>,
    trigrams: View<u32>,
    trig_offs: View<u32>,
    trig_posts: View<u32>,
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
            pod_bytes(&self.by_frn),
            pod_bytes(&self.name_offs),
            pod_bytes(&self.path_offs),
            pod_bytes(&self.trigrams),
            pod_bytes(&self.trig_offs),
            pod_bytes(&self.trig_posts),
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
        if buf.len() < HDR_LEN_V3 || buf[0..8] != *MAGIC {
            bail!("not a FERIDX dump: {}", path.display());
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if version != FORMAT_VERSION && version != 4 && version != 3 {
            bail!(
                "dump format v{version} unsupported (want v{FORMAT_VERSION}, v4 or v3) — re-run `fer index`"
            );
        }
        let (sec, hdr_len) = match version {
            3 => (SEC_V3, HDR_LEN_V3),
            4 => (SEC_V4, HDR_LEN_V4),
            _ => (SEC, HDR_LEN),
        };
        let n = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let mut offs = [0u64; SEC + 1];
        for (i, o) in offs.iter_mut().enumerate().take(sec + 1) {
            *o = u64::from_le_bytes(buf[32 + i * 8..40 + i * 8].try_into().unwrap());
        }
        let mut counts = [0usize; 6];
        for (i, c) in counts.iter_mut().enumerate() {
            *c = u32::from_le_bytes(
                buf[32 + (sec + 1) * 8 + i * 4..32 + (sec + 1) * 8 + (i + 1) * 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
        }
        // Section sizes: entries are 56B (always 8-divisible, exact check);
        // u32 sections may carry up to 7 bytes of alignment padding when
        // n*4 isn't a multiple of 8, so use the padded bound.
        let padded = |bytes: u64, logical: usize| {
            bytes as usize >= logical && (bytes as usize - logical) < 8
        };
        anyhow::ensure!(
            offs[0] == hdr_len as u64
                && offs[sec] == len
                && offs[..sec].windows(2).all(|w| w[0] <= w[1])
                && offs.iter().take(sec).all(|o| o % 8 == 0)
                && (offs[1] - offs[0]) as usize == n * std::mem::size_of::<Entry>()
                && padded(offs[17] - offs[16], n * 4)
                && (version == 3 || padded(offs[18] - offs[17], n * 4))
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
        // v3 dumps predate the accelerator arrays: rebuild them in memory from
        // the id-ordered entries section (arena offsets are monotone there).
        let aux = if version == 3 {
            let entries_view: View<Entry> = view_of(view_at(offs[0], entry_bytes));
            let mut acc = AuxAccel {
                name_offs: Vec::with_capacity(n),
                path_offs: Vec::with_capacity(n),
                trig: None,
            };
            for e in entries_view.slice() {
                acc.name_offs.push(e.name_off);
                acc.path_offs.push(e.path_off);
            }
            eprintln!(
                "fer: loaded v3 dump (accelerator arrays rebuilt in memory) — run `fer upgrade` for the v5 trigram index, or `fer index` for a full refresh"
            );
            Some(acc)
        } else {
            None
        };
        let name_offs = match &aux {
            Some(a) => View::from_slice(&a.name_offs),
            None => view_of(view_at(offs[17], perm_bytes)),
        };
        let path_offs = match &aux {
            Some(a) => View::from_slice(&a.path_offs),
            None => view_of(view_at(offs[18], perm_bytes)),
        };
        // Trigram sections exist only in v5; older dumps fall back to the
        // whole-arena scan path (empty views).
        let (trigrams, trig_offs, trig_posts) = if version == 5 {
            (
                view_of(view_at(offs[19], (offs[20] - offs[19]) as usize)),
                view_of(view_at(offs[20], (offs[21] - offs[20]) as usize)),
                view_of(view_at(offs[21], (offs[22] - offs[21]) as usize)),
            )
        } else {
            (
                View::<u32>::from_slice(&[]),
                View::<u32>::from_slice(&[]),
                View::<u32>::from_slice(&[]),
            )
        };
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
            by_frn: view_of(view_at(offs[16], perm_bytes)),
            name_offs,
            path_offs,
            trigrams,
            trig_offs,
            trig_posts,
        };
        Ok(MemIndex { _keep: Keep::Mapped(MappedData { mmap, aux }), sec })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of distinct trigrams in the index (0 when absent — v3/v4 dumps).
    pub fn trigram_count(&self) -> usize {
        self.trigrams.len()
    }

    /// Rebuild the trigram index in memory when it is missing (v3/v4 dumps)
    /// and repoint the section views at the new arrays. The arrays live in
    /// the `Keep` anchor (OwnedData fields or the MappedData aux slot).
    /// Returns false when the index was already present. `fer upgrade` uses
    /// this for a format-only migration — no admin, no MFT re-read; a later
    /// `save()` then rewrites the dump as v5.
    pub fn build_missing_trigrams(&mut self) -> bool {
        if !self.trigrams.is_empty() {
            return false;
        }
        let (t, o, p) = build_trigrams(self.names.slice(), self.entries.slice());
        let (tv, ov, pv) = match &mut self._keep {
            Keep::Owned(owned) => {
                owned.trigrams = t;
                owned.trig_offs = o;
                owned.trig_posts = p;
                (
                    View::from_slice(&owned.trigrams),
                    View::from_slice(&owned.trig_offs),
                    View::from_slice(&owned.trig_posts),
                )
            }
            Keep::Mapped(mapped) => {
                let aux = mapped.aux.get_or_insert_with(AuxAccel::default);
                aux.trig = Some((t, o, p));
                let trig = aux.trig.as_ref().expect("just set");
                (
                    View::from_slice(&trig.0),
                    View::from_slice(&trig.1),
                    View::from_slice(&trig.2),
                )
            }
        };
        self.sec.trigrams = tv;
        self.sec.trig_offs = ov;
        self.sec.trig_posts = pv;
        true
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

    /// Arena bytes of entry `i`'s path / lowercased name / reversed name
    /// (zero-allocation views — the monitor's flush fast path copies these
    /// straight into the rebuilt index).
    pub fn path_bytes(&self, i: usize) -> &[u8] {
        let e = &self.entries[i];
        &self.paths[e.path_off as usize..e.path_off as usize + e.path_len as usize]
    }
    pub fn name_l_bytes(&self, i: usize) -> &[u8] {
        let e = &self.entries[i];
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }
    pub fn rev_bytes(&self, i: usize) -> &[u8] {
        let e = &self.entries[i];
        &self.revs[e.rev_off as usize..e.rev_off as usize + e.rev_len as usize]
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

    /// FRN → entry index via the `by_frn` permutation (two binary searches).
    /// Replaces the ~100 MB FRN HashMap the monitor used to materialize at
    /// startup and on every flush. `0` means "no FRN" and never matches.
    pub fn find_frn(&self, frn: u64) -> Option<u32> {
        if frn == 0 {
            return None;
        }
        let lo = self.by_frn.partition_point(|&i| self.entries[i as usize].frn < frn);
        self.by_frn
            .get(lo)
            .copied()
            .filter(|&i| self.entries[i as usize].frn == frn)
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
                + self.by_ctime.len()
                + self.by_frn.len()
                + self.name_offs.len()
                + self.path_offs.len())
                * 4
            + (self.dir_ids.len()
                + self.file_ids.len()
                + self.hidden_ids.len()
                + self.system_ids.len()
                + self.readonly_ids.len()
                + self.reparse_ids.len())
                * 4
            + (self.trigrams.len() + self.trig_offs.len() + self.trig_posts.len()) * 4
    }

    /// Evaluate the whole query in memory; returns matching file ids ascending.
    /// Independent terms evaluate on scoped threads (multi-scan queries then
    /// cost one scan, not the sum); intersections stay sequential.
    pub fn search(&self, q: &Query) -> Vec<u32> {
        let evals = |terms: &[Term]| -> Vec<IdSet<'_>> {
            if terms.len() > 1 {
                std::thread::scope(|s| {
                    let handles: Vec<_> =
                        terms.iter().map(|t| s.spawn(|| self.eval(t))).collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("query eval thread panicked"))
                        .collect()
                })
            } else {
                terms.iter().map(|t| self.eval(t)).collect()
            }
        };
        let mut acc: Option<IdSet<'_>> = None;
        for ids in evals(&q.include) {
            acc = Some(match acc {
                None => ids,
                Some(a) => IdSet::Owned(intersect(a.as_slice(), ids.as_slice())),
            });
            if acc.as_ref().is_some_and(|s| s.as_slice().is_empty()) {
                return Vec::new();
            }
        }
        let mut acc = acc.unwrap_or_else(|| IdSet::Owned(self.all_ids()));
        for ids in evals(&q.exclude) {
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
            Term::Regex(pat) => IdSet::Owned(self.scan_regex(pat)),
        }
    }

    /// Regex scan over the (lowercased) name arena. The pattern was validated
    /// and lowercased at parse time, so this compile cannot fail. When the
    /// pattern yields exact literals, they prefilter the whole-arena SIMD
    /// scan (every match must contain at least one extracted literal);
    /// candidates are then verified with the full regex on the entry's own
    /// bytes. Patterns with no usable literal keep the per-entry scan.
    fn scan_regex(&self, pattern: &str) -> Vec<u32> {
        let re = regex::bytes::Regex::new(pattern).expect("regex pattern validated at parse");
        let mut out = Vec::new();
        if let Some(seeds) = regex_literal_seeds(pattern) {
            let mut cands: Vec<u32> = Vec::new();
            for seed in &seeds {
                cands = union(&cands, &self.scan_name_hits(seed));
            }
            for idx in cands {
                let e = &self.entries[idx as usize];
                let name =
                    &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
                if re.is_match(name) {
                    out.push(e.id);
                }
            }
            return out;
        }
        for e in &self.entries {
            let name =
                &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            if re.is_match(name) {
                out.push(e.id);
            }
        }
        out
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

    /// Substring scan over the (lowercased) name arena, returning ids ascending.
    /// See [`Self::scan_name_hits`] for the scanning strategy.
    fn scan_names(&self, needle: &[u8]) -> Vec<u32> {
        if needle.is_empty() {
            return self.all_ids();
        }
        self.scan_name_hits(needle)
            .iter()
            .map(|&i| self.entries[i as usize].id)
            .collect()
    }

    /// Whole-arena substring scan returning matching entry INDICES (ascending:
    /// the arena is laid out in entry order and the scan cursor only ever
    /// advances). This is the hot path for substring and regex-prefilter
    /// queries — ONE SIMD pass over the contiguous names arena instead of one
    /// tiny scan per entry (4M short-haystack scans waste the SIMD setup).
    ///
    /// Correctness notes:
    /// * a hit straddling an entry boundary is an arena concatenation
    ///   artifact and is rejected, advancing just one byte so a real
    ///   overlapping match is never shadowed (non-overlapping iterators
    ///   would silently drop it);
    /// * after a real hit the scan jumps to the entry's end (contains
    ///   semantics — at most one hit per entry);
    /// * the entry cursor advances monotonically because `from` only grows.
    fn scan_name_hits(&self, needle: &[u8]) -> Vec<u32> {
        // Trigram fast path: ≥3-byte needles intersect posting lists down to a
        // tiny candidate set (O(posting) instead of O(arena)); each candidate
        // is still verified with a real memmem on its own name, so the trigram
        // index is only a sound superset filter. Absent index (v3/v4 dumps) →
        // whole-arena scan.
        if needle.len() >= 3
            && let Some(cands) = self.trigram_candidates(needle)
        {
            let mut out = Vec::with_capacity(cands.len());
            let names = self.names.slice();
            for idx in cands {
                let e = &self.entries[idx as usize];
                let name =
                    &names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
                if memchr::memmem::find(name, needle).is_some() {
                    out.push(idx);
                }
            }
            return out;
        }
        let mut out = Vec::new();
        let arena = self.names.slice();
        let n = self.entries.len();
        if arena.is_empty() || n == 0 {
            return out;
        }
        let offs = self.name_offs.slice();
        let mut from = 0usize;
        let mut e_idx = 0usize;
        loop {
            if from >= arena.len() {
                break;
            }
            let rel = if needle.len() == 1 {
                memchr::memchr(needle[0], &arena[from..])
            } else {
                memchr::memmem::find(&arena[from..], needle)
            };
            let Some(rel) = rel else { break };
            let abs = from + rel;
            while e_idx + 1 < n && offs[e_idx + 1] as usize <= abs {
                e_idx += 1;
            }
            let e = &self.entries[e_idx];
            let end = e.name_off as usize + e.name_len as usize;
            if abs + needle.len() <= end {
                // fully inside this entry: real match; record and skip the
                // rest of the entry (boolean contains semantics)
                out.push(e_idx as u32);
                from = end;
            } else {
                // straddles a concatenation boundary: artifact
                from = abs + 1;
            }
        }
        out
    }

    /// Trigram prefilter: intersect the posting lists of the needle's distinct
    /// trigrams, starting from the shortest list; returns candidate entry
    /// indices ascending, or None when the trigram index is absent (v3/v4
    /// dumps load with empty sections and fall back to the arena scan).
    /// Soundness: a name containing the needle contains every distinct trigram
    /// of the needle, so the intersection is a superset of the matches.
    fn trigram_candidates(&self, needle: &[u8]) -> Option<Vec<u32>> {
        if self.trigrams.is_empty() {
            return None;
        }
        let mut ts: Vec<u32> = Vec::new();
        for w in needle.windows(3) {
            let t = ((w[0] as u32) << 16) | ((w[1] as u32) << 8) | w[2] as u32;
            if ts.last() != Some(&t) {
                ts.push(t);
            }
        }
        let posts_of = |t: u32| -> &[u32] {
            let pos = self.trigrams.partition_point(|&x| x < t);
            if pos < self.trigrams.len() && self.trigrams[pos] == t {
                let s = self.trig_offs[pos] as usize;
                let e = self.trig_offs[pos + 1] as usize;
                &self.trig_posts.slice()[s..e]
            } else {
                &[]
            }
        };
        // shortest posting first (cheapest intersection)
        ts.sort_unstable_by_key(|&t| posts_of(t).len());
        let mut cands: Vec<u32> = posts_of(ts[0]).to_vec();
        for t in &ts[1..] {
            cands = intersect(&cands, posts_of(*t));
            if cands.is_empty() {
                break;
            }
        }
        Some(cands)
    }

    /// ASCII-case-insensitive substring scan over the (original-case) path
    /// arena, returning ids ascending. See [`Self::scan_paths_hits`].
    fn scan_paths_ci(&self, needle: &[u8]) -> Vec<u32> {
        if needle.is_empty() {
            return self.all_ids();
        }
        self.scan_paths_hits(needle)
            .iter()
            .map(|&i| self.entries[i as usize].id)
            .collect()
    }

    /// Whole-arena CI seed scan over the path arena returning entry INDICES:
    /// one SIMD pass via `memchr2` over both case variants of the folded
    /// first byte (plain `memchr` for non-letters), folded-tail verification
    /// per candidate, and the same boundary mapping plus entry-end jumps as
    /// `scan_name_hits`. Shared by the path-substring term and the glob
    /// prefilter.
    fn scan_paths_hits(&self, needle: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let arena = self.paths.slice();
        let n = self.entries.len();
        if arena.is_empty() || n == 0 {
            return out;
        }
        let offs = self.path_offs.slice();
        let f = fold(needle[0]);
        let alt = if f.is_ascii_lowercase() {
            Some(f - 32)
        } else if f.is_ascii_uppercase() {
            Some(f + 32)
        } else {
            None
        };
        let mut from = 0usize;
        let mut e_idx = 0usize;
        loop {
            if from >= arena.len() {
                break;
            }
            let rel = match alt {
                Some(a) => memchr::memchr2(f, a, &arena[from..]),
                None => memchr::memchr(f, &arena[from..]),
            };
            let Some(rel) = rel else { break };
            let abs = from + rel;
            while e_idx + 1 < n && offs[e_idx + 1] as usize <= abs {
                e_idx += 1;
            }
            let e = &self.entries[e_idx];
            let end = e.path_off as usize + e.path_len as usize;
            if abs + needle.len() <= end && ci_eq_at(arena, abs, needle) {
                out.push(e_idx as u32);
                from = end;
            } else {
                from = abs + 1;
            }
        }
        out
    }

    /// Whole-arena seed scan (entry indices) over the name or path arena,
    /// depending on `use_names`; names are pre-lowercased (plain memmem),
    /// paths keep original case (folded-first-byte memchr2 + tail check).
    fn scan_arena_hits(&self, seed: &[u8], use_names: bool) -> Vec<u32> {
        if use_names {
            self.scan_name_hits(seed)
        } else {
            self.scan_paths_hits(seed)
        }
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

    /// Glob scan. Three layers, in order:
    /// 1. pure-star pattern → everything;
    /// 2. leading literal prefix ≥ 2 bytes → byte-sorted name permutation
    ///    narrows the candidate range (unchanged fast path);
    /// 3. otherwise a whole-arena SIMD prefilter on the longest literal run
    ///    of the pattern (every glob literal must appear in any match, so any
    ///    run is a sound superset filter), then the compiled bit-parallel
    ///    matcher verifies each candidate (zero allocation).
    fn scan_glob(&self, arena: &[u8], pattern: &str, use_names: bool) -> Vec<u32> {
        let tokens = glob_tokens(pattern);
        if tokens.iter().all(|t| *t == GTok::Star) {
            return self.all_ids();
        }
        let prog = GlobProg::compile(&tokens);
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
                if prog.matches(name) {
                    out.push(e.id);
                }
            }
            out.sort_unstable();
            return out;
        }
        let seed = longest_literal_run(&tokens);
        let cands: Vec<u32> = if seed.is_empty() {
            (0..self.entries.len() as u32).collect()
        } else {
            self.scan_arena_hits(&seed, use_names)
        };
        for idx in cands {
            let e = &self.entries[idx as usize];
            let slice = if use_names {
                &arena[e.name_off as usize..e.name_off as usize + e.name_len as usize]
            } else {
                &arena[e.path_off as usize..e.path_off as usize + e.path_len as usize]
            };
            if prog.matches(slice) {
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
    let mut by_frn = (0..n as u32).collect::<Vec<u32>>();
    let mut dir_ids: Vec<u32> = Vec::new();
    let mut file_ids: Vec<u32> = Vec::new();
    let mut hidden_ids: Vec<u32> = Vec::new();
    let mut system_ids: Vec<u32> = Vec::new();
    let mut readonly_ids: Vec<u32> = Vec::new();
    let mut reparse_ids: Vec<u32> = Vec::new();
    let e: &[Entry] = &entries;
    // The trigram build is independent of the sorts/attribute pass — run it in
    // the same scope so finalize stays one parallel phase.
    let mut trig: Option<(Vec<u32>, Vec<u32>, Vec<u32>)> = None;
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
            // id tiebreak keeps the sort deterministic (dump bytes stable).
            by_frn.sort_unstable_by_key(|&i| (e[i as usize].frn, e[i as usize].id));
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
        s.spawn(|| {
            trig = Some(build_trigrams(&names, e));
        });
    });
    let (trigrams, trig_offs, trig_posts) = trig.expect("trigram build thread");
    // Accelerator arrays: arena offsets in id order (monotone by
    // construction), used to map whole-arena scan hits back to entries.
    let name_offs: Vec<u32> = e.iter().map(|t| t.name_off).collect();
    let path_offs: Vec<u32> = e.iter().map(|t| t.path_off).collect();
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
        by_frn,
        dir_ids,
        file_ids,
        hidden_ids,
        system_ids,
        readonly_ids,
        reparse_ids,
        name_offs,
        path_offs,
        trigrams,
        trig_offs,
        trig_posts,
    })
}

/// Build the trigram index: every distinct 3-byte window of every (lowercased)
/// name maps to the sorted list of entries containing it.
///
/// Memory-conscious construction — NO (trigram, idx) pair array, NO sort:
/// 1. pass one counts occurrences per trigram (HashMap, ~85K entries ≈ 3 MB);
/// 2. prefix sums turn counts into the `trig_offs` layout and per-trigram
///    fill cursors;
/// 3. pass two walks entries in order and appends each entry index directly
///    into its trigram's posting slot — entry order IS ascending index order,
///    so postings come out sorted for free.
///
/// Per-entry duplicate trigrams are filtered by a 256-slot open-addressing
/// set with an epoch counter (no per-entry allocation, no rehash cost).
/// Peak extra memory ≈ the final trigs/offs/posts arrays (~255 MB at 4.17M
/// entries), instead of ~530 MB of sortable u64 pairs.
fn build_trigrams(names: &[u8], entries: &[Entry]) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    use std::collections::HashMap;
    let mut counts: HashMap<u32, u32> = HashMap::new();
    let mut dedup: Vec<(u32, u32)> = vec![(0, u32::MAX); 256]; // (epoch, trigram)
    let mut epoch: u32 = 0;
    // pass 1: per-entry distinct-trigram counting
    for e in entries {
        epoch = epoch.wrapping_add(1);
        let name = &names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
        for w in name.windows(3) {
            let t = ((w[0] as u32) << 16) | ((w[1] as u32) << 8) | w[2] as u32;
            // open addressing: 24-bit trigram hashed into 256 slots
            let mut slot = (t as usize) & 255;
            loop {
                if dedup[slot].0 != epoch {
                    dedup[slot] = (epoch, t);
                    *counts.entry(t).or_insert(0) += 1;
                    break;
                } else if dedup[slot].1 == t {
                    break; // already counted for this entry
                }
                slot = (slot + 1) & 255;
            }
        }
    }
    // pass 2 (prefix): trig_offs layout + per-trigram fill cursors
    let mut trigs: Vec<u32> = counts.keys().copied().collect();
    trigs.sort_unstable();
    let mut cursor: HashMap<u32, u32> = HashMap::with_capacity(trigs.len());
    let mut offs: Vec<u32> = Vec::with_capacity(trigs.len() + 1);
    offs.push(0);
    for &t in &trigs {
        cursor.insert(t, offs.last().copied().unwrap_or(0));
        offs.push(offs.last().copied().unwrap_or(0) + counts[&t]);
    }
    let mut posts: Vec<u32> = vec![0; offs.last().copied().unwrap_or(0) as usize];
    // pass 3: fill postings in entry order (ascending per trigram by design)
    for (i, e) in entries.iter().enumerate() {
        epoch = epoch.wrapping_add(1);
        let name = &names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
        for w in name.windows(3) {
            let t = ((w[0] as u32) << 16) | ((w[1] as u32) << 8) | w[2] as u32;
            let mut slot = (t as usize) & 255;
            loop {
                if dedup[slot].0 != epoch {
                    dedup[slot] = (epoch, t);
                    let c = cursor.get_mut(&t).expect("trigram counted in pass 1");
                    posts[*c as usize] = i as u32;
                    *c += 1;
                    break;
                } else if dedup[slot].1 == t {
                    break;
                }
                slot = (slot + 1) & 255;
            }
        }
    }
    (trigs, offs, posts)
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
            by_frn: View::from_slice(&o.by_frn),
            name_offs: View::from_slice(&o.name_offs),
            path_offs: View::from_slice(&o.path_offs),
            trigrams: View::from_slice(&o.trigrams),
            trig_offs: View::from_slice(&o.trig_offs),
            trig_posts: View::from_slice(&o.trig_posts),
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

/// Compiled glob matcher: literal segments between `Star`s, each compiled to
/// a Shift-And mask table (or a naive matcher for pathological >64-token
/// segments). Compiled ONCE per query; matching is per-candidate and
/// zero-allocation. Byte-level `?` semantics and ASCII case folding are
/// preserved exactly (same contract as the old DP matcher).
struct GlobProg {
    segs: Vec<GlobSeg>,
    /// No trailing `*` → the last segment must end exactly at hay.len()
    /// (full-string match, same as the old DP: `*.rs` must not match
    /// "main.rss", while the implicit leading star keeps the prefix free).
    anchored_end: bool,
}

#[allow(clippy::large_enum_variant)]
enum GlobSeg {
    /// Shift-And: `n` tokens, 2 KB on-stack mask table (both case variants of
    /// each literal folded in, so one table serves lowercase names AND
    /// original-case paths). Large-variant lint muted: the Bit variant is the
    /// overwhelmingly common case and lives on the caller's stack, not the heap.
    Bit { n: u8, masks: [u64; 256] },
    Naive(Vec<GTok>),
}

impl GlobProg {
    fn compile(tokens: &[GTok]) -> GlobProg {
        let anchored_end = tokens.last() != Some(&GTok::Star);
        let segs = tokens
            .split(|t| *t == GTok::Star)
            .flat_map(|seg| {
                if seg.is_empty() {
                    None
                } else if seg.len() <= 64 {
                    let mut masks = [0u64; 256];
                    for (j, t) in seg.iter().enumerate() {
                        match t {
                            GTok::Lit(c) => {
                                let c = *c; // already lowercased by glob_tokens
                                masks[c as usize] |= 1 << j;
                                if c.is_ascii_lowercase() {
                                    masks[(c - 32) as usize] |= 1 << j;
                                }
                            }
                            GTok::Any => {
                                for m in masks.iter_mut() {
                                    *m |= 1 << j;
                                }
                            }
                            GTok::Star => unreachable!("segments contain no stars"),
                        }
                    }
                    Some(GlobSeg::Bit { n: seg.len() as u8, masks })
                } else {
                    Some(GlobSeg::Naive(seg.to_vec()))
                }
            })
            .collect();
        GlobProg { segs, anchored_end }
    }

    /// Greedy earliest-match per segment is safe: an earlier segment end
    /// leaves a superset of valid starts for every later segment (a `*`
    /// matches any run, including empty). The anchored-end contract applies
    /// to the final segment only.
    fn matches(&self, hay: &[u8]) -> bool {
        let mut pos = 0usize;
        let last = self.segs.len();
        for (i, seg) in self.segs.iter().enumerate() {
            let anchored = i == last - 1 && self.anchored_end;
            let (found, end) = match seg {
                GlobSeg::Bit { n, masks } => bit_seg_match(*n as usize, masks, hay, pos, anchored),
                GlobSeg::Naive(tokens) => naive_seg_match(tokens, hay, pos, anchored),
            };
            if !found {
                return false;
            }
            pos = end;
        }
        true
    }
}

/// Shift-And segment match. Earliest position in hay[start..] where the
/// segment's literal bytes and `?` wildcards line up; with `anchored`, the
/// match must end exactly at hay.len() (the scan runs to the end and the
/// final automaton state answers "some suffix ends here").
fn bit_seg_match(n: usize, masks: &[u64; 256], hay: &[u8], start: usize, anchored: bool) -> (bool, usize) {
    if start + n > hay.len() {
        return (false, start);
    }
    let target = 1u64 << (n - 1);
    let mut state: u64 = 0;
    if anchored {
        for &b in &hay[start..] {
            state = ((state << 1) | 1) & masks[fold(b) as usize];
        }
        (state & target != 0, hay.len())
    } else {
        for (k, &b) in hay[start..].iter().enumerate() {
            state = ((state << 1) | 1) & masks[fold(b) as usize];
            if state & target != 0 {
                return (true, start + k + 1);
            }
        }
        (false, start)
    }
}

/// Fallback for >64-token segments (pathological: names are ≤ 255 bytes).
fn naive_seg_match(tokens: &[GTok], hay: &[u8], start: usize, anchored: bool) -> (bool, usize) {
    let n = tokens.len();
    if start + n > hay.len() {
        return (false, start);
    }
    if anchored {
        let pos = hay.len() - n;
        let ok = tokens.iter().enumerate().all(|(j, t)| match t {
            GTok::Lit(c) => fold(hay[pos + j]) == *c,
            GTok::Any => true,
            GTok::Star => unreachable!(),
        });
        return (ok, hay.len());
    }
    for pos in start..=hay.len() - n {
        let ok = tokens.iter().enumerate().all(|(j, t)| match t {
            GTok::Lit(c) => fold(hay[pos + j]) == *c,
            GTok::Any => true,
            GTok::Star => unreachable!(),
        });
        if ok {
            return (true, pos + n);
        }
    }
    (false, start)
}

/// Longest contiguous run of literal bytes in the token list — the SIMD
/// prefilter seed for the full-scan glob path. Glob has no alternation, so
/// every literal run must appear in any match: a sound superset filter.
fn longest_literal_run(tokens: &[GTok]) -> Vec<u8> {
    let mut best: Vec<u8> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for t in tokens {
        match t {
            GTok::Lit(c) => cur.push(*c),
            _ => {
                if cur.len() > best.len() {
                    best = cur.clone();
                }
                cur.clear();
            }
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    best
}

/// Bit-parallel glob match over bytes with ASCII case folding (no allocation).
/// Test-only convenience wrapper (the engine uses `GlobProg` directly).
#[cfg(test)]
fn glob_match(tokens: &[GTok], hay: &[u8]) -> bool {
    GlobProg::compile(tokens).matches(hay)
}

/// Extract exact literals from a (pre-lowercased) regex pattern for SIMD
/// prefiltering. regex-syntax guarantees every match contains at least one
/// extracted literal, so "entry contains any of these" is a sound superset
/// filter; candidates are verified with the full regex afterwards. Returns
/// None when the pattern has no usable literal (pure classes/quantifiers).
/// Capped at a few seeds so alternation-heavy patterns stay cheap.
fn regex_literal_seeds(pattern: &str) -> Option<Vec<Vec<u8>>> {
    use regex_syntax::hir::literal::Extractor;
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let seq = Extractor::new().extract(&hir);
    let lits = seq.literals()?;
    const MAX_SEEDS: usize = 8;
    let mut out: Vec<Vec<u8>> = Vec::new();
    for lit in lits {
        if !lit.is_exact() || lit.as_bytes().is_empty() {
            continue;
        }
        out.push(lit.as_bytes().to_vec());
        if out.len() >= MAX_SEEDS {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out) }
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
            (r"D:\proj\src\main.rs", EntryMeta { size: 2 << 20, mtime: now, ctime: now, flags: 0, frn: Some(42), ..Default::default() }),
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

    /// Reference DP glob matcher (the pre-bit-parallel implementation),
    /// retained as the correctness oracle for the compiled matcher.
    fn glob_match_dp(tokens: &[GTok], hay: &[u8]) -> bool {
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

    #[test]
    fn glob_bitparallel_cases() {
        let gm = |p: &str, h: &str| glob_match(&glob_tokens(p), h.as_bytes());
        assert!(gm("*.rs", "main.rs"));
        assert!(!gm("*.rs", "main.rss"));
        assert!(gm("a?c", "abc"));
        assert!(gm("a?c", "axc"));
        assert!(gm("a?c", "xabc")); // implicit leading star: substring semantics
        assert!(!gm("a?c", "ac"));
        assert!(gm("*foo*bar*", "xfooybarz"));
        assert!(!gm("*foo*bar*", "barfoo")); // order matters across stars
        assert!(gm("???", "abc"));
        assert!(!gm("???", "ab"));
        // byte-level `?` semantics over UTF-8, end-anchored without trailing star
        assert!(gm("报???", "报告"));
        assert!(gm("报???", "报道"));
        assert!(!gm("报?", "报告")); // 4-byte pattern vs 6-byte hay: cannot end-anchor
        assert!(!gm("报?", "报")); // hay shorter than the 4-token segment
        // case folding against original-case hay
        assert!(gm("MAIN*", "main.rs"));
        assert!(!gm("MAIN*", "sub.rs"));
        // >64-token segment: naive fallback path
        let long = "a".repeat(70) + "z";
        let hay = "x".repeat(10) + &"a".repeat(70) + "z";
        assert!(gm(&format!("*{long}*"), &hay));
        assert!(!gm(&format!("*{long}*"), &"a".repeat(69)));
        assert!(!gm(&format!("*{long}*"), &"a".repeat(71)));
    }

    #[test]
    fn glob_bitparallel_matches_dp_random() {
        // Deterministic pseudo-random patterns over {a,b,?,*} vs hays over
        // {a,b}; the compiled matcher must agree with the DP oracle in every
        // case (greedy earliest-match per segment is safe).
        let mut seed = 0x9E37_79B9u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        const ALPHA: &[u8] = b"ab?*";
        for _ in 0..800 {
            let plen = (rnd() % 8 + 1) as usize;
            let hlen = (rnd() % 14) as usize;
            let p: String = (0..plen).map(|_| ALPHA[(rnd() % 4) as usize] as char).collect();
            let h: String = (0..hlen).map(|_| (b'a' + (rnd() % 2) as u8) as char).collect();
            let tokens = glob_tokens(&p);
            assert_eq!(
                glob_match(&tokens, h.as_bytes()),
                glob_match_dp(&tokens, h.as_bytes()),
                "pattern={p} hay={h}"
            );
        }
    }

    #[test]
    fn glob_scan_prefilter() {
        let mut b = MemBuilder::default();
        let meta = EntryMeta { size: 0, mtime: 0, ctime: 0, flags: 0, ..Default::default() };
        b.push(r"D:\p\foo_bar.rs", meta.clone()); // id 0: "*foo*bar*" ✓
        b.push(r"D:\p\bar_foo.txt", meta.clone()); // id 1: order wrong
        b.push(r"D:\p\xabc", meta.clone()); // id 2: "a?c" ✓ (ends abc)
        b.push(r"D:\p\axc", meta.clone()); // id 3: "a?c" ✓ (ends axc)
        b.push(r"D:\p\abc.txt", meta.clone()); // id 4: "a?c" ✗ (ends txt)
        let mem = b.finish();

        let q = Query::parse("*foo*bar*").unwrap();
        assert_eq!(mem.search(&q), vec![0]);
        // no ≥2 literal run → memchr prefilter + bit-parallel verification;
        // no trailing star → end-anchored (full-string) semantics
        let q = Query::parse("a?c").unwrap();
        assert_eq!(mem.search(&q), vec![2, 3]);
        // 1-byte prefix: legacy contains-semantics (the by_name fast path only
        // engages for ≥2-byte prefixes) — every name containing 'a' matches
        let q = Query::parse("a*").unwrap();
        assert_eq!(mem.search(&q), vec![0, 1, 2, 3, 4]);
        // ≥2-byte prefix keeps the by_name fast path
        let q = Query::parse("ax*").unwrap();
        assert_eq!(mem.search(&q), vec![3]);
        let q = Query::parse("x*").unwrap();
        // contains-x semantics: "txt" also has an x
        assert_eq!(mem.search(&q), vec![1, 2, 3, 4]);
        // pure star = everything
        let q = Query::parse("*").unwrap();
        assert_eq!(mem.search(&q), vec![0, 1, 2, 3, 4]);
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

    #[test]
    fn find_frn_binary_search() {
        let mem = build_mem();
        let idx = mem.find_frn(42).expect("frn 42 present");
        assert_eq!(mem.path_at(idx as usize), r"D:\proj\src\main.rs");
        assert!(mem.find_frn(43).is_none());
        assert!(mem.find_frn(0).is_none()); // 0 = "no FRN"
    }

    #[test]
    fn dump_roundtrip_v3() {
        let mem = build_mem();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.feridx");
        mem.save(&path).unwrap();
        let loaded = MemIndex::load_dump(&path).unwrap();
        assert_eq!(loaded.len(), mem.len());
        assert_eq!(loaded.dir_count(), mem.dir_count());
        assert_eq!(loaded.file_count(), mem.file_count());
        let idx = loaded.find_frn(42).expect("frn survives roundtrip");
        assert_eq!(loaded.path_at(idx as usize), r"D:\proj\src\main.rs");
        let q = Query::parse("ext:rs").unwrap();
        assert_eq!(loaded.search(&q), mem.search(&q));
    }

    #[test]
    fn regex_scan() {
        let mem = build_mem();
        // anchored exact match: main.rs only
        let q = Query::parse("regex:^main\\.rs$").unwrap();
        let hits = mem.hits(&mem.search(&q), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"D:\proj\src\main.rs");
        // unanchored + class + quantifier: both .rs files
        let q = Query::parse("regex:^.*\\.rs$").unwrap();
        let ids = mem.search(&q);
        assert_eq!(ids.len(), 2);
        // negation composes with regex
        let q = Query::parse("ext:rs !regex:lib").unwrap();
        let hits = mem.hits(&mem.search(&q), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"D:\proj\src\main.rs");
    }

    #[test]
    fn arena_scan_boundary_artifacts() {
        // Adjacent names in the arena concatenate into "xab"+"bbc"+"ab" =
        // "xabbbcab". Boundary-straddling hits must be rejected, and a real
        // match that OVERLAPS an earlier artifact must still be found
        // (a non-overlapping iterator would silently drop it).
        let mut b = MemBuilder::default();
        let meta = EntryMeta { size: 0, mtime: 0, ctime: 0, flags: 0, ..Default::default() };
        b.push(r"D:\a\xab", meta.clone()); // id 0
        b.push(r"D:\a\bbc", meta.clone()); // id 1
        b.push(r"D:\a\ab", meta.clone()); // id 2
        let mem = b.finish();

        // "bb": first hit at abs=2 straddles entry0/entry1 (artifact), the
        // real hit at abs=3 sits fully inside entry1 → must return [1].
        let q = Query::parse("bb").unwrap();
        assert_eq!(mem.search(&q), vec![1]);

        // "ab": real hits in entry0 (pos 1) and entry2 (pos 0) → [0, 2].
        let q = Query::parse("ab").unwrap();
        assert_eq!(mem.search(&q), vec![0, 2]);

        // single-byte needle: only entry1 contains 'c'.
        let q = Query::parse("c").unwrap();
        assert_eq!(mem.search(&q), vec![1]);

        // long needle spanning multiple entries: nothing.
        let q = Query::parse("bbca").unwrap();
        assert!(mem.search(&q).is_empty());
    }

    #[test]
    fn path_ci_arena_scan() {
        let mut b = MemBuilder::default();
        let meta = EntryMeta { size: 0, mtime: 0, ctime: 0, flags: 0, ..Default::default() };
        b.push(r"D:\KitA\File", meta.clone()); // id 0: path contains "kita\file" (CI)
        b.push(r"D:\Other\kita", meta.clone()); // id 1: path contains "other\kita"
        b.push(r"D:\Other\KITB", meta.clone()); // id 2: "other\kitb", not "kita"
        let mem = b.finish();

        let q = Query::parse(r"kita\file").unwrap();
        assert_eq!(mem.search(&q), vec![0]);
        let q = Query::parse(r"other\kita").unwrap();
        assert_eq!(mem.search(&q), vec![1]);
        // bare token WITHOUT a separator is a basename term by contract; the
        // full-path substring form carries a separator:
        let q = Query::parse(r"d:\other").unwrap();
        assert_eq!(mem.search(&q), vec![1, 2]);
    }

    #[test]
    fn regex_arena_prefilter() {
        let mut b = MemBuilder::default();
        let meta = EntryMeta { size: 0, mtime: 0, ctime: 0, flags: 0, ..Default::default() };
        b.push(r"D:\p\main.rs", meta.clone()); // id 0
        b.push(r"D:\p\lib.rs", meta.clone()); // id 1
        b.push(r"D:\p\README.md", meta.clone()); // id 2
        b.push(r"D:\p\m.rs", meta.clone()); // id 3
        let mem = b.finish();

        // literal prefilter: ".rs" candidates {0,1,3} → anchored ^m narrows to 0,3
        let q = Query::parse("regex:^m").unwrap();
        assert_eq!(mem.search(&q), vec![0, 3]);
        // literal "main" (exact) → only 0
        let q = Query::parse("regex:main").unwrap();
        assert_eq!(mem.search(&q), vec![0]);
        // suffix literal: all .rs files
        let q = Query::parse("regex:\\.rs$").unwrap();
        assert_eq!(mem.search(&q), vec![0, 1, 3]);
        // no usable literal → fallback per-entry scan still correct
        let q = Query::parse("regex:[a-z]+").unwrap();
        assert_eq!(mem.search(&q), vec![0, 1, 2, 3]);
        // alternation: prefilter must not drop matches that lack one branch
        let q = Query::parse("regex:main|readme").unwrap();
        assert_eq!(mem.search(&q), vec![0, 2]);
    }

    #[test]
    fn trigram_fast_path_correctness() {
        let mut b = MemBuilder::default();
        let meta = EntryMeta { size: 0, mtime: 0, ctime: 0, flags: 0, ..Default::default() };
        b.push(r"D:\t\aaaa", meta.clone()); // id 0: "aaa" with duplicate trigram
        b.push(r"D:\t\abababa", meta.clone()); // id 1: "aba" twice, non-adjacent
        b.push(r"D:\t\abcabc", meta.clone()); // id 2
        b.push(r"D:\t\x", meta.clone()); // id 3: name < 3 bytes, no trigrams
        b.push(r"D:\t\bca", meta.clone()); // id 4: shares "bca" with id 2
        let mem = b.finish();

        // ≥3-byte needles take the trigram fast path (owned index has it)
        assert_eq!(mem.search(&Query::parse("aaa").unwrap()), vec![0]);
        assert_eq!(mem.search(&Query::parse("aba").unwrap()), vec![1]);
        assert_eq!(mem.search(&Query::parse("bca").unwrap()), vec![2, 4]);
        // intersection of three trigrams narrows to id 2
        assert_eq!(mem.search(&Query::parse("abcabc").unwrap()), vec![2]);
        // trigram absent from the whole index → empty
        assert!(mem.search(&Query::parse("zzz").unwrap()).is_empty());
        // trigram hit but verification fails (needle longer than name)
        assert!(mem.search(&Query::parse("aaaaa").unwrap()).is_empty());
        // <3-byte needles keep the whole-arena scan
        assert_eq!(mem.search(&Query::parse("ab").unwrap()), vec![1, 2]);
        assert_eq!(mem.search(&Query::parse("x").unwrap()), vec![3]);
    }

    #[test]
    fn v4_dump_compat_load() {
        let mem = build_mem();
        let dir = tempfile::tempdir().unwrap();
        let v5path = dir.path().join("t.feridx");
        mem.save(&v5path).unwrap();
        // Rebuild a genuine v4-layout file from the v5 one: first 19 sections,
        // 216-byte v4 header (v4 has accelerators but no trigram index).
        let v5 = std::fs::read(&v5path).unwrap();
        let mut offs5 = [0u64; SEC + 1];
        for (i, o) in offs5.iter_mut().enumerate() {
            *o = u64::from_le_bytes(v5[32 + i * 8..40 + i * 8].try_into().unwrap());
        }
        let counts5: Vec<u32> = (0..6)
            .map(|i| {
                u32::from_le_bytes(
                    v5[32 + (SEC + 1) * 8 + i * 4..32 + (SEC + 1) * 8 + (i + 1) * 4]
                        .try_into()
                        .unwrap(),
                )
            })
            .collect();
        let payload = v5[offs5[0] as usize..offs5[SEC_V4] as usize].to_vec();
        let mut hdr = Vec::with_capacity(HDR_LEN_V4);
        hdr.extend_from_slice(MAGIC);
        hdr.extend_from_slice(&4u32.to_le_bytes());
        hdr.extend_from_slice(&(mem.len() as u32).to_le_bytes());
        hdr.extend_from_slice(&0i64.to_le_bytes());
        hdr.extend_from_slice(&0i64.to_le_bytes());
        let mut offs4 = [0u64; SEC_V4 + 1];
        offs4[0] = HDR_LEN_V4 as u64;
        for i in 1..=SEC_V4 {
            offs4[i] = HDR_LEN_V4 as u64 + (offs5[i] - offs5[0]);
        }
        for o in offs4 {
            hdr.extend_from_slice(&o.to_le_bytes());
        }
        for c in counts5 {
            hdr.extend_from_slice(&c.to_le_bytes());
        }
        let mut v4file = hdr;
        v4file.extend_from_slice(&payload);
        let v4path = dir.path().join("t4.feridx");
        std::fs::write(&v4path, &v4file).unwrap();

        let loaded = MemIndex::load_dump(&v4path).unwrap();
        assert_eq!(loaded.len(), mem.len());
        assert!(loaded.trigrams.is_empty(), "v4 loads without a trigram index");
        // ≥3-byte needles fall back to the arena scan with identical results
        for q in ["report", "rs", "*.rs", "ext:rs", "regex:rs", "!hidden:true"] {
            assert_eq!(
                loaded.search(&Query::parse(q).unwrap()),
                mem.search(&Query::parse(q).unwrap()),
                "query {q}: v4-compat load vs owned mismatch"
            );
        }
    }

    #[test]
    fn v5_dump_roundtrip_trigram() {
        // The trigram index must survive a dump roundtrip (mapped views) and
        // serve the fast path identically to the owned index.
        let mut b = MemBuilder::default();
        let meta = EntryMeta { size: 0, mtime: 0, ctime: 0, flags: 0, ..Default::default() };
        b.push(r"D:\t\report.md", meta.clone());
        b.push(r"D:\t\年度报告.md", meta.clone());
        b.push(r"D:\t\main.rs", meta.clone());
        let mem = b.finish();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.feridx");
        mem.save(&path).unwrap();
        let loaded = MemIndex::load_dump(&path).unwrap();
        assert!(!loaded.trigrams.is_empty());
        for q in ["rep", "report", "报告", "年度", "main", "rs", "zzz"] {
            assert_eq!(
                loaded.search(&Query::parse(q).unwrap()),
                mem.search(&Query::parse(q).unwrap()),
                "query {q}: v5 roundtrip mismatch"
            );
        }
    }

    #[test]
    fn v3_dump_compat_load() {
        let mem = build_mem();
        let dir = tempfile::tempdir().unwrap();
        let v4path = dir.path().join("t.feridx");
        mem.save(&v4path).unwrap();
        // Rebuild a GENUINE v3-layout file from the v4 one: identical section
        // payloads (same order), but a 200-byte v3 header. (Simply patching
        // the version field would not work — v3/v4 headers have different
        // sizes and offset-table layouts.)
        let v4 = std::fs::read(&v4path).unwrap();
        let mut offs4 = [0u64; SEC + 1];
        for (i, o) in offs4.iter_mut().enumerate() {
            *o = u64::from_le_bytes(v4[32 + i * 8..40 + i * 8].try_into().unwrap());
        }
        let counts4: Vec<u32> = (0..6)
            .map(|i| {
                u32::from_le_bytes(
                    v4[32 + (SEC + 1) * 8 + i * 4..32 + (SEC + 1) * 8 + (i + 1) * 4]
                        .try_into()
                        .unwrap(),
                )
            })
            .collect();
        let payload = v4[offs4[0] as usize..offs4[SEC_V3] as usize].to_vec();
        let mut v3hdr = Vec::with_capacity(HDR_LEN_V3);
        v3hdr.extend_from_slice(MAGIC);
        v3hdr.extend_from_slice(&3u32.to_le_bytes());
        v3hdr.extend_from_slice(&(mem.len() as u32).to_le_bytes());
        v3hdr.extend_from_slice(&0i64.to_le_bytes()); // created
        v3hdr.extend_from_slice(&0i64.to_le_bytes()); // reserved
        let mut offs3 = [0u64; SEC_V3 + 1];
        offs3[0] = HDR_LEN_V3 as u64;
        for i in 1..=SEC_V3 {
            offs3[i] = HDR_LEN_V3 as u64 + (offs4[i] - offs4[0]);
        }
        for o in offs3 {
            v3hdr.extend_from_slice(&o.to_le_bytes());
        }
        for c in counts4 {
            v3hdr.extend_from_slice(&c.to_le_bytes());
        }
        let mut v3file = v3hdr;
        v3file.extend_from_slice(&payload);
        let v3path = dir.path().join("t3.feridx");
        std::fs::write(&v3path, &v3file).unwrap();

        let loaded = MemIndex::load_dump(&v3path).unwrap();
        assert_eq!(loaded.len(), mem.len());
        for q in ["rs", "report", "*.rs", "ext:rs", "a?c", "regex:rs", "!hidden:true"] {
            assert_eq!(
                loaded.search(&Query::parse(q).unwrap()),
                mem.search(&Query::parse(q).unwrap()),
                "query {q}: v3-compat load vs owned mismatch"
            );
        }
        // v4 roundtrip keeps the accelerator sections mapped (no aux rebuild).
        let v4b = MemIndex::load_dump(&v4path).unwrap();
        assert_eq!(
            v4b.search(&Query::parse("rs").unwrap()),
            mem.search(&Query::parse("rs").unwrap())
        );
    }
}
