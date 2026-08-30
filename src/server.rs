//! HTTP API server (axum) with a minimal web UI. Queries run entirely
//! against the in-memory engine; `/api/rescan` rebuilds from the volumes and
//! refreshes the dump + the live engine.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

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
    match outcome {
        Ok((total, hits)) => Json(json!({
            "ok": true,
            "query": qq,
            "engine": "mem",
            "count": hits.len(),
            "total": total,
            "took_ms": t.elapsed().as_millis(),
            "hits": hits,
        })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn stats(State(st): State<AppState>) -> Json<Value> {
    let mem = st.mem.read().unwrap().clone();
    let files = mem.file_count() as u64;
    let dirs = mem.dir_count() as u64;
    let dump = dump_path(&st.db);
    let dump_mb = std::fs::metadata(&dump).map(|m| m.len() / (1 << 20)).unwrap_or(0);
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
