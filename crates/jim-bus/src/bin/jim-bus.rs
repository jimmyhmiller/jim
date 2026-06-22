//! `jim-bus` daemon entry point (standalone binary).
//!
//! In production the daemon is normally hosted by the `jim` or `jimctl`
//! binary via self-exec (`<exe> bus-daemon`), so this standalone binary
//! exists mainly for tests (pointed at by `JIM_BUS_BIN`) and manual
//! debugging. Set `JIM_BUS_FOREGROUND=1` to skip daemonization;
//! `JIM_BUS_LOG=/path` routes stderr to a file when daemonized.

fn main() {
    jim_bus::daemon::daemonize_if_requested();
    if let Err(e) = jim_bus::run() {
        eprintln!("[jim-bus] fatal: {e}");
        std::process::exit(1);
    }
}
