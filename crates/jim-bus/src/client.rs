//! Client side of the jim bus: spawn-on-demand, publish, and subscribe.
//!
//! Mirrors how the terminal attaches to `jim_daemon`: try to connect; if
//! the socket isn't there, self-exec the daemon and poll for it to appear.
//! No central registry — the socket path *is* the rendezvous.
//!
//! Three entry points cover every caller:
//!  * [`publish_oneshot`] — fire-and-forget publish (jimctl, GUI publisher).
//!  * [`fetch_retained`] — one connect that reads the retained replay and
//!    returns it (the agent roster lookup).
//!  * [`BusHandle`] — a long-lived background subscriber + publisher with
//!    automatic reconnect, for the GUI and the `jimctl` follow loops.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::proto::{decode, encode, BusFrame, BusMessage, ClientFrame, Role};

const BOOT_TIMEOUT: Duration = Duration::from_secs(3);

/// Connect to the bus, spawning the daemon if it isn't running yet.
pub fn connect_or_spawn() -> std::io::Result<UnixStream> {
    let socket = crate::socket_path()
        .ok_or_else(|| std::io::Error::other("HOME not set; cannot locate bus socket"))?;
    match UnixStream::connect(&socket) {
        Ok(s) => Ok(s),
        Err(_) => {
            spawn_daemon()?;
            wait_for_socket(&socket, BOOT_TIMEOUT)
        }
    }
}

/// Self-exec the daemon: `<exe> bus-daemon`. Both `jim` and `jimctl`
/// dispatch on `bus-daemon` in `main`; `JIM_BUS_BIN` overrides the binary
/// for tests. The daemon double-forks itself, so the immediate child exits
/// quickly — we `wait()` it to avoid a zombie.
fn spawn_daemon() -> std::io::Result<()> {
    let bin = match std::env::var_os("JIM_BUS_BIN") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe()?,
    };
    let mut child = Command::new(bin).arg(crate::DAEMON_ARG).spawn()?;
    let _ = child.wait();
    Ok(())
}

fn wait_for_socket(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(5);
    loop {
        if path.exists() {
            if let Ok(sock) = UnixStream::connect(path) {
                return Ok(sock);
            }
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::other(format!(
                "bus daemon socket never appeared: {}",
                path.display()
            )));
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(50));
    }
}

/// Fire-and-forget publish: connect (spawning the daemon if needed), send
/// Hello+Publish, drop. The daemon parses and broadcasts even after the
/// publisher hangs up.
pub fn publish_oneshot(msg: &BusMessage) -> std::io::Result<()> {
    let mut s = connect_or_spawn()?;
    s.write_all(&encode(&ClientFrame::Hello { role: Role::Publisher }).map_err(std::io::Error::other)?)?;
    s.write_all(&encode(&ClientFrame::Publish(msg.clone())).map_err(std::io::Error::other)?)?;
    let _ = s.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// Connect once, read the full retained replay, and return it. Used for a
/// point-in-time snapshot (the agent roster) where a streaming subscriber
/// would be overkill. Spawns the daemon if absent.
pub fn fetch_retained() -> std::io::Result<Vec<BusMessage>> {
    let mut s = connect_or_spawn()?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(
        &encode(&ClientFrame::Hello {
            role: Role::Subscriber { since_seq: None },
        })
        .map_err(std::io::Error::other)?,
    )?;

    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 16 * 1024];
    let mut out = Vec::new();
    let mut started = false;
    loop {
        match decode::<BusFrame>(&buf) {
            Ok(Some((f, consumed))) => {
                buf.drain(0..consumed);
                match f {
                    BusFrame::ReplayStart => started = true,
                    BusFrame::Message { msg, .. } => out.push(msg),
                    BusFrame::ReplayEnd if started => return Ok(out),
                    BusFrame::ReplayEnd => return Ok(out),
                    BusFrame::Lagged { .. } => {}
                }
                continue;
            }
            Ok(None) => {}
            Err(e) => return Err(std::io::Error::other(format!("decode: {e}"))),
        }
        match s.read(&mut tmp) {
            Ok(0) => return Ok(out),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(out)
            }
            Err(e) => return Err(e),
        }
    }
}

/// One item handed up from the subscriber thread.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// A delivered message (retained-replay or live). `retain` on the
    /// message says whether it updates the retained mirror.
    Message(BusMessage),
    /// The retained-replay segment finished; everything after is live.
    /// Emitted once per (re)connect.
    ReplayEnd,
}

/// A long-lived bus connection: a background subscriber feeding an inbound
/// channel and a background publisher draining an outbound one. Both
/// reconnect (and re-spawn the daemon) forever, so a daemon restart looks
/// like a brief hiccup. `Send + Sync` (the `Receiver` is behind a `Mutex`)
/// so it can live in a Bevy `Resource`.
pub struct BusHandle {
    publish_tx: Sender<BusMessage>,
    inbound: Mutex<Receiver<Inbound>>,
    stop: Arc<AtomicBool>,
}

impl BusHandle {
    pub fn spawn() -> Self {
        let (in_tx, in_rx) = mpsc::channel::<Inbound>();
        let (pub_tx, pub_rx) = mpsc::channel::<BusMessage>();
        let stop = Arc::new(AtomicBool::new(false));

        let stop_sub = stop.clone();
        let _ = thread::Builder::new()
            .name("jim-bus-subscriber".into())
            .spawn(move || subscriber_loop(in_tx, stop_sub));

        let stop_pub = stop.clone();
        let _ = thread::Builder::new()
            .name("jim-bus-publisher".into())
            .spawn(move || publisher_loop(pub_rx, stop_pub));

        Self {
            publish_tx: pub_tx,
            inbound: Mutex::new(in_rx),
            stop,
        }
    }

    /// Queue a message for publication (handed to the publisher thread).
    pub fn publish(&self, msg: BusMessage) {
        let _ = self.publish_tx.send(msg);
    }

    /// Drain everything the subscriber has delivered since the last call.
    pub fn drain(&self) -> Vec<Inbound> {
        let mut out = Vec::new();
        if let Ok(rx) = self.inbound.lock() {
            while let Ok(item) = rx.try_recv() {
                out.push(item);
            }
        }
        out
    }
}

impl Drop for BusHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Forever-reconnecting subscriber. Tracks the highest seq delivered and
/// resumes from it after a drop.
fn subscriber_loop(tx: Sender<Inbound>, stop: Arc<AtomicBool>) {
    let mut last_seq: Option<u64> = None;
    let mut backoff = Duration::from_millis(100);
    while !stop.load(Ordering::Relaxed) {
        match subscriber_session(&tx, &stop, &mut last_seq) {
            Ok(()) => return, // stop flag flipped
            Err(_) => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(3));
            }
        }
    }
}

fn subscriber_session(
    tx: &Sender<Inbound>,
    stop: &Arc<AtomicBool>,
    last_seq: &mut Option<u64>,
) -> std::io::Result<()> {
    let mut s = connect_or_spawn()?;
    s.set_read_timeout(Some(Duration::from_millis(250)))?;
    let hello = encode(&ClientFrame::Hello {
        role: Role::Subscriber {
            since_seq: *last_seq,
        },
    })
    .map_err(std::io::Error::other)?;
    s.write_all(&hello)?;

    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 16 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        loop {
            match decode::<BusFrame>(&buf) {
                Ok(Some((f, consumed))) => {
                    buf.drain(0..consumed);
                    match f {
                        BusFrame::Message { seq, msg } => {
                            *last_seq = Some(seq);
                            if tx.send(Inbound::Message(msg)).is_err() {
                                stop.store(true, Ordering::SeqCst);
                                return Ok(());
                            }
                        }
                        BusFrame::ReplayEnd => {
                            let _ = tx.send(Inbound::ReplayEnd);
                        }
                        BusFrame::ReplayStart | BusFrame::Lagged { .. } => {}
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(std::io::Error::other(format!("decode: {e}"))),
            }
        }
        match s.read(&mut tmp) {
            Ok(0) => return Err(std::io::Error::other("bus closed")),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
    }
}

/// Forever-reconnecting publisher. Holds one Publisher connection and
/// writes queued messages to it; on any write failure it reconnects
/// (re-spawning the daemon if needed) and retries.
fn publisher_loop(rx: Receiver<BusMessage>, stop: Arc<AtomicBool>) {
    let mut conn: Option<UnixStream> = None;
    while !stop.load(Ordering::Relaxed) {
        // Block for the next message, but wake periodically to honor stop.
        let msg = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        // Two attempts: reuse the live connection, else reconnect once.
        for attempt in 0..2 {
            if conn.is_none() {
                match open_publisher() {
                    Ok(s) => conn = Some(s),
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                }
            }
            let s = conn.as_mut().unwrap();
            match encode(&ClientFrame::Publish(msg.clone()))
                .map_err(std::io::Error::other)
                .and_then(|b| s.write_all(&b))
            {
                Ok(()) => break,
                Err(_) => {
                    conn = None; // drop & retry on the next attempt
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }
    }
}

fn open_publisher() -> std::io::Result<UnixStream> {
    let mut s = connect_or_spawn()?;
    s.write_all(&encode(&ClientFrame::Hello { role: Role::Publisher }).map_err(std::io::Error::other)?)?;
    Ok(s)
}
