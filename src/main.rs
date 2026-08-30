use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

use file_engine_rust::indexer::{self, Method};
use file_engine_rust::mem::{MemIndex, dump_path};
use file_engine_rust::query::Query;
use file_engine_rust::usn;

#[derive(Parser)]
#[command(name = "fer", version, about = "Everything-grade instant file search, rewritten in Rust")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
    /// Index base path (default: %LOCALAPPDATA%\file-engine-rust\index.db;
    /// the actual index is the .feridx dump next to it)
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
    /// (Re)build the index (writes the .feridx dump)
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
        /// Flush the in-memory index back to the dump at least this often
        #[arg(long, default_value_t = 60)]
        flush_secs: u64,
    },
    /// Index statistics
    Stats,
    /// Find duplicate files (same size + identical content)
    Dupes {
        /// Only consider files at least this big (e.g. 1kb, 10mb)
        #[arg(long, default_value = "1kb")]
        min_size: String,
        /// Only consider files whose name contains this substring
        #[arg(long)]
        name: Option<String>,
        /// Maximum duplicate groups to report
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
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

/// Load the dump; if missing, point the user at the one-time index build.
fn load_index(db: &Path) -> Result<MemIndex> {
    let dump = dump_path(db);
    if !dump.exists() {
        anyhow::bail!(
            "no index at {} — run `fer index` once (builds it in ~1 minute)",
            dump.display()
        );
    }
    MemIndex::load_dump(&dump)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db = cli.db.clone().unwrap_or_else(default_db);
    match cli.command {
        Cmd::Volumes => {
            let vols = usn::list_volumes();
            let json_vols: Vec<serde_json::Value> = vols
                .iter()
                .map(|v| json!({ "drive": v.drive.to_string(), "fs": v.fs, "label": v.label }))
                .collect();
            if cli.json {
                print_json(json!({ "ok": true, "volumes": json_vols }))?;
            } else {
                for v in &vols {
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
            let (report, mem) = indexer::build(&vols, method)?;
            let t_dump = Instant::now();
            let dump = dump_path(&db);
            mem.save(&dump)?;
            eprintln!(
                "[dump] {} entries, {} MB written to {} in {} ms",
                mem.len(),
                mem.memory_bytes() / (1 << 20),
                dump.display(),
                t_dump.elapsed().as_millis()
            );
            if cli.json {
                print_json(json!({
                    "ok": true,
                    "report": report,
                    "dump": dump.display().to_string(),
                }))?;
            } else {
                println!(
                    "indexed {} files + {} dirs in {:.2}s (method: {}, skipped: {}, dump: {})",
                    report.files,
                    report.dirs,
                    report.elapsed_ms as f64 / 1000.0,
                    report.method,
                    report.skipped,
                    dump.display()
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
            let mem = load_index(&db)?;
            let t = Instant::now();
            let ids = mem.search(&q);
            let total = ids.len() as u64;
            let hits = mem.hits(&ids, limit);
            let took = t.elapsed().as_millis();
            if cli.json {
                print_json(json!({
                    "ok": true,
                    "query": query,
                    "engine": "mem",
                    "total": total,
                    "count": hits.len(),
                    "took_ms": took,
                    "hits": hits,
                }))?;
            } else if count_only {
                println!("{total}");
                eprintln!("{total} total results in {took} ms");
            } else {
                for h in &hits {
                    println!("{}", h.path);
                }
                eprintln!(
                    "{} results (total {total}) in {took} ms",
                    hits.len()
                );
            }
        }
        Cmd::Serve { addr } => {
            let mem = load_index(&db)?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(file_engine_rust::server::serve(&addr, mem, &db))?;
        }
        Cmd::Monitor {
            volume,
            interval_secs,
            flush_secs,
        } => {
            let mem = load_index(&db)?;
            let dump = dump_path(&db);
            file_engine_rust::monitor::run(
                mem,
                volume.to_ascii_uppercase(),
                dump,
                Duration::from_secs(interval_secs),
                Duration::from_secs(flush_secs),
            )?;
        }
        Cmd::Stats => {
            let mem = load_index(&db)?;
            let files = mem.file_count() as u64;
            let dirs = mem.dir_count() as u64;
            let dump = dump_path(&db);
            let dump_mb = std::fs::metadata(&dump).map(|m| m.len() / (1 << 20)).unwrap_or(0);
            if cli.json {
                print_json(json!({
                    "ok": true,
                    "dump": dump.display().to_string(),
                    "dump_mb": dump_mb,
                    "files": files,
                    "dirs": dirs,
                    "entries": files + dirs,
                }))?;
            } else {
                println!("dump:    {}", dump.display());
                println!("files:   {files}");
                println!("dirs:    {dirs}");
                println!("entries: {}", files + dirs);
                println!("size:    {dump_mb} MB");
            }
        }
        Cmd::Dupes { min_size, name, limit } => {
            let min = file_engine_rust::query::parse_bytes(&min_size)?;
            let mem = load_index(&db)?;
            let mut last_log = std::time::Instant::now();
            let report = file_engine_rust::dupes::find(
                &mem,
                min,
                name.as_deref(),
                limit,
                |n| {
                    if last_log.elapsed().as_millis() > 2000 {
                        eprintln!("[dupes] {n} files hashed ...");
                        last_log = std::time::Instant::now();
                    }
                },
            )?;
            if cli.json {
                print_json(json!({
                    "ok": true,
                    "groups": report.groups,
                    "wasted_bytes": report.wasted_bytes,
                    "files_hashed": report.files_hashed,
                    "skipped": report.skipped,
                }))?;
            } else {
                for g in &report.groups {
                    println!(
                        "{}  x{}  wasted {}",
                        fmt_bytes(g.size),
                        g.paths.len(),
                        fmt_bytes(g.size * (g.paths.len() as u64 - 1))
                    );
                    for p in &g.paths {
                        println!("    {p}");
                    }
                }
                println!(
                    "{} duplicate groups, {} wasted, {} files hashed ({} skipped)",
                    report.groups.len(),
                    fmt_bytes(report.wasted_bytes),
                    report.files_hashed,
                    report.skipped
                );
            }
        }
    }
    Ok(())
}

fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.2} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}
