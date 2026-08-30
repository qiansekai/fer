//! HTTP API server (axum) with a minimal web UI.

use std::sync::{Arc, Mutex, RwLock};

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
use crate::mem::MemIndex;
use crate::store::Store;
use crate::usn;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Store>>,
    mem: Arc<RwLock<Option<Arc<MemIndex>>>>,
}

pub async fn serve(addr: &str, store: Store, mem_index: bool) -> Result<()> {
    let mem = if mem_index {
        match load_mem(&store) {
            Some(m) => Some(Arc::new(m)),
            None => None,
        }
    } else {
        None
    };
    let state = AppState {
        db: Arc::new(Mutex::new(store)),
        mem: Arc::new(RwLock::new(mem)),
    };
    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/health", get(health))
        .route("/api/search", get(search))
        .route("/api/stats", get(stats))
        .route("/api/rescan", post(rescan))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("[server] listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Load the in-memory engine: prefer the pre-built dump (sub-second), fall
/// back to materializing from SQLite when the dump is missing or stale.
fn load_mem(store: &Store) -> Option<MemIndex> {
    let t = std::time::Instant::now();
    let db = store.db_path();
    let dump = crate::mem::dump_path(db);
    if crate::mem::dump_is_fresh(db) {
        match MemIndex::load_dump(&dump) {
            Ok(m) => {
                eprintln!(
                    "[server] memory index loaded from dump: {} entries, {} MB in {} ms",
                    m.len(),
                    m.memory_bytes() / (1 << 20),
                    t.elapsed().as_millis()
                );
                return Some(m);
            }
            Err(e) => {
                eprintln!("[server] dump load failed ({e:#}) — falling back to SQL materialization");
            }
        }
    }
    match store.load_mem_index() {
        Ok(m) => {
            eprintln!(
                "[server] memory index materialized from SQL: {} entries, {} MB in {} ms",
                m.len(),
                m.memory_bytes() / (1 << 20),
                t.elapsed().as_millis()
            );
            Some(m)
        }
        Err(e) => {
            eprintln!("[server] memory index failed ({e:#}) — falling back to SQL");
            None
        }
    }
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
    let parsed = match crate::query::Query::parse(&q.q) {
        Ok(p) => p,
        Err(e) => return Json(json!({ "ok": false, "error": e.to_string() })),
    };
    // Hybrid dispatch: ≥3-char substrings ride the FTS5 trigram index (SQL
    // wins there); everything else — short substrings, suffix, prefix, ranges,
    // flags, globs — runs in the memory engine (Everything-style). The mem
    // scan (up to ~70ms) runs off the executor via spawn_blocking.
    let use_sql = crate::mem::MemIndex::prefers_sql(&parsed);
    if !use_sql {
        // NB: bind the clone to a variable first — a temporary RwLock guard
        // in an if-let scrutinee would live across the `.await` below and
        // make the handler future !Send.
        let mem = st.mem.read().unwrap().clone();
        if let Some(mem) = mem {
            let qq = q.q.clone();
            let pq = parsed.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let ids = mem.search(&pq);
                let total = ids.len() as u64;
                let hits = mem.hits(&ids, limit);
                (total, hits)
            })
            .await;
            if let Ok((total, hits)) = outcome {
                return Json(json!({
                    "ok": true,
                    "query": qq,
                    "engine": "mem",
                    "count": hits.len(),
                    "total": total,
                    "took_ms": t.elapsed().as_millis(),
                    "hits": hits,
                }));
            }
        }
    }
    let store = st.db.lock().unwrap();
    let result = match store.search_query(&parsed, Some(limit)) {
        Ok(r) => json!({
            "ok": true,
            "query": q.q,
            "engine": "sql",
            "count": r.hits.len(),
            "total": r.total,
            "took_ms": t.elapsed().as_millis(),
            "hits": r.hits,
        }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    };
    Json(result)
}

async fn stats(State(st): State<AppState>) -> Json<Value> {
    let store = st.db.lock().unwrap();
    let (files, dirs) = store.counts().unwrap_or((0, 0));
    let vols: Vec<String> = usn::list_volumes()
        .iter()
        .map(|v| format!("{}: ({}) [{}]", v.drive, v.label.trim(), v.fs))
        .collect();
    let mem = st.mem.read().unwrap().as_ref().map(|m| {
        json!({ "entries": m.len(), "bytes": m.memory_bytes() })
    });
    Json(json!({
        "ok": true,
        "files": files,
        "dirs": dirs,
        "entries": files + dirs,
        "db": store.db_path().to_string_lossy(),
        "volumes": vols,
        "mem_index": mem,
    }))
}

async fn rescan(State(st): State<AppState>) -> Json<Value> {
    let db = st.db.clone();
    let mem_slot = st.mem.clone();
    let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<crate::BuildReport> {
        let mut store = db.lock().unwrap();
        let vols = usn::list_volumes();
        let report = indexer::build(&mut store, &vols, Method::Auto)?;
        // Reload the in-memory engine so it reflects the new index.
        let mem = load_mem(&store);
        *mem_slot.write().unwrap() = mem.map(Arc::new);
        Ok(report)
    })
    .await;
    match outcome {
        Ok(Ok(report)) => Json(json!({ "ok": true, "report": report })),
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
<h1>⚡ <span>File-Engine-Rust</span> — 毫秒级全盘文件名搜索</h1>
<input id="q" placeholder="输入文件名（支持 * ? 通配符），回车或输入即搜…" autofocus>
<div class="row"><label><input type="checkbox" id="p"> 全路径匹配</label>
<span id="meta" style="margin-left:1em"></span></div>
<ul id="res"></ul>
<script>
const q=document.getElementById('q'),res=document.getElementById('res'),meta=document.getElementById('meta');
let t;
q.addEventListener('input',()=>{clearTimeout(t);t=setTimeout(run,120);});
async function run(){
  const v=q.value;
  if(!v){res.innerHTML='';meta.textContent='';return;}
  const url='/api/search?q='+encodeURIComponent(v)+'&limit=200'+(document.getElementById('p').checked?'&path=true':'');
  const j=await (await fetch(url)).json();
  res.innerHTML='';meta.textContent=(j.total??0)+' 条命中 · '+(j.took_ms??0)+' ms';
  for(const h of (j.hits||[])){
    const li=document.createElement('li');
    if(h.is_dir)li.className='dir';
    li.textContent=h.path;
    res.appendChild(li);
  }
}
</script>
</body></html>
"#;
