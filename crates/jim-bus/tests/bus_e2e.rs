//! End-to-end test for the standalone bus daemon — proving the two
//! properties that justify this crate's existence:
//!
//!   1. publish → subscribe works with **no GUI** in the picture (the
//!      daemon is the hub), and
//!   2. retained state **survives a daemon restart** (it's persisted to
//!      disk), which the old in-GUI bus never managed.
//!
//! One `#[test]` runs both phases sequentially because they share the
//! process-global `JIM_BUS_*` env (cargo runs separate test fns in
//! parallel, which would race on the socket path).

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use jim_bus::client;
use jim_bus::proto::BusMessage;

const DAEMON_BIN: &str = env!("CARGO_BIN_EXE_jim-bus");

fn spawn_daemon() -> Child {
    Command::new(DAEMON_BIN)
        .spawn()
        .expect("spawn jim-bus daemon")
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("daemon socket never came up: {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn publish(topic: &str, payload: serde_json::Value, retain: bool, sender: &str) {
    client::publish_oneshot(&BusMessage {
        project: None,
        topic: topic.to_string(),
        payload_json: payload.to_string(),
        sender: sender.to_string(),
        retain,
    })
    .expect("publish");
}

#[test]
fn bus_works_headless_and_retains_across_restart() {
    let dir = std::env::temp_dir().join(format!("jim-bus-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("bus.sock");
    let retained = dir.join("retained.json");
    let pid = dir.join("bus.pid");

    // SAFETY: set before any threads are spawned in this test.
    unsafe {
        std::env::set_var("JIM_BUS_SOCK", &sock);
        std::env::set_var("JIM_BUS_RETAINED", &retained);
        std::env::set_var("JIM_BUS_PID", &pid);
        std::env::set_var("JIM_BUS_BIN", DAEMON_BIN);
        // Stay in the foreground so the child we spawn *is* the daemon and
        // we can kill it deterministically (no double-fork detach).
        std::env::set_var("JIM_BUS_FOREGROUND", "1");
    }

    // ---- Phase 1: pub/sub with no GUI ----
    let mut daemon = spawn_daemon();
    wait_for_socket(&sock);

    {
        let sub = client::BusHandle::spawn();
        // Let the subscriber connect + finish its (empty) replay.
        std::thread::sleep(Duration::from_millis(300));

        publish("agent.all", serde_json::json!({"text": "hello"}), false, "tester");

        let mut got = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            for item in sub.drain() {
                if let client::Inbound::Message(m) = item {
                    if m.topic == "agent.all" {
                        got = Some(m);
                    }
                }
            }
            if got.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let m = got.expect("subscriber should receive the published message");
        assert_eq!(m.sender, "tester");
        assert!(m.payload_json.contains("hello"));
    } // drop the subscriber

    // ---- Phase 2: retained state survives a daemon restart ----
    publish("demo.state", serde_json::json!({"v": 1}), true, "tester");
    // The daemon persists synchronously on accept; give the socket write a
    // beat to be processed, then confirm the store hit disk.
    std::thread::sleep(Duration::from_millis(300));
    assert!(retained.exists(), "retained store should be written to disk");

    daemon.kill().unwrap();
    let _ = daemon.wait();
    // Wait for the socket to disappear so the restart binds cleanly.
    let gone = Instant::now() + Duration::from_secs(3);
    while sock.exists() && Instant::now() < gone {
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut daemon2 = spawn_daemon();
    wait_for_socket(&sock);

    let retained_msgs = client::fetch_retained().expect("fetch retained");
    let found = retained_msgs
        .iter()
        .find(|m| m.topic == "demo.state")
        .expect("retained topic should survive the restart");
    assert!(found.payload_json.contains("\"v\":1"));
    assert!(found.retain);

    daemon2.kill().unwrap();
    let _ = daemon2.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
