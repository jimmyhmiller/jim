//! `jim-lsp` — headless rust-analyzer sidecar daemon.
//!
//! Spawned on demand (by `jimctl lsp` or the LSP explorer widget) when no
//! daemon for a given workspace root is alive. Detaches into the background
//! and serves structural queries over a Unix socket until killed.
//!
//! Usage:
//!     jim-lsp <path-inside-workspace>
//!
//! The argument may be any path inside the target Cargo workspace; the daemon
//! resolves it up to the workspace root and keys itself on that. Internal —
//! `jimctl`/the widget are the only intended callers.

use std::path::PathBuf;

fn main() {
    let start = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: jim-lsp <path-inside-workspace>");
            std::process::exit(2);
        }
    };
    jim_lsp::daemon::run(&start);
}
