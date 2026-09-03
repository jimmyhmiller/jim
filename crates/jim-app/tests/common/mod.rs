//! Shared setup for integration tests that need a working daemon.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use jim_terminal::daemon_client::DaemonClient;
use jim_terminal::daemon_proto::ClientMessage;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

pub struct DaemonTestEnv {
    /// Held so the temp dir survives the test. Backing store for HOME +
    /// the daemon stderr log.
    pub home: tempfile::TempDir,
    /// Short path (under /tmp) where sockets / pid files live. Must be
    /// short to satisfy macOS's 104-char `SUN_LEN`.
    pub runtime_dir: PathBuf,
}

impl Drop for DaemonTestEnv {
    fn drop(&mut self) {
        // A production terminal daemon deliberately survives its client, but
        // an isolated test daemon belongs to this fixture. Stop every daemon
        // while its socket still exists; deleting the runtime directory first
        // strands a double-forked process with no remaining control path.
        let daemons = daemon_pids(&self.runtime_dir);
        for (session_id, _) in &daemons {
            if let Ok(mut client) = DaemonClient::reattach(*session_id, 1, 1) {
                client.send(&ClientMessage::Kill);
                client.try_flush();
            }
        }

        let graceful_deadline = Instant::now() + Duration::from_secs(2);
        wait_until_dead(&daemons, graceful_deadline);

        // A panic may have left a half-started daemon unable to complete the
        // protocol handshake. PID files live in this fixture's unique runtime
        // directory, so they identify only processes spawned by this test.
        signal_survivors(&daemons, Signal::SIGTERM);
        let term_deadline = Instant::now() + Duration::from_secs(1);
        wait_until_dead(&daemons, term_deadline);
        signal_survivors(&daemons, Signal::SIGKILL);

        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

fn daemon_pids(runtime_dir: &std::path::Path) -> Vec<(u64, Pid)> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("pid") {
                return None;
            }
            let session_id = path.file_stem()?.to_str()?.parse().ok()?;
            let pid = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
            Some((session_id, Pid::from_raw(pid)))
        })
        .collect()
}

fn is_alive(pid: Pid) -> bool {
    kill(pid, None).is_ok()
}

fn wait_until_dead(daemons: &[(u64, Pid)], deadline: Instant) {
    while Instant::now() < deadline && daemons.iter().any(|(_, pid)| is_alive(*pid)) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal_survivors(daemons: &[(u64, Pid)], signal: Signal) {
    for (_, pid) in daemons {
        if is_alive(*pid) {
            let _ = kill(*pid, signal);
        }
    }
}

/// Set up an isolated environment: HOME → tempdir, runtime dir →
/// short /tmp path, daemon binary path discovered from this test's own
/// `current_exe()`. Returns a guard whose drop cleans the runtime dir.
pub fn setup_isolated_daemon_env() -> DaemonTestEnv {
    let home = tempfile::Builder::new()
        .prefix("terminal-bevy-test-home-")
        .tempdir()
        .expect("tempdir");

    let test_exe = std::env::current_exe().expect("current_exe");
    let daemon_bin = test_exe
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("jim-daemon"))
        .expect("derive daemon binary path");
    assert!(
        daemon_bin.exists(),
        "jim-daemon binary not built at {}",
        daemon_bin.display()
    );

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let runtime_dir = PathBuf::from(format!(
        "/tmp/terminal-bevy-test-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&runtime_dir).expect("create runtime_dir");

    // SAFETY: tests using this helper must run single-threaded
    // (--test-threads=1). We mutate shared process env.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("TERMINAL_BEVY_DAEMON_BIN", &daemon_bin);
        std::env::set_var("TERMINAL_BEVY_RUNTIME_DIR", &runtime_dir);
        std::env::set_var("TERMINAL_DAEMON_LOG", home.path().join("daemon.log"));
    }

    DaemonTestEnv { home, runtime_dir }
}

/// Pick a session_id unlikely to collide with anything from a previous
/// crashed test run, even within the same runtime_dir.
pub fn random_session_id() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    900_000 + (nanos.wrapping_mul(pid) % 100_000)
}
