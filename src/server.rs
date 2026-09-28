//! HTTP API server (axum) with a minimal web UI. Queries run entirely
//! against the in-memory engine; `/api/rescan` rebuilds from the volumes and
//! refreshes the dump + the live engine.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, State},
    response::Html,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::indexer::{self, Method};
use crate::mem::{MemBuilder, MemIndex, SortKey, dump_path};
use crate::usn;
use crate::{Hit, fold_lower, push};

#[derive(Clone)]
struct AppState {
    mem: Arc<RwLock<Arc<MemIndex>>>,
    /// Real-time changes pushed by `fer monitor` (see [`Overlay`]).
    ///
    /// Lock order: **overlay before mem**. Every writer that swaps the engine
    /// (the hot-reload thread, `/api/rescan`) holds the overlay write lock
    /// across the swap *and* the `Overlay::reset()` that follows it, and every
    /// reader takes the overlay read lock before cloning the engine — so the
    /// engine and the overlay's resolved removal ids a query sees always
    /// belong to the same dump generation.
    overlay: Arc<RwLock<Overlay>>,
    db: PathBuf,
    cache: Arc<Mutex<QueryCache>>,
}

/// Real-time overlay fed by `fer monitor` over `crate::push`.
///
/// The dump snapshot is the base index. `monitor` holds the *live* index in
/// memory but only writes it back every `--flush-secs` (1800 s by default —
/// a flush rewrites the whole multi-GB dump, so shortening it means
/// terabytes/day of writes). Everything in this struct is therefore "what
/// changed since the last flush", and it is what makes a file created seconds
/// ago searchable.
///
/// `delta` is a small [`MemIndex`] built from the pending appends, so the
/// overlay reuses the entire query engine instead of maintaining a parallel
/// matcher. Both sides are keyed by lowercase path, which makes re-applying a
/// batch (the monitor resends its whole pending set each round) idempotent.
#[derive(Default)]
struct Overlay {
    appended: HashMap<String, push::AppendEntry>,
    /// Folded-lowercase paths pending removal, shared with query handlers by
    /// `Arc`: a query clones 8 bytes instead of deep-copying the whole set
    /// (measured in one monitor window: 21,777 then 75,852 removals — that was
    /// a per-query copy of tens of thousands of strings).
    removed: Arc<HashSet<String>>,
    /// `removed` resolved to entry ids of the dump engine, ascending and
    /// deduplicated. Resolved under the overlay lock by [`Overlay::apply`], so
    /// a query filters with one sorted-slice lookup instead of folding and
    /// hashing a path per candidate hit — and `total` can count the
    /// removals that actually hit the query exactly.
    removed_ids: Arc<Vec<u32>>,
    delta: Option<Arc<MemIndex>>,
    batches: u64,
    last_apply: Option<Instant>,
    /// True once either pending set hit [`OVERLAY_MAX`]: the overlay stops
    /// growing and a flush (or `fer index`) is required to shrink it again.
    saturated: bool,
    /// Bumped by [`Overlay::reset`] — the moments entry ids stop meaning
    /// anything (dump hot reload, rescan). `removed_ids` is rebuilt by every
    /// `apply`, so this is a diagnostic guard rather than a cache key;
    /// `/api/feed` reports it.
    generation: u64,
}

/// Cap on each pending set held in the overlay (appends and removals alike). A
/// flush clears both long before this in practice; the cap only guards against
/// a monitor whose flush never fires (e.g. `--flush-secs` set absurdly high).
const OVERLAY_MAX: usize = 200_000;

impl Overlay {
    /// Apply one batch. This is a **full replacement**, not a delta: the
    /// monitor re-sends its entire pending set every round (see `crate::push`),
    /// so anything missing from `batch` is no longer pending.
    ///
    /// That distinction is load-bearing. A file created and then deleted within
    /// one flush window is dropped from the monitor's `appended` list and never
    /// enters its `removed` set (there is no index entry to point at yet), so no
    /// later batch mentions it at all — an accumulating receiver would keep the
    /// stale append forever and keep reporting a path that no longer exists.
    /// `mem` is the dump engine the removal paths are resolved against. The
    /// caller holds the overlay lock, so the engine and the ids derived from it
    /// cannot drift apart (see the lock-order note on [`AppState::overlay`]).
    fn apply(&mut self, b: push::Batch, mem: &MemIndex) {
        self.appended.clear();
        let mut removed: HashSet<String> = HashSet::with_capacity(b.remove.len().min(OVERLAY_MAX));
        for p in &b.remove {
            if removed.len() >= OVERLAY_MAX {
                self.saturated = true;
                break;
            }
            removed.insert(fold_lower(p));
        }
        // Resolving up to 200k paths is the expensive part of a batch, and the
        // monitor re-sends its whole pending set every round: skip the resolve
        // entirely when this batch repeated the previous set verbatim (a round
        // that only created files changes nothing on this side), otherwise let
        // `incremental_removed_ids` reuse the previous resolution and search
        // only the newcomers.
        let unchanged = self.removed.len() == removed.len() && *self.removed == removed;
        // Keep the previous set alive for the incremental resolve below (an Arc
        // swap, no copy).
        let prev = std::mem::replace(&mut self.removed, Arc::new(removed));
        for e in b.append {
            let k = fold_lower(&e.p);
            if self.removed.contains(&k) {
                continue; // deleted after being created, within the same window
            }
            if self.appended.len() >= OVERLAY_MAX {
                self.saturated = true;
                break;
            }
            self.appended.insert(k, e);
        }
        self.batches += 1;
        self.last_apply = Some(Instant::now());
        self.rebuild();
        if !unchanged {
            // The engine's entry ids are the only thing `removed` is ever
            // compared against from here on: they are invalidated wholesale by
            // [`Overlay::reset`], never patched.
            let ids = incremental_removed_ids(&self.removed, &prev, &self.removed_ids, mem);
            self.removed_ids = Arc::new(ids);
        }
    }

    fn rebuild(&mut self) {
        if self.appended.is_empty() {
            self.delta = None;
            return;
        }
        let mut b = MemBuilder::default();
        for e in self.appended.values() {
            b.push(&e.p, e.meta());
        }
        self.delta = Some(Arc::new(b.finish()));
    }

    /// Cheap per-query view: the delta index plus the removal ids. Both are
    /// `Arc` clones — this used to deep-copy the whole removal set on every
    /// single query.
    fn snapshot(&self) -> (Option<Arc<MemIndex>>, Arc<Vec<u32>>) {
        (self.delta.clone(), self.removed_ids.clone())
    }

    /// Drop everything: a fresh dump already contains these changes, so keeping
    /// them would shadow the reloaded snapshot. The resolved removal ids go
    /// with them — they name entries of the engine that is being replaced.
    ///
    /// Callers must hold the overlay write lock across the engine swap and this
    /// call together, otherwise a query can pair one generation's engine with
    /// another generation's ids.
    fn reset(&mut self) {
        self.appended.clear();
        self.removed = Arc::new(HashSet::new());
        self.removed_ids = Arc::new(Vec::new());
        self.delta = None;
        self.saturated = false;
        self.generation += 1;
    }

    fn pending(&self) -> (usize, usize) {
        (self.appended.len(), self.removed.len())
    }
}

/// Resolve folded removal paths to ids of `mem` (`find_path_idx` is an
/// ASCII-CI binary search over the dump's `by_path` permutation, O(log n)
/// each). A path this snapshot does not contain — created after the dump was
/// written, or already flushed away — resolves to nothing, which is exactly
/// the "nothing to filter" answer. The result is ascending and deduplicated so
/// query handlers can binary-search it.
fn resolve_removed(removed: &HashSet<String>, mem: &MemIndex) -> Vec<u32> {
    let mut ids: Vec<u32> = Vec::with_capacity(removed.len());
    for p in removed {
        if let Some(i) = mem.find_path_idx(p) {
            ids.push(i as u32);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// [resolve_removed], reusing prev_ids when the new removal set is a superset
/// of the previous one.
///
/// The monitor's pending removal set only *grows* inside a flush window (USN
/// deletes and rename-into-place retirements are inserted, and only a flush
/// clears it) and it re-sends the whole set every round — so the newcomers are
/// a handful of paths while the set itself can hold tens of thousands.
/// Resolving all of them per round measured **1.79 s for 75,852 paths** against
/// a 100k-entry index in a debug build, and the resolve runs under the overlay
/// write lock, i.e. it stalls queries.
///
/// The superset test makes this a pure optimization, never a correctness
/// dependency: anything else — a shrinking set (a flush boundary clears the
/// monitor's set before serve has reloaded the dump) or the batches right after
/// [Overlay::reset] — falls back to a full resolve.
fn incremental_removed_ids(
    now: &HashSet<String>,
    prev: &HashSet<String>,
    prev_ids: &[u32],
    mem: &MemIndex,
) -> Vec<u32> {
    if now.len() < prev.len() || !prev.iter().all(|p| now.contains(p)) {
        return resolve_removed(now, mem);
    }
    let mut out: Vec<u32> = prev_ids.to_vec();
    out.extend(
        now.iter()
            .filter(|p| !prev.contains(*p))
            .filter_map(|p| mem.find_path_idx(p).map(|i| i as u32)),
    );
    // prev_ids was already ascending and deduplicated, so the merged set only
    // needs a re-sort of tens of thousands of u32 — microseconds next to the
    // binary searches it replaces.
    out.sort_unstable();
    out.dedup();
    out
}

/// Tiny TTL-bounded LRU for identical repeated queries (agents re-issue the
/// same search constantly). TTL keeps results fresh across external index
/// refreshes; capacity is small enough that eviction scans are trivial.
const CACHE_CAP: usize = 256;
const CACHE_TTL: Duration = Duration::from_secs(3);

#[derive(Default)]
struct QueryCache {
    map: HashMap<String, (serde_json::Value, Instant)>,
}

impl QueryCache {
    fn get(&mut self, key: &str) -> Option<serde_json::Value> {
        let fresh = self
            .map
            .get(key)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(v, _)| v.clone());
        if fresh.is_some() {
            return fresh;
        }
        self.purge_expired();
        None
    }

    fn insert(&mut self, key: String, value: serde_json::Value) {
        self.purge_expired();
        if self.map.len() >= CACHE_CAP
            && let Some(oldest) = self
                .map
                .iter()
                .min_by(|(_, (_, a)), (_, (_, b))| a.cmp(b))
                .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        self.map.insert(key, (value, Instant::now()));
    }

    fn purge_expired(&mut self) {
        self.map.retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

/// `warm` controls the background page warm-up: when true (the default) the
/// whole dump is touched once so the first client query pays no page-fault tax,
/// at the cost of pulling the entire dump into the working set. Pass false
/// (`serve --no-warm`) when the working-set figure matters more than first-query
/// latency — the pages are file-backed, so either way the OS can reclaim them.
pub async fn serve(
    addr: &str,
    mem: MemIndex,
    db: &std::path::Path,
    warm: bool,
    feed_addr: Option<&str>,
) -> Result<()> {
    eprintln!(
        "[server] memory index ready: {} entries, {} MB",
        mem.len(),
        mem.memory_bytes() / (1 << 20)
    );
    let state = AppState {
        mem: Arc::new(RwLock::new(Arc::new(mem))),
        overlay: Arc::new(RwLock::new(Overlay::default())),
        db: db.to_path_buf(),
        cache: Arc::new(Mutex::new(QueryCache::default())),
    };
    // Background warm-up: touch one byte per page of every mapped section so
    // the first client query doesn't pay the mmap page-fault tax. Sequential
    // reads over ~1 GB; the OS scheduler deprioritizes naturally. CLI
    // single-shot runs skip this (warm-up would exceed the query cost), and
    // `--no-warm` skips it here as well.
    if warm {
        let warm_mem = state.mem.read().unwrap().clone();
        std::thread::spawn(move || warm_mem.warm());
    }
    // Real-time change feed: `fer monitor` broadcasts the changes it has already
    // applied in memory. Without this, a file created now stays invisible until
    // the monitor's next dump flush — 1800 s by default, because a flush rewrites
    // the whole multi-GB dump. See `crate::push` for the wire format.
    if let Some(feed_src) = feed_addr {
        let feed = push::Feed::connect(feed_src);
        let ov = state.overlay.clone();
        let feed_mem = state.mem.clone();
        let drain_cache = state.cache.clone();
        std::thread::spawn(move || {
            loop {
                let batches = feed.drain();
                if !batches.is_empty() {
                    {
                        // Lock order: overlay, then mem. `apply` resolves the
                        // removal paths against this engine, so both are read
                        // under one lock (see AppState::overlay).
                        let mut g = ov.write().unwrap();
                        let mem = feed_mem.read().unwrap().clone();
                        for b in batches {
                            g.apply(b, &mem);
                        }
                    }
                    // Results changed: drop cached responses so the next query
                    // reflects the new state instead of a 3 s-old answer.
                    drain_cache.lock().unwrap().clear();
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }
    // Background dump hot-reload: `fer monitor` and external `fer index` runs
    // rewrite the dump while this server is up. Poll its mtime and swap the
    // engine, so a long-lived serve never answers from a stale snapshot. The
    // dump is written to a temp file and then renamed (MemIndex::save), so a
    // poll can never catch a half-written file; a reload is a ~1ms mmap whose
    // pages come straight from the page cache the writer just populated.
    let reload_slot = state.mem.clone();
    let reload_cache = state.cache.clone();
    let reload_overlay = state.overlay.clone();
    let reload_dump = dump_path(db);
    std::thread::spawn(move || {
        let mut stamp = std::fs::metadata(&reload_dump)
            .and_then(|m| m.modified())
            .ok();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let now = std::fs::metadata(&reload_dump)
                .and_then(|m| m.modified())
                .ok();
            if now.is_none() || now == stamp {
                continue;
            }
            stamp = now;
            match MemIndex::load_dump(&reload_dump) {
                Ok(fresh) => {
                    let n = fresh.len();
                    // Warm BEFORE the swap, not after: the freshly mapped dump
                    // is all page faults, and the old engine keeps serving
                    // during this ~1s sequential touch, so no client ever pays
                    // the cold-page tax on the new snapshot (measured 631ms
                    // cold vs 22ms warm for `a?c`). Skipped under --no-warm.
                    if warm {
                        fresh.warm();
                    }
                    // Swap the engine and reset the overlay under one overlay
                    // write lock: `reset` drops removal ids that name entries
                    // of the *old* engine, and a query must never observe the
                    // new engine paired with those ids (or the reverse).
                    if let Ok(mut o) = reload_overlay.write() {
                        *reload_slot.write().unwrap() = Arc::new(fresh);
                        // The reloaded dump already contains every change the
                        // overlay was carrying; keeping them would shadow the
                        // fresh snapshot (and permanently pin entries the dump
                        // has since dropped).
                        o.reset();
                    }
                    reload_cache.lock().unwrap().clear();
                    eprintln!("[server] dump changed on disk — reloaded {n} entries");
                }
                Err(e) => eprintln!("[server] dump reload failed (keeping old engine): {e:#}"),
            }
        }
    });
    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/health", get(health))
        .route("/api/search", get(search))
        .route("/api/feed", get(feed))
        .route("/api/du", get(du))
        .route("/api/stats", get(stats))
        .route("/api/reveal", post(reveal))
        .route("/api/rescan", post(rescan))
        .route("/api/rebuild", post(rebuild))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("[server] listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct RevealQuery {
    path: String,
}

/// Show `path` in Explorer with the entry selected — what a double-click in the
/// UI does.
///
/// Safety notes, since this is the one endpoint that launches a process:
/// * The path travels as a **separate argv element**, never interpolated into a
///   command string, so quotes/`&`/`|` in a file name cannot become a second
///   command. `explorer.exe` also parses `/select,<path>` itself, and a leading
///   `/` or `-` is neutralised by the fact that the whole argument is prefixed
///   with `/select,`.
/// * It is a POST, not a GET, so a stray link or a prefetch cannot trigger it.
/// * `Origin`/`Referer`, when present, must match this server — otherwise any web
///   page the user visits could make their Explorer pop open folders.
/// * Only existing paths are accepted, so this cannot be used to probe the
///   filesystem for names that do not exist.
async fn reveal(Query(q): Query<RevealQuery>, headers: axum::http::HeaderMap) -> Json<Value> {
    // Same-origin check. A missing header (curl, an agent) is allowed; anything
    // pointing elsewhere is refused — that is what stops a random web page the
    // user has open from popping folders up in their Explorer.
    for name in ["origin", "referer"] {
        if let Some(v) = headers.get(name).and_then(|h| h.to_str().ok()) {
            let same = v.starts_with("http://127.0.0.1:")
                || v.starts_with("http://localhost:")
                || v.starts_with("http://[::1]:");
            if !v.is_empty() && !same {
                return Json(json!({ "ok": false, "error": "拒绝跨源请求" }));
            }
        }
    }
    if !std::path::Path::new(&q.path).exists() {
        return Json(json!({ "ok": false, "error": "路径不存在" }));
    }
    match std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", q.path))
        .spawn()
    {
        Ok(_) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Change-feed status: what the real-time overlay is carrying on top of the
/// dump snapshot. `delta_entries` is the number of entries a query can see
/// beyond the snapshot right now.
async fn feed(State(st): State<AppState>) -> Json<Value> {
    let g = match st.overlay.read() {
        Ok(g) => g,
        Err(_) => return Json(json!({ "ok": false, "error": "overlay lock poisoned" })),
    };
    let (appended, removed) = g.pending();
    // Sample a few pending paths. Without this the feed only exposes counts, so
    // "the overlay holds 500 entries but my new file is not searchable" cannot be
    // told apart from "the file never entered the overlay at all".
    let sample_append: Vec<&str> = g.appended.values().take(4).map(|e| e.p.as_str()).collect();
    let sample_remove: Vec<&str> = g.removed.iter().take(3).map(|s| s.as_str()).collect();
    Json(json!({
        "ok": true,
        "pending_append": appended,
        "pending_remove": removed,
        "delta_entries": g.delta.as_ref().map(|d| d.len()).unwrap_or(0),
        "batches_applied": g.batches,
        "saturated": g.saturated,
        // Removals that resolved to an entry id of the current dump: only
        // these can actually hide a hit. A path that resolves to nothing is
        // either newer than the dump or already flushed away.
        "removed_in_dump": g.removed_ids.len(),
        "overlay_generation": g.generation,
        "last_apply_age_ms": g.last_apply.map(|t| t.elapsed().as_millis()),
        "sample_append": sample_append,
        "sample_remove": sample_remove,
    }))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
    /// `name|path|size|allocated|mtime|ctime`. Absent (or empty) keeps the
    /// index-order behaviour, byte-for-byte: the response passes through
    /// [`MemIndex::hits`] exactly as before.
    sort: Option<String>,
    /// `1|true|yes|on` / `0|false|no|off`. Only meaningful together with
    /// `sort`; a key sorts ascending unless this is set.
    desc: Option<String>,
}

/// Parse the `sort=` knob. `Ok(None)` means "no server-side sorting".
fn parse_sort(raw: Option<&str>) -> Result<Option<SortKey>, String> {
    match raw {
        None | Some("") => Ok(None),
        Some(s) => SortKey::parse(s).map(Some).ok_or_else(|| {
            format!(
                "unknown sort key `{s}` (expected one of: {})",
                SortKey::ALL.map(SortKey::as_str).join(", ")
            )
        }),
    }
}

/// Parse a boolean query knob. Accepts the HTML checkbox spelling (`on`) and
/// `yes`/`no` as well as `1`/`0` and `true`/`false`, so the documented
/// `&desc=1` and `&desc=true` behave identically.
fn parse_flag(raw: Option<&str>) -> Result<bool, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(false),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => Err(format!(
            "`{other}` is not a boolean (use 1/0 or true/false)"
        )),
    }
}

/// One page of hits in the requested order: index order when `sort` is `None`
/// (the original behaviour), otherwise the top-`limit` under [`SortKey`].
fn page(mem: &MemIndex, ids: &[u32], limit: usize, sort: Option<SortKey>, desc: bool) -> Vec<Hit> {
    match sort {
        None => mem.hits(ids, limit),
        Some(k) => mem.hits_sorted(ids, limit, k, desc),
    }
}

/// Ids from `ids` that are not in `skip`. Both slices must be ascending.
/// `cap` stops the scan as soon as that many survivors were collected — that
/// is what keeps the unsorted page cheap (the caller wants a prefix, not the
/// whole filtered set).
fn ids_without(ids: &[u32], skip: &[u32], cap: Option<usize>) -> Vec<u32> {
    let mut out = Vec::with_capacity(cap.map_or(ids.len(), |c| c.min(ids.len())));
    let mut si = 0usize;
    for &id in ids {
        while si < skip.len() && skip[si] < id {
            si += 1;
        }
        if skip.get(si) == Some(&id) {
            continue;
        }
        out.push(id);
        if cap.is_some_and(|c| out.len() >= c) {
            break;
        }
    }
    out
}

/// The overlay-aware query core — `/api/search`'s blocking body, factored out
/// so tests can drive it without an HTTP server.
///
/// `total` is the number of hits this very query would return with **no
/// limit**, the overlay's removals included. It is therefore never greater than
/// what the caller can page through; before this it counted dump entries the
/// overlay had already deleted (measured: a deleted file answered `total=1`
/// with an empty `hits`).
fn search_overlay(
    mem: &MemIndex,
    delta: Option<&Arc<MemIndex>>,
    removed_ids: &[u32],
    q: &crate::query::Query,
    limit: usize,
    sort: Option<SortKey>,
    desc: bool,
) -> (u64, Vec<Hit>) {
    let mut hits: Vec<Hit> = Vec::with_capacity(limit.min(1024));
    let mut total = 0u64;
    // The overlay holds everything `fer monitor` has applied since the last
    // dump flush. Query it first so a file created seconds ago lands on page
    // one — the snapshot alone would not have it at all until the next flush
    // (1800 s by default). No removal filter is needed on this side:
    // `Overlay::apply` drops any append whose folded path is also in the
    // batch's removal set, so the delta and the removal set are disjoint by
    // construction (and there is a test pinning that).
    if let Some(d) = delta {
        let dids = d.search(q);
        total += dids.len() as u64;
        hits.extend(page(d, &dids, limit, sort, desc));
    }
    let ids = mem.search(q);
    total += ids.len() as u64;
    let mut base = if removed_ids.is_empty() {
        page(mem, &ids, limit, sort, desc)
    } else {
        // How many of this query's candidates are actually removed. `ids` is
        // ascending (MemIndex::search contract) and `removed_ids` is
        // ascending, so this is an O(m log n) intersection — far cheaper than
        // hashing every candidate, and a broad query can carry millions.
        let removed_hits = removed_ids
            .iter()
            .filter(|id| ids.binary_search(id).is_ok())
            .count();
        total -= removed_hits as u64;
        if removed_hits == 0 {
            page(mem, &ids, limit, sort, desc)
        } else {
            // At most `removed_hits` candidates can be filtered out, so the top
            // (limit + removed_hits) of the unfiltered set already contains the
            // whole final page: an element that survives into the top-limit has
            // fewer than limit + removed_hits elements ahead of it unfiltered.
            let kept = match sort {
                // Unsorted: the page is a prefix of the ascending ids, so the
                // scan can stop as soon as `limit` survivors are collected.
                None => ids_without(&ids, removed_ids, Some(limit)),
                // Sorted: the top-N needs the entire filtered candidate set —
                // a prefix could hide the winner behind a removed entry.
                Some(_) => ids_without(&ids, removed_ids, None),
            };
            page(mem, &kept, limit, sort, desc)
        }
    };
    hits.append(&mut base);
    if let Some(k) = sort {
        // Subset top-N merge. The pool is all of the delta side plus the dump
        // side's own top-`limit`, and the union's true top-`limit` is inside
        // it: dropping the *other* side's elements can only improve an
        // element's rank, so anything ranking within the union's top-`limit`
        // also ranks within the top-`limit` of its own side (and the delta side
        // is taken in full). Re-sorting the pool and cutting to `limit` is
        // therefore exactly the union's top-N. This needs both sides to order
        // by one total order, which is why `SortKey::cmp_hits` (used here) and
        // `MemIndex::cmp_entries` (used by `hits_sorted`) share the same key
        // and the same ascending raw-path tie-break.
        hits.sort_unstable_by(|a, b| k.cmp_hits(a, b, desc));
    }
    hits.truncate(limit);
    (total, hits)
}

async fn search(State(st): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Value> {
    let t = std::time::Instant::now();
    let limit = q.limit.map(|l| l.min(10_000)).unwrap_or(100);
    let sort = match parse_sort(q.sort.as_deref()) {
        Ok(s) => s,
        Err(e) => return Json(json!({ "ok": false, "error": e })),
    };
    let desc = match parse_flag(q.desc.as_deref()) {
        Ok(d) => d,
        Err(e) => return Json(json!({ "ok": false, "error": e })),
    };
    // Repeat-query fast path: agents re-issue identical searches constantly;
    // a fresh TTL entry is served without touching the engine. The new knobs
    // are part of the key; the unsorted key stays byte-identical to the
    // pre-sort one (`desc` cannot matter without `sort`).
    let cache_key = match sort {
        None => format!("{}|{}", q.q, limit),
        Some(k) => format!("{}|{}|{}|{}", q.q, limit, k.as_str(), desc),
    };
    if let Some(mut hit) = st.cache.lock().unwrap().get(&cache_key) {
        // The cached payload carries the ORIGINAL computation's took_ms —
        // refresh it so the field reflects this request (near-zero on a hit).
        if let Some(obj) = hit.as_object_mut() {
            obj.insert("took_ms".to_string(), json!(t.elapsed().as_millis()));
        }
        return Json(hit);
    }
    let parsed = match crate::query::Query::parse(&q.q) {
        Ok(p) => p,
        Err(e) => return Json(json!({ "ok": false, "error": e.to_string() })),
    };
    // Bind the Arc clones to locals first: a temporary RwLock guard in the
    // if-let scrutinee would live across the `.await` and make the handler
    // future !Send. The scan (up to ~70ms) runs off the executor.
    //
    // Both locks are taken here, overlay first (see AppState::overlay), so the
    // engine and the overlay's removal ids are one consistent generation.
    let (mem, delta, removed_ids) = {
        let ov = st.overlay.read().unwrap();
        let mem = st.mem.read().unwrap().clone();
        let (delta, removed_ids) = ov.snapshot();
        (mem, delta, removed_ids)
    };
    let qq = q.q.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        search_overlay(&mem, delta.as_ref(), &removed_ids, &parsed, limit, sort, desc)
    })
    .await;
    let resp = match outcome {
        Ok((total, hits)) => json!({
            "ok": true,
            "query": qq,
            "engine": "mem",
            "count": hits.len(),
            "total": total,
            "took_ms": t.elapsed().as_millis(),
            "hits": hits,
        }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    };
    st.cache.lock().unwrap().insert(cache_key, resp.clone());
    Json(resp)
}

#[derive(Deserialize)]
struct DuQuery {
    path: String,
    depth: Option<usize>,
    top: Option<usize>,
    allocated: Option<bool>,
}

/// WizTree-style directory size aggregation from the in-memory index.
async fn du(State(st): State<AppState>, Query(q): Query<DuQuery>) -> Json<Value> {
    let t = std::time::Instant::now();
    // Bind the Arc clone to a local first: the whole-volume scan can take
    // ~1s, so it runs off the executor via spawn_blocking.
    let mem = st.mem.read().unwrap().clone();
    let path = q.path.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        crate::du::scan(
            &mem,
            &path,
            q.depth,
            q.top.unwrap_or(20),
            q.allocated.unwrap_or(false),
        )
    })
    .await;
    match outcome {
        Ok(Ok(report)) => {
            let mut v = serde_json::to_value(&report).unwrap_or_default();
            let obj = v.as_object_mut().expect("report serializes to an object");
            obj.insert("ok".to_string(), json!(true));
            obj.insert("took_ms".to_string(), json!(t.elapsed().as_millis()));
            Json(v)
        }
        Ok(Err(e)) => Json(json!({ "ok": false, "error": e.to_string() })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn stats(State(st): State<AppState>) -> Json<Value> {
    let mem = st.mem.read().unwrap().clone();
    let files = mem.file_count() as u64;
    let dirs = mem.dir_count() as u64;
    let dump = dump_path(&st.db);
    let dump_mb = std::fs::metadata(&dump)
        .map(|m| m.len() / (1 << 20))
        .unwrap_or(0);
    Json(json!({
        "ok": true,
        "files": files,
        "dirs": dirs,
        "entries": files + dirs,
        "dump": dump.to_string_lossy(),
        "dump_mb": dump_mb,
        "mem_bytes": mem.memory_bytes(),
    }))
}

async fn rescan(State(st): State<AppState>) -> Json<Value> {
    let db = st.db.clone();
    let slot = st.mem.clone();
    let overlay = st.overlay.clone();
    let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let vols = usn::list_volumes();
        let (report, mem) = indexer::build(&vols, Method::Auto)?;
        let dump = dump_path(&db);
        mem.save(&dump)?;
        // Same order as the hot-reload thread: swap + reset under the overlay
        // write lock. The rebuilt dump already contains the pending changes,
        // and the overlay's removal ids name entries of the old engine.
        {
            let mut o = overlay.write().unwrap();
            *slot.write().unwrap() = Arc::new(mem);
            o.reset();
        }
        Ok(json!({ "report": report, "dump": dump.to_string_lossy() }))
    })
    .await;
    // The index changed under the cache — drop every cached response.
    st.cache.lock().unwrap().clear();
    match outcome {
        Ok(Ok(v)) => Json(json!({ "ok": true, "result": v })),
        Ok(Err(e)) => Json(json!({ "ok": false, "error": format!("{e:#}") })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Ask the running `fer monitor` to re-scan its volume and rewrite the dump.
///
/// The default deployment runs `serve` unelevated — raw $MFT access needs admin,
/// which is exactly why `/api/rescan` fails there. The monitor *is* elevated, so
/// this forwards the request to its control channel; the dump-mtime hot reload
/// then swaps the fresh index in without restarting anything.
async fn rebuild() -> Json<Value> {
    let outcome = tokio::task::spawn_blocking(|| {
        crate::control::request(crate::control::DEFAULT_CONTROL_ADDR, "rebuild")
    })
    .await;
    match outcome {
        Ok(Ok(msg)) => Json(json!({ "ok": true, "message": msg })),
        Ok(Err(e)) => Json(json!({ "ok": false, "error": format!("{e:#}") })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// The web UI, built from `webui/` by Vite into a single self-contained
/// index.html (all JS and CSS inlined by vite-plugin-singlefile — see
/// `webui/vite.config.js`). Embedding the build output keeps `fer serve` a
/// lone binary with no asset directory to ship or locate at runtime.
///
/// `build.rs` checks that the file exists and tells you to run
/// `cd webui && npm install && npm run build` if it does not.
const INDEX_HTML: &str = include_str!("../webui/dist/index.html");

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryMeta;
    use crate::query::Query;

    /// Overlay / sorting fixture. Sizes, mtimes and names contain ties on
    /// purpose, and directories sit next to files:
    ///
    /// | id | path                 | size | mtime | dir |
    /// |----|----------------------|------|-------|-----|
    /// | 0  | D:\docs\readme.txt  | 500  | 30    | no  |
    /// | 1  | D:\docs\annual.md   | 500  | 10    | no  |
    /// | 2  | D:\proj\main.rs     | 900  | 20    | no  |
    /// | 3  | D:\proj\src         | 0    | 99    | yes |
    /// | 4  | D:\media\zz.bin     | 900  | 5     | no  |
    /// | 5  | D:\a.txt             | 100  | 40    | no  |
    /// | 6  | D:\media\sub        | 0    | 1     | yes |
    fn fixture() -> MemIndex {
        let rows: [(&str, u64, i64, bool); 7] = [
            (r"D:\docs\readme.txt", 500, 30, false),
            (r"D:\docs\annual.md", 500, 10, false),
            (r"D:\proj\main.rs", 900, 20, false),
            (r"D:\proj\src", 0, 99, true),
            (r"D:\media\zz.bin", 900, 5, false),
            (r"D:\a.txt", 100, 40, false),
            (r"D:\media\sub", 0, 1, true),
        ];
        let mut b = MemBuilder::default();
        for (path, size, mtime, is_dir) in rows {
            b.push(
                path,
                EntryMeta {
                    is_dir,
                    size,
                    // Deliberately `size + 1`, so an `allocated` sort cannot
                    // pass by accidentally being a `size` sort.
                    allocated: size + 1,
                    mtime,
                    ctime: -mtime,
                    ..Default::default()
                },
            );
        }
        b.finish()
    }

    fn all_ids(mem: &MemIndex) -> Vec<u32> {
        mem.search(&Query::parse("").unwrap())
    }

    fn paths(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| h.path.clone()).collect()
    }

    fn batch(append: &[&str], remove: &[&str]) -> push::Batch {
        push::Batch {
            append: append
                .iter()
                .map(|p| push::AppendEntry::new(*p, EntryMeta::default()))
                .collect(),
            remove: remove.iter().map(|p| p.to_string()).collect(),
        }
    }

    // -- sorting ------------------------------------------------------------

    /// `name` sorts on the basename (CI), `path` on the whole path (also CI,
    /// matching the dump's `by_path` order); both fall back to the raw path
    /// bytes, which is what makes the order total.
    #[test]
    fn hits_sorted_by_name_and_path() {
        let mem = fixture();
        let ids = all_ids(&mem);
        assert_eq!(
            paths(&mem.hits_sorted(&ids, 10, SortKey::Name, false)),
            vec![
                r"D:\a.txt",
                r"D:\docs\annual.md",
                r"D:\proj\main.rs",
                r"D:\docs\readme.txt",
                r"D:\proj\src",
                r"D:\media\sub",
                r"D:\media\zz.bin",
            ]
        );
        assert_eq!(
            paths(&mem.hits_sorted(&ids, 3, SortKey::Path, false)),
            vec![r"D:\a.txt", r"D:\docs\annual.md", r"D:\docs\readme.txt"]
        );
        // descending reverses the key only
        assert_eq!(
            paths(&mem.hits_sorted(&ids, 1, SortKey::Name, true)),
            vec![r"D:\media\zz.bin"]
        );
    }

    /// Ties break on the path, and `limit` can cut inside a tie group — the
    /// size-desc top 3 stops in the middle of the two 500-byte files.
    #[test]
    fn hits_sorted_size_ties_and_limits() {
        let mem = fixture();
        let ids = all_ids(&mem);
        assert_eq!(
            paths(&mem.hits_sorted(&ids, 3, SortKey::Size, true)),
            vec![r"D:\media\zz.bin", r"D:\proj\main.rs", r"D:\docs\annual.md"]
        );
        assert_eq!(
            paths(&mem.hits_sorted(&ids, 10, SortKey::Size, false)),
            vec![
                r"D:\media\sub",
                r"D:\proj\src",
                r"D:\a.txt",
                r"D:\docs\annual.md",
                r"D:\docs\readme.txt",
                r"D:\media\zz.bin",
                r"D:\proj\main.rs",
            ]
        );
    }

    /// `allocated`, `mtime` and `ctime` are separate keys from `size`: the
    /// fixture's directories carry `allocated = 1` and the negated mtime makes
    /// the ctime order the exact mirror of the mtime order.
    #[test]
    fn hits_sorted_allocated_and_times() {
        let mem = fixture();
        let ids = all_ids(&mem);
        assert_eq!(
            paths(&mem.hits_sorted(&ids, 2, SortKey::Allocated, false)),
            vec![r"D:\media\sub", r"D:\proj\src"]
        );
        let by_mtime = paths(&mem.hits_sorted(&ids, 3, SortKey::Mtime, true));
        assert_eq!(
            by_mtime,
            vec![r"D:\proj\src", r"D:\a.txt", r"D:\docs\readme.txt"]
        );
        assert_eq!(
            paths(&mem.hits_sorted(&ids, 3, SortKey::Ctime, false)),
            by_mtime
        );
    }

    /// `hits_sorted` must be the top-N under exactly the order that
    /// `SortKey::cmp_hits` defines — that is the order the overlay merge uses,
    /// so any drift would silently break "the page is the union's top-N".
    #[test]
    fn hits_sorted_agrees_with_hit_level_order() {
        let mem = fixture();
        let ids = all_ids(&mem);
        for key in SortKey::ALL {
            for desc in [false, true] {
                let mut reference = mem.hits(&ids, usize::MAX);
                reference.sort_by(|a, b| key.cmp_hits(a, b, desc));
                for limit in [0usize, 1, 2, 3, 6, 7, 100] {
                    assert_eq!(
                        paths(&mem.hits_sorted(&ids, limit, key, desc)),
                        paths(&reference[..limit.min(reference.len())]),
                        "{key:?} desc={desc} limit={limit}"
                    );
                }
            }
        }
    }

    // -- overlay ------------------------------------------------------------

    /// `apply` replaces both sets wholesale (the monitor re-sends its full
    /// pending set) and `snapshot` shares them by `Arc` — the per-query deep
    /// copy of the removal set is what the `Arc` exists to kill.
    #[test]
    fn overlay_apply_replaces_and_shares() {
        let mem = fixture();
        let mut ov = Overlay::default();
        ov.apply(batch(&[r"D:\new.txt"], &[r"D:\docs\readme.txt"]), &mem);
        let shared = ov.removed.clone();
        assert!(Arc::ptr_eq(&shared, &ov.removed));
        let (delta, ids) = ov.snapshot();
        assert_eq!(ids.as_slice(), &[0u32]); // readme.txt is entry 0
        assert_eq!(
            paths(&delta.unwrap().hits(&[0], 10)),
            vec![r"D:\new.txt"]
        );

        ov.apply(batch(&[r"D:\other.txt"], &[r"D:\proj\main.rs"]), &mem);
        let (delta, ids) = ov.snapshot();
        assert_eq!(ids.as_slice(), &[2u32]); // main.rs is entry 2
        assert!(!Arc::ptr_eq(&shared, &ov.removed));
        assert_eq!(
            paths(&delta.unwrap().hits(&[0], 10)),
            vec![r"D:\other.txt"]
        );
        assert_eq!(ov.pending(), (1, 1));
        // removals are stored folded, and the previous batch is gone
        assert!(ov.removed.contains(r"d:\proj\main.rs"));
        assert!(!ov.removed.contains(r"d:\docs\readme.txt"));
    }

    /// A path created and deleted inside one monitor window is dropped outright
    /// (it is in the batch's removal set, so it never enters the delta) — the
    /// property that keeps deleted files from coming back through the overlay.
    #[test]
    fn append_shadowed_by_removal_is_dropped() {
        let mem = fixture();
        let mut ov = Overlay::default();
        ov.apply(batch(&[r"D:\flip.txt"], &[r"D:\flip.txt"]), &mem);
        assert_eq!(ov.pending(), (0, 1));
        assert!(ov.snapshot().0.is_none());
    }

    /// Removal paths resolve to dump entry ids, case-insensitively. A path the
    /// dump does not contain resolves to nothing and must not hide a hit.
    #[test]
    fn removals_resolve_to_dump_ids() {
        let mem = fixture();
        let mut ov = Overlay::default();
        ov.apply(batch(&[], &[r"d:\DOCS\ReadMe.TXT", r"D:\never-existed.txt"]), &mem);
        let (_, removed_ids) = ov.snapshot();
        assert_eq!(removed_ids.as_slice(), &[0u32]);

        let (total, hits) = search_overlay(
            &mem,
            None,
            &removed_ids,
            &Query::parse("").unwrap(),
            100,
            None,
            false,
        );
        assert_eq!(total, 6);
        assert_eq!(hits.len(), 6);
    }

    /// `reset` clears everything and bumps the generation that labels the
    /// entry-id space (a fresh dump renumbers entries).
    #[test]
    fn overlay_reset_clears_and_bumps_generation() {
        let mem = fixture();
        let mut ov = Overlay::default();
        ov.apply(batch(&[r"D:\new.txt"], &[r"D:\a.txt"]), &mem);
        assert_eq!(ov.generation, 0);
        assert_eq!(ov.pending(), (1, 1));
        ov.reset();
        assert_eq!(ov.generation, 1);
        assert_eq!(ov.pending(), (0, 0));
        let (delta, removed_ids) = ov.snapshot();
        assert!(delta.is_none());
        assert!(removed_ids.is_empty());
    }

    /// Both pending sets are capped: past `OVERLAY_MAX` the overlay stops
    /// growing and reports `saturated` (the signal that a flush or
    /// `fer index` is needed). `/api/feed` surfaces it.
    #[test]
    fn overlay_saturates_past_the_cap() {
        let mem = fixture();
        let pending: Vec<String> = (0..OVERLAY_MAX + 1)
            .map(|i| format!(r"D:\pending\{i}.txt"))
            .collect();
        let mut ov = Overlay::default();
        ov.apply(
            push::Batch {
                append: Vec::new(),
                remove: pending,
            },
            &mem,
        );
        assert!(ov.saturated);
        assert_eq!(ov.pending(), (0, OVERLAY_MAX));
        // None of them exist in the dump, so none of them filter anything.
        assert!(ov.snapshot().1.is_empty());
    }

    // -- total ---------------------------------------------------------------

    /// The measured defect: a deleted file answered `total = 1` with an empty
    /// `hits` array. Both are now zero — sorted or not — and the removal is
    /// exact (the other six entries survive).
    #[test]
    fn deleted_file_leaves_hits_and_total_empty() {
        let mem = fixture();
        let mut ov = Overlay::default();
        ov.apply(batch(&[], &[r"D:\docs\readme.txt"]), &mem);
        let (delta, removed_ids) = ov.snapshot();
        let q = Query::parse("readme").unwrap();
        for sort in [None, Some(SortKey::Size)] {
            let (total, hits) =
                search_overlay(&mem, delta.as_ref(), &removed_ids, &q, 100, sort, true);
            assert_eq!(total, 0, "sort={sort:?}");
            assert!(hits.is_empty(), "sort={sort:?}");
        }
        let (total, hits) = search_overlay(
            &mem,
            delta.as_ref(),
            &removed_ids,
            &Query::parse("").unwrap(),
            100,
            None,
            false,
        );
        assert_eq!(total, 6);
        assert!(!hits.iter().any(|h| h.path.ends_with("readme.txt")));
    }

    /// `total` is the number of hits the same query returns with **no limit** —
    /// for every limit and every sort key, and with the delta counted in.
    #[test]
    fn total_equals_unlimited_hit_count() {
        let mem = fixture();
        let mut ov = Overlay::default();
        ov.apply(
            batch(
                &[r"D:\newly-created.txt", r"D:\media\brand-new.bin"],
                &[r"D:\docs\readme.txt", r"D:\proj\main.rs"],
            ),
            &mem,
        );
        let (delta, removed_ids) = ov.snapshot();
        let q = Query::parse("").unwrap();
        let (full_total, all) =
            search_overlay(&mem, delta.as_ref(), &removed_ids, &q, usize::MAX, None, false);
        assert_eq!(full_total as usize, all.len());
        assert_eq!(full_total, 7); // 7 dump entries - 2 removed + 2 delta
        for limit in [1usize, 2, 3, 7, 100] {
            for sort in [None, Some(SortKey::Size), Some(SortKey::Mtime)] {
                let (total, page) =
                    search_overlay(&mem, delta.as_ref(), &removed_ids, &q, limit, sort, true);
                assert_eq!(total, full_total, "limit={limit} sort={sort:?}");
                assert_eq!(page.len(), 7.min(limit), "limit={limit} sort={sort:?}");
            }
        }
        assert!(!all.iter().any(|h| h.path.ends_with("readme.txt")));
        assert!(all.iter().any(|h| h.path.ends_with("newly-created.txt")));
    }

    // -- overlay + sorting ---------------------------------------------------

    /// A sorted page is the **union's** top-N: a huge delta entry outranks every
    /// dump entry even though the delta side is only taken "in full" while the
    /// dump side contributes its own top-N. Brute force over both sides is the
    /// reference.
    #[test]
    fn sorted_page_is_the_union_top_n() {
        let mem = fixture();
        let mut ov = Overlay::default();
        let mut b = batch(
            &[r"D:\media\huge.bin", r"D:\docs\tiny.txt"],
            &[
                r"D:\media\zz.bin",
                r"D:\proj\main.rs",
                r"D:\docs\readme.txt",
                r"D:\a.txt",
                r"D:\proj\src",
            ],
        );
        b.append[0].s = 99 << 20;
        b.append[1].s = 1;
        ov.apply(b, &mem);
        let (delta, removed_ids) = ov.snapshot();
        let delta = delta.unwrap();
        let q = Query::parse("").unwrap();

        for desc in [false, true] {
            let (total, page) = search_overlay(
                &mem,
                Some(&delta),
                &removed_ids,
                &q,
                2,
                Some(SortKey::Size),
                desc,
            );
            let want = if desc {
                vec![r"D:\media\huge.bin", r"D:\docs\annual.md"]
            } else {
                vec![r"D:\media\sub", r"D:\docs\tiny.txt"]
            };
            assert_eq!(paths(&page), want, "desc={desc}");

            // Reference: materialize every entry the overlay can see.
            let mut reference = delta.hits(&delta.search(&q), usize::MAX);
            let dump_ids: Vec<u32> = mem
                .search(&q)
                .into_iter()
                .filter(|id| !removed_ids.contains(id))
                .collect();
            reference.extend(mem.hits(&dump_ids, usize::MAX));
            reference.sort_unstable_by(|a, b| SortKey::Size.cmp_hits(a, b, desc));
            assert_eq!(total as usize, reference.len(), "desc={desc}");
            assert_eq!(paths(&page), paths(&reference[..2]), "desc={desc}");
        }
    }

    /// Without `sort` the page is the pre-sort one: index order, delta first.
    #[test]
    fn unsorted_page_keeps_index_order() {
        let mem = fixture();
        let q = Query::parse("").unwrap();
        let ids = mem.search(&q);
        let (total, hits) = search_overlay(&mem, None, &[], &q, 3, None, false);
        assert_eq!(total as usize, ids.len());
        assert_eq!(paths(&hits), paths(&mem.hits(&ids, 3)));

        let mut ov = Overlay::default();
        ov.apply(batch(&[r"D:\zzz-new.txt"], &[]), &mem);
        let (delta, removed_ids) = ov.snapshot();
        let (total, hits) =
            search_overlay(&mem, delta.as_ref(), &removed_ids, &q, 3, None, false);
        assert_eq!(total, 8);
        assert_eq!(hits.first().map(|h| h.path.as_str()), Some(r"D:\zzz-new.txt"));
        assert_eq!(hits.len(), 3);
    }

    // -- helpers -------------------------------------------------------------

    #[test]
    fn sort_and_flag_parsing() {
        assert_eq!(parse_sort(None), Ok(None));
        assert_eq!(parse_sort(Some("")), Ok(None));
        assert_eq!(parse_sort(Some("SIZE")), Ok(Some(SortKey::Size)));
        assert_eq!(parse_sort(Some("Allocated")), Ok(Some(SortKey::Allocated)));
        assert!(parse_sort(Some("bogus")).is_err());
        assert_eq!(parse_flag(None), Ok(false));
        assert_eq!(parse_flag(Some("1")), Ok(true));
        assert_eq!(parse_flag(Some(" true ")), Ok(true));
        assert_eq!(parse_flag(Some("off")), Ok(false));
        assert!(parse_flag(Some("maybe")).is_err());
        for key in SortKey::ALL {
            assert_eq!(SortKey::parse(key.as_str()), Some(key));
        }
    }

    /// Scale check for the "never materialize the whole match set" promise:
    /// 100k candidates, a 100-hit page. Only the page is turned into strings
    /// (the reference below needs all 100k), and the timing prints under
    /// `--nocapture`.
    #[test]
    fn hits_sorted_page_is_cheap_at_scale() {
        const N: usize = 100_000;
        let mut b = MemBuilder::default();
        for i in 0..N {
            b.push(
                &format!(r"D:\scale\dir{:03}\file{:06}.dat", i % 512, i),
                EntryMeta {
                    size: (i as u64 * 7919) % 100_000,
                    mtime: i as i64,
                    ..Default::default()
                },
            );
        }
        let mem = b.finish();
        let ids = all_ids(&mem);
        assert_eq!(ids.len(), N);

        let t0 = std::time::Instant::now();
        let page = mem.hits_sorted(&ids, 100, SortKey::Size, true);
        let page_ms = t0.elapsed().as_millis();

        let t1 = std::time::Instant::now();
        let mut reference = mem.hits(&ids, usize::MAX);
        reference.sort_by(|a, b| SortKey::Size.cmp_hits(a, b, true));
        let full_ms = t1.elapsed().as_millis();

        assert_eq!(paths(&page), paths(&reference[..100]));
        eprintln!(
            "hits_sorted: {N} ids -> 100 hits in {page_ms} ms              (materialize-everything reference: {full_ms} ms)"
        );
    }

    /// The removal set is resolved once per batch, under the overlay lock —
    /// not once per query. This is the worst case seen in production (75,852
    /// pending removals, from the lead's monitor measurement) against a
    /// 100k-entry index.
    #[test]
    fn resolving_a_large_removal_set_is_bounded() {
        const N: usize = 100_000;
        let mut b = MemBuilder::default();
        for i in 0..N {
            b.push(
                &format!(r"D:\bulk\dir{:03}\file{:06}.dat", i % 512, i),
                EntryMeta::default(),
            );
        }
        let mem = b.finish();
        let removed: HashSet<String> = (0..75_852)
            .map(|i| format!(r"d:\bulk\dir{:03}\file{:06}.dat", i % 512, i))
            .collect();

        let t = std::time::Instant::now();
        let ids = resolve_removed(&removed, &mem);
        let ms = t.elapsed().as_millis();
        assert_eq!(ids.len(), removed.len());
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids must be ascending");
        eprintln!(
            "resolve_removed: {} paths -> {} ids in {ms} ms",
            removed.len(),
            ids.len()
        );

        // One newcomer on top of the same 75k set: the incremental path must
        // reuse the previous ids rather than re-searching all of them.
        let mut with_new = removed.clone();
        with_new.insert(r"d:\bulk\dir001\file999999.dat".to_string());
        let t2 = std::time::Instant::now();
        let ids2 = incremental_removed_ids(&with_new, &removed, &ids, &mem);
        let inc_ms = t2.elapsed().as_millis();
        assert_eq!(ids2, resolve_removed(&with_new, &mem));
        eprintln!("incremental_removed_ids: 1 newcomer on 75,852 paths in {inc_ms} ms");
    }

    /// Whatever branch the incremental resolver takes, the cached ids must
    /// always equal a from-scratch resolve of the current removal set — the
    /// invariant `/api/search` relies on for both `total` and the filtering.
    #[test]
    fn removal_ids_always_match_a_full_resolve() {
        let mem = fixture();
        let transitions: [&[&str]; 6] = [
            &[],
            &[r"D:\docs\readme.txt"],                              // first removal
            &[r"D:\docs\readme.txt", r"D:\proj\main.rs"],          // grow
            &[r"D:\proj\main.rs"],                                 // shrink (flush boundary)
            &[r"D:\media\zz.bin", r"D:\a.txt"],                    // replace
            &[r"D:\a.txt", r"D:\docs\readme.txt", r"D:\nope.txt"], // grow + unresolvable
        ];
        let mut ov = Overlay::default();
        for (i, set) in transitions.iter().enumerate() {
            ov.apply(batch(&[], set), &mem);
            assert_eq!(
                ov.snapshot().1.as_slice(),
                resolve_removed(&ov.removed, &mem).as_slice(),
                "transition {i}: {set:?}"
            );
        }
    }

    #[test]
    fn ids_without_skips_and_caps() {
        let ids = [1u32, 2, 3, 5, 8, 13];
        assert_eq!(ids_without(&ids, &[], None), ids.to_vec());
        assert_eq!(ids_without(&ids, &[2, 5], None), vec![1, 3, 8, 13]);
        // `cap` stops the scan: the unsorted page only needs a prefix
        assert_eq!(ids_without(&ids, &[2, 5], Some(2)), vec![1, 3]);
        assert_eq!(ids_without(&ids, &[0, 14], None), ids.to_vec());
        assert_eq!(
            ids_without(&ids, &[1, 2, 3, 5, 8, 13], None),
            Vec::<u32>::new()
        );
    }
}
