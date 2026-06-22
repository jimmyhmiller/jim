//! The jim message bus — a standalone daemon that hosts the
//! widget↔widget / agent (`agent.*`) message bus so it survives a GUI
//! restart, exactly like `jim_daemon` hosts the terminal so the shell
//! survives one.
//!
//! Before this crate the bus lived *inside* the Bevy app: external
//! processes published to the GUI's `~/.jim/socket`, the GUI held the
//! retained store + agent roster in memory and wrote the tail log. Close
//! the GUI and the bus — and every cross-session agent on it — went dark.
//!
//! Now the daemon owns the socket, the retained store (persisted to disk),
//! the resume ring, and the roster sweep. The GUI is just another
//! client: it subscribes to deliver messages to widget panes and
//! publishes their emits. `jimctl` (and the MCP bridge) likewise connect
//! as clients. Either binary can *host* the daemon by self-exec
//! (`<exe> bus-daemon`); whoever needs the bus first spawns it.
//!
//! Path resolution lives here so the daemon and every client agree on
//! where the socket is.

pub mod client;
pub mod daemon;
pub mod proto;

use std::path::PathBuf;

/// The subcommand token a host binary recognizes to *become* the bus
/// daemon. `ensure_running` self-execs `<current_exe> bus-daemon`; both
/// `jim` (the GUI) and `jimctl` dispatch on this in their `main`. Tests
/// override the spawned binary with `JIM_BUS_BIN`.
pub const DAEMON_ARG: &str = "bus-daemon";

fn home_path(file: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".jim");
    p.push(file);
    Some(p)
}

/// Where the bus listens. `$HOME/.jim/bus.sock`, short enough on every
/// system we ship to to stay inside macOS's 104-byte `sun_path` cap.
/// `JIM_BUS_SOCK` overrides it (tests use a temp dir).
pub fn socket_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("JIM_BUS_SOCK") {
        return Some(PathBuf::from(p));
    }
    home_path("bus.sock")
}

/// PID file the daemon writes on startup, for liveness checks / debugging.
pub fn pid_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("JIM_BUS_PID") {
        return Some(PathBuf::from(p));
    }
    home_path("bus.pid")
}

/// Durable JSON snapshot of the retained store. Reloaded on daemon start
/// so retained topics (the agent roster, every widget's retained state)
/// survive a daemon restart — the property the in-GUI bus never had.
/// `JIM_BUS_RETAINED` overrides it.
pub fn retained_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("JIM_BUS_RETAINED") {
        return Some(PathBuf::from(p));
    }
    home_path("bus-retained.json")
}

/// Run the daemon to completion (the `bus-daemon` entry). Resolves all
/// paths from the environment/defaults. Never returns under normal
/// operation.
pub fn run() -> std::io::Result<()> {
    let socket = socket_path()
        .ok_or_else(|| std::io::Error::other("HOME not set; cannot locate bus socket"))?;
    let retained = retained_path()
        .ok_or_else(|| std::io::Error::other("HOME not set; cannot locate retained store"))?;
    let pid = pid_path()
        .ok_or_else(|| std::io::Error::other("HOME not set; cannot locate pid file"))?;
    daemon::run(socket, retained, pid)
}
