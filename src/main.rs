use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

use file_engine_rust::indexer::{self, Method};
use file_engine_rust::query::Query;
use file_engine_rust::store::Store;
use file_engine_rust::usn;

#[derive(Parser)]
#[command(name = "fer", version, about = "Everything-grade instant file search, rewritten in Rust")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
    /// SQLite index path (default: %LOCALAPPDATA%\file-engine-rust\index.db)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Machine-readable JSON output (stable for agents)
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// List fixed NTFS volumes
    Volumes,
    /// (Re)build the index
    Index {
        /// Comma-separated drive letters; empty = all fixed NTFS volumes
        #[arg(long, default_value = "")]
        volumes: String,
        /// auto | mft | usn | walk
        #[arg(long, default_value = "auto")]
        method: String,
    },
    /// Instant search in the query language (see README)
    Search {
        /// Query: plain substring, wildcard, or filters like
        /// "ext:rs size:>1mb dm:thisweek parent:D:\proj !temp"
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        count_only: bool,
    },
    /// Start the HTTP API + web UI
    Serve {
        #[arg(long, default_value = "127.0.0.1:9876")]
        addr: String,
    },
    /// Watch the USN journal and keep the index live (requires admin)
    Monitor {
        #[arg(long)]
        volume: char,
        #[arg(long, default_value_t = 5)]
        interval_secs: u64,
    },
    /// Index statistics
    Stats,
}

fn default_db() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("file-engine-rust").join("index.db")
}

fn print_json(value: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db = cli.db.clone().unwrap_or_else(default_db);
    match cli.command {
        Cmd::Volumes => {
            let vols: Vec<serde_json::Value> = usn::list_volumes()
                .iter()
                .map(|v| json!({ "drive": v.drive.to_string(), "fs": v.fs, "label": v.label }))
                .collect();
            if cli.json {
                print_json(json!({ "ok": true, "volumes": vols }))?;
            } else {
                for v in usn::list_volumes() {
                    println!("{}:  [{:5}]  {}", v.drive, v.fs, v.label);
                }
            }
        }
        Cmd::Index { volumes, method } => {
            let method = Method::parse(&method)?;
            let vols = indexer::resolve_volumes(&volumes);
            if vols.is_empty() {
                anyhow::bail!("no fixed NTFS volumes selected");
            }
            for v in &vols {
                eprintln!("target: {}: ({}) [{}]", v.drive, v.label.trim(), v.fs);
            }
            let mut store = Store::open(&db)?;
            let report = indexer::build(&mut store, &vols, method)?;
            if cli.json {
                print_json(json!({ "ok": true, "report": report, "db": db.display().to_string() }))?;
            } else {
                println!(
                    "indexed {} files + {} dirs in {:.2}s (method: {}, skipped: {}, db: {})",
                    report.files,
                    report.dirs,
                    report.elapsed_ms as f64 / 1000.0,
                    report.method,
                    report.skipped,
                    db.display()
                );
            }
        }
        Cmd::Search { query, limit, count_only } => {
            let q = match Query::parse(&query) {
                Ok(q) => q,
                Err(e) => {
                    if cli.json {
                        print_json(json!({ "ok": false, "error": e.to_string() }))?;
                        std::process::exit(2);
                    }
                    return Err(e);
                }
            };
            let store = Store::open(&db)?;
            let t = Instant::now();
            let r = store.search_query(&q, Some(limit))?;
            let took = t.elapsed().as_millis();
            if cli.json {
                print_json(json!({
                    "ok": true,
                    "query": query,
                    "total": r.total,
                    "count": r.hits.len(),
                    "took_ms": took,
                    "hits": r.hits,
                }))?;
            } else if count_only {
                println!("{}", r.total);
                eprintln!("{} total results in {} ms", r.total, took);
            } else {
                for h in &r.hits {
                    println!("{}", h.path);
                }
                eprintln!(
                    "{} results (total {}) in {} ms",
                    r.hits.len(),
                    r.total,
                    took
                );
            }
        }
        Cmd::Serve { addr } => {
            let store = Store::open(&db)?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(file_engine_rust::server::serve(&addr, store))?;
        }
        Cmd::Monitor {
            volume,
            interval_secs,
        } => {
            let mut store = Store::open(&db)?;
            file_engine_rust::monitor::run(
                &mut store,
                volume.to_ascii_uppercase(),
                Duration::from_secs(interval_secs),
            )?;
        }
        Cmd::Stats => {
            let store = Store::open(&db)?;
            let (files, dirs) = store.counts()?;
            if cli.json {
                print_json(json!({
                    "ok": true,
                    "db": db.display().to_string(),
                    "files": files,
                    "dirs": dirs,
                    "entries": files + dirs,
                }))?;
            } else {
                println!("db:      {}", db.display());
                println!("files:   {files}");
                println!("dirs:    {dirs}");
                println!("entries: {}", files + dirs);
            }
        }
    }
    Ok(())
}
