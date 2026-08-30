//! HTTP API server (axum) with a minimal web UI.

use std::sync::{Arc, Mutex};

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
use crate::store::Store;
use crate::usn;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Store>>,
}

pub async fn serve(addr: &str, store: Store) -> Result<()> {
    let state = AppState {
        db: Arc::new(Mutex::new(store)),
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
    let limit = q.limit.map(|l| l.min(10_000));
    let result = {
        let store = st.db.lock().unwrap();
        match crate::query::Query::parse(&q.q) {
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
            Ok(query) => match store.search_query(&query, limit) {
                Ok(r) => json!({
                    "ok": true,
                    "query": q.q,
                    "count": r.hits.len(),
                    "total": r.total,
                    "took_ms": t.elapsed().as_millis(),
                    "hits": r.hits,
                }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            },
        }
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
    Json(json!({
        "ok": true,
        "files": files,
        "dirs": dirs,
        "entries": files + dirs,
        "db": store.db_path().to_string_lossy(),
        "volumes": vols,
    }))
}

async fn rescan(State(st): State<AppState>) -> Json<Value> {
    let db = st.db.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut store = db.lock().unwrap();
        let vols = usn::list_volumes();
        indexer::build(&mut store, &vols, Method::Auto)
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
