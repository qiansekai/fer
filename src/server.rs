//! HTTP API server (axum) with a minimal web UI. Queries run entirely
//! against the in-memory engine; `/api/rescan` rebuilds from the volumes and
//! refreshes the dump + the live engine.

use std::collections::HashMap;
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
use crate::mem::{MemIndex, dump_path};
use crate::usn;

#[derive(Clone)]
struct AppState {
    mem: Arc<RwLock<Arc<MemIndex>>>,
    db: PathBuf,
    cache: Arc<Mutex<QueryCache>>,
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

pub async fn serve(addr: &str, mem: MemIndex, db: &std::path::Path) -> Result<()> {
    eprintln!(
        "[server] memory index ready: {} entries, {} MB",
        mem.len(),
        mem.memory_bytes() / (1 << 20)
    );
    let state = AppState {
        mem: Arc::new(RwLock::new(Arc::new(mem))),
        db: db.to_path_buf(),
        cache: Arc::new(Mutex::new(QueryCache::default())),
    };
    // Background warm-up: touch one byte per page of every mapped section so
    // the first client query doesn't pay the mmap page-fault tax. Sequential
    // reads over ~1 GB; the OS scheduler deprioritizes naturally. CLI
    // single-shot runs skip this (warm-up would exceed the query cost).
    let warm_mem = state.mem.read().unwrap().clone();
    std::thread::spawn(move || warm_mem.warm());
    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/health", get(health))
        .route("/api/search", get(search))
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
    let qq = q.q.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let ids = mem.search(&parsed);
        let total = ids.len() as u64;
        let hits = mem.hits(&ids, limit);
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

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh">
<head>
<meta charset="utf-8">
<title>File-Engine-Rust</title>
<style>
body{font-family:system-ui,Segoe UI,sans-serif;margin:2rem auto;max-width:56rem;padding:0 1rem;background:#0f1115;color:#e8eaed}
h1{font-size:1.4rem}h1 span{color:#8ab4f8}
input#q{width:100%;padding:.6rem .8rem;font-size:1rem;background:#1c1f26;color:#e8eaed;border:1px solid #3c4043;border-radius:8px;box-sizing:border-box}
.row{margin:.5rem 0;color:#9aa0a6;font-size:.85rem}
ul{list-style:none;padding:0;margin:0}
li{padding:.3rem .2rem;border-bottom:1px solid #23262d;font-family:Consolas,monospace;font-size:.9rem;overflow-wrap:anywhere}
li.dir{color:#8ab4f8}
</style>
</head>
<body>
<h1><span>File-Engine-Rust</span> — 毫秒级全盘文件名搜索</h1>
<input id="q" placeholder="输入文件名（支持 * ? 通配符），回车或输入即搜…" autofocus>
<div class="row"><label><input type="checkbox" id="p"> 全路径匹配</label>
<span id="meta" style="margin-left:1em"></span></div>
<ul id="res"></ul>
<script>
const q=document.getElementById('q'),res=document.getElementById('res'),meta=document.getElementById('meta');
let t;
q.addEventListener('input',()=>{clearTimeout(t);t=setTimeout(run,120);});
async function run(){
  const v=q.value.trim();if(!v){res.innerHTML='';meta.textContent='';return;}
  const s=Date.now();
  const r=await fetch('/api/search?q='+encodeURIComponent(v)+'&limit=100').then(x=>x.json());
  meta.textContent=r.total+' 个结果 · '+(Date.now()-s)+' ms · '+r.engine;
  res.innerHTML=(r.hits||[]).map(h=>'<li'+(h.is_dir?' class="dir"':'')+'>'+h.path.replace(/&/g,'&amp;').replace(/</g,'&lt;')+'</li>').join('');
}
run();
</script>
</body>
</html>"#;
