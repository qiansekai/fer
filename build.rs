use std::path::Path;

// `src/server.rs` embeds the built UI with `include_str!("../webui/dist/index.html")`.
// A missing build output would otherwise surface as a bare "couldn't read file"
// from rustc with no hint about how to produce it, so fail early and say what to do.
fn main() {
    let dist = Path::new("webui/dist/index.html");
    if !dist.exists() {
        panic!(
            "\n\nwebui/dist/index.html not found — src/server.rs embeds it with include_str!.\n\
             Build the UI first:\n\n    cd webui && npm install && npm run build\n\n\
             (Working directory was {})\n",
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into())
        );
    }
    // Rebuild the crate when the embedded UI or the sources behind it change.
    println!("cargo:rerun-if-changed=webui/dist/index.html");
    println!("cargo:rerun-if-changed=webui/src");
    println!("cargo:rerun-if-changed=webui/index.html");
}
