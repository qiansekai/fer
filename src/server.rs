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
use crate::mem::{MemBuilder, MemIndex, dump_path};
use crate::usn;
use crate::{fold_lower, push};

#[derive(Clone)]
struct AppState {
    mem: Arc<RwLock<Arc<MemIndex>>>,
    /// Real-time changes pushed by `fer monitor` (see [`Overlay`]).
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
    removed: HashSet<String>,
    delta: Option<Arc<MemIndex>>,
    batches: u64,
    last_apply: Option<Instant>,
    /// True once the pending set hit [`OVERLAY_MAX`]: the overlay stops growing
    /// and a flush (or `fer index`) is required to shrink it again.
    saturated: bool,
}

/// Cap on pending appends held in the overlay. A flush clears it long before
/// this in practice; the cap only guards against a monitor whose flush never
/// fires (e.g. `--flush-secs` set absurdly high).
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
    fn apply(&mut self, b: push::Batch) {
        self.appended.clear();
        self.removed.clear();
        for p in &b.remove {
            self.removed.insert(fold_lower(p));
        }
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

    /// Cheap per-query view: the delta index plus the removal set.
    fn snapshot(&self) -> (Option<Arc<MemIndex>>, Arc<HashSet<String>>) {
        (self.delta.clone(), Arc::new(self.removed.clone()))
    }

    /// Drop everything: a fresh dump already contains these changes, so keeping
    /// them would shadow the reloaded snapshot.
    fn reset(&mut self) {
        self.appended.clear();
        self.removed.clear();
        self.delta = None;
        self.saturated = false;
    }

    fn pending(&self) -> (usize, usize) {
        (self.appended.len(), self.removed.len())
    }
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
        let drain_cache = state.cache.clone();
        std::thread::spawn(move || {
            loop {
                let batches = feed.drain();
                if !batches.is_empty() {
                    {
                        let mut g = ov.write().unwrap();
                        for b in batches {
                            g.apply(b);
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
                    *reload_slot.write().unwrap() = Arc::new(fresh);
                    reload_cache.lock().unwrap().clear();
                    // The reloaded dump already contains every change the overlay
                    // was carrying; keeping them would shadow the fresh snapshot
                    // (and permanently pin entries the dump has since dropped).
                    if let Ok(mut o) = reload_overlay.write() {
                        o.reset();
                    }
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
        .route("/api/rescan", post(rescan))
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
        "last_apply_age_ms": g.last_apply.map(|t| t.elapsed().as_millis()),
        "sample_append": sample_append,
        "sample_remove": sample_remove,
    }))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn search(State(st): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Value> {
    let t = std::time::Instant::now();
    let limit = q.limit.map(|l| l.min(10_000)).unwrap_or(100);
    // Repeat-query fast path: agents re-issue identical searches constantly;
    // a fresh TTL entry is served without touching the engine.
    let cache_key = format!("{}|{}", q.q, limit);
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
    // Bind the Arc clone to a local first: a temporary RwLock guard in the
    // if-let scrutinee would live across the `.await` and make the handler
    // future !Send. The scan (up to ~70ms) runs off the executor.
    let mem = st.mem.read().unwrap().clone();
    let (delta, removed) = st.overlay.read().unwrap().snapshot();
    let qq = q.q.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        // The overlay holds everything `fer monitor` has applied since the last
        // dump flush. Query it first so a file created seconds ago lands on page
        // one — the snapshot alone would not have it at all until the next flush
        // (1800 s by default).
        let mut hits: Vec<crate::Hit> = Vec::with_capacity(limit);
        let mut total = 0u64;
        if let Some(d) = &delta {
            let dids = d.search(&parsed);
            total += dids.len() as u64;
            for h in d.hits(&dids, limit) {
                if !removed.contains(&fold_lower(&h.path)) {
                    hits.push(h);
                }
            }
        }
        let ids = mem.search(&parsed);
        total += ids.len() as u64;
        // Over-fetch by the removal-set size so deletions the monitor reported
        // cannot starve the page below `limit` entries.
        let extra = removed.len().min(10_000);
        let mut base = mem.hits(&ids, limit.saturating_add(extra));
        if !removed.is_empty() {
            base.retain(|h| !removed.contains(&fold_lower(&h.path)));
        }
        hits.append(&mut base);
        hits.truncate(limit);
        (total, hits)
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
    let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let vols = usn::list_volumes();
        let (report, mem) = indexer::build(&vols, Method::Auto)?;
        let dump = dump_path(&db);
        mem.save(&dump)?;
        *slot.write().unwrap() = Arc::new(mem);
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

/// The web UI, built from `webui/` by Vite into a single self-contained
/// index.html (all JS and CSS inlined by vite-plugin-singlefile — see
/// `webui/vite.config.js`). Embedding the build output keeps `fer serve` a
/// lone binary with no asset directory to ship or locate at runtime.
///
/// `build.rs` checks that the file exists and tells you to run
/// `cd webui && npm install && npm run build` if it does not.
const INDEX_HTML: &str = include_str!("../webui/dist/index.html");