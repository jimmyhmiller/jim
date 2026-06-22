//! The jim bus daemon.
//!
//! Single-threaded `poll(2)` loop (same shape as `jim_daemon` /
//! `claude_bus`). One listener socket; each accepted connection is either
//! a publisher (writes `Publish` frames, never reads) or a subscriber
//! (after `Hello`, receives the retained replay then a live stream of
//! `Message` frames).
//!
//! What used to live in the GUI now lives here:
//!  * the **retained store** — `(project, topic) → latest value` — the
//!    backbone of the agent roster (`agent.hello.*`) and every widget's
//!    retained state. Persisted to `bus-retained.json`, so it survives a
//!    daemon restart (the in-GUI bus lost it on every restart).
//!  * the **resume ring** — recent live messages, so a subscriber that
//!    briefly disconnects can resume by `since_seq` without a full resync.
//!  * the **roster sweep** — periodically tombstones `agent.hello.*`
//!    entries whose process is gone or whose heartbeat went stale.

#![allow(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::proto::{decode, encode, BusFrame, BusMessage, ClientFrame, Role};

/// Cap on retained live messages kept for `since_seq` resume. Reconnects
/// are quick, so a modest ring covers the gap; anything older forces a
/// full retained resync (a `Lagged` marker).
const RING_CAPACITY: usize = 2048;

/// Cap on a single subscriber's outbound buffer before we drop it. A
/// subscriber that can't keep up shouldn't balloon daemon memory.
const MAX_SEND_BUF: usize = 8 * 1024 * 1024;

/// Cap on a publisher's accumulated parse buffer.
const MAX_RECV_BUF: usize = 4 * 1024 * 1024;

/// How often to run the agent-roster staleness sweep.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// An `agent.hello.<id>` entry is stale once its heartbeat is this old.
/// Matches the GUI's old `ROSTER_STALE_SECS`.
const ROSTER_STALE_SECS: u64 = 35;

/// One retained `(project, topic)` value.
#[derive(Clone)]
struct RetainedVal {
    seq: u64,
    payload_json: String,
    sender: String,
}

/// One recent live message kept for resume.
struct RingEntry {
    seq: u64,
    /// Pre-encoded `BusFrame::Message` bytes — encode once, ship to many.
    frame: Vec<u8>,
}

enum ConnState {
    /// Hello not yet received.
    Anonymous { recv_buf: Vec<u8> },
    Publisher { recv_buf: Vec<u8> },
    Subscriber { send_buf: Vec<u8> },
}

struct Conn {
    stream: UnixStream,
    state: ConnState,
}

impl Conn {
    fn fd(&self) -> i32 {
        self.stream.as_raw_fd()
    }
    fn wants_pollout(&self) -> bool {
        matches!(&self.state, ConnState::Subscriber { send_buf } if !send_buf.is_empty())
    }
}

/// On-disk shape of one retained entry. Payload is stored as real JSON (not
/// a quoted string) so the file is human-readable / hand-editable.
#[derive(Serialize, Deserialize)]
struct PersistEntry {
    project: Option<u64>,
    topic: String,
    sender: String,
    payload: serde_json::Value,
}

pub struct Daemon {
    listener: UnixListener,
    conns: Vec<Conn>,
    /// Monotonic message id. Resets to 0 on restart; resume across a
    /// restart degrades to a full retained resync, which is correct.
    next_seq: u64,
    /// `(project, topic) → latest retained value`.
    retained: HashMap<(Option<u64>, String), RetainedVal>,
    /// Recent live messages for `since_seq` resume.
    ring: VecDeque<RingEntry>,
    retained_path: PathBuf,
    last_sweep: Instant,
}

impl Daemon {
    pub fn new(socket: &Path, retained_path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Stale socket from a dead predecessor — remove so bind() succeeds.
        // A *live* predecessor still holds the listener; bind() then fails
        // with EADDRINUSE and we bail (only one daemon at a time).
        if socket.exists() {
            let _ = std::fs::remove_file(socket);
        }
        let listener = UnixListener::bind(socket)?;
        listener.set_nonblocking(true)?;

        let retained = load_retained(retained_path);
        // Seed next_seq past anything we reloaded so replayed retained
        // entries never collide with fresh live seqs.
        let next_seq = retained.values().map(|v| v.seq).max().map(|s| s + 1).unwrap_or(0);

        Ok(Self {
            listener,
            conns: Vec::new(),
            next_seq,
            retained,
            ring: VecDeque::with_capacity(RING_CAPACITY),
            retained_path: retained_path.to_path_buf(),
            last_sweep: Instant::now(),
        })
    }

    pub fn run(mut self) -> std::io::Result<()> {
        let listener_fd = self.listener.as_raw_fd();
        loop {
            let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(1 + self.conns.len());
            pollfds.push(libc::pollfd {
                fd: listener_fd,
                events: libc::POLLIN,
                revents: 0,
            });
            for c in &self.conns {
                let mut events = libc::POLLIN;
                if c.wants_pollout() {
                    events |= libc::POLLOUT;
                }
                pollfds.push(libc::pollfd {
                    fd: c.fd(),
                    events,
                    revents: 0,
                });
            }

            // 1s timeout so the roster sweep runs even with no socket
            // traffic.
            let ret =
                unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 1000) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    std::thread::sleep(Duration::from_millis(10));
                }
                continue;
            }

            if pollfds[0].revents & libc::POLLIN != 0 {
                loop {
                    match self.listener.accept() {
                        Ok((s, _)) => {
                            let _ = s.set_nonblocking(true);
                            self.conns.push(Conn {
                                stream: s,
                                state: ConnState::Anonymous {
                                    recv_buf: Vec::new(),
                                },
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
            }

            let mut to_drop: Vec<usize> = Vec::new();
            for i in 0..self.conns.len() {
                let pf_idx = i + 1;
                if pf_idx >= pollfds.len() {
                    continue; // accepted this tick; serviced next loop
                }
                let revents = pollfds[pf_idx].revents;
                if revents & libc::POLLOUT != 0 && self.flush_subscriber(i).is_err() {
                    to_drop.push(i);
                    continue;
                }
                if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
                    && self.handle_readable(i).is_err()
                {
                    to_drop.push(i);
                    continue;
                }
            }

            if !to_drop.is_empty() {
                to_drop.sort_unstable();
                to_drop.dedup();
                for i in to_drop.into_iter().rev() {
                    self.conns.remove(i);
                }
            }

            if self.last_sweep.elapsed() >= SWEEP_INTERVAL {
                self.last_sweep = Instant::now();
                self.sweep_roster();
            }
        }
    }

    // The frame-parse loop matches on conn state each pass on purpose: a
    // dispatched Hello mutates the state mid-loop, and the second arm needs
    // a `&mut` drain — a `while let` can't express that cleanly.
    #[allow(clippy::while_let_loop)]
    fn handle_readable(&mut self, i: usize) -> Result<(), ()> {
        let mut tmp = [0u8; 16 * 1024];
        let mut peer_closed = false;
        loop {
            match self.conns[i].stream.read(&mut tmp) {
                Ok(0) => {
                    peer_closed = true;
                    break;
                }
                Ok(n) => match &mut self.conns[i].state {
                    ConnState::Anonymous { recv_buf } | ConnState::Publisher { recv_buf } => {
                        if recv_buf.len() + n > MAX_RECV_BUF {
                            return Err(());
                        }
                        recv_buf.extend_from_slice(&tmp[..n]);
                    }
                    // Subscribers shouldn't write post-Hello; ignore.
                    ConnState::Subscriber { .. } => {}
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(()),
            }
        }

        loop {
            let (consumed, frame) = match &self.conns[i].state {
                ConnState::Anonymous { recv_buf } | ConnState::Publisher { recv_buf } => {
                    match decode::<ClientFrame>(recv_buf) {
                        Ok(Some((f, n))) => (n, f),
                        Ok(None) => break,
                        Err(_) => return Err(()),
                    }
                }
                ConnState::Subscriber { .. } => break,
            };
            match &mut self.conns[i].state {
                ConnState::Anonymous { recv_buf } | ConnState::Publisher { recv_buf } => {
                    recv_buf.drain(0..consumed);
                }
                ConnState::Subscriber { .. } => unreachable!(),
            }
            self.dispatch_frame(i, frame)?;
        }
        if peer_closed {
            Err(())
        } else {
            Ok(())
        }
    }

    fn dispatch_frame(&mut self, i: usize, frame: ClientFrame) -> Result<(), ()> {
        match frame {
            ClientFrame::Hello { role } => {
                if !matches!(self.conns[i].state, ConnState::Anonymous { .. }) {
                    return Err(()); // double Hello
                }
                match role {
                    Role::Publisher => {
                        // Preserve any bytes buffered past the Hello — the
                        // first Publish frame(s).
                        let recv_buf = match std::mem::replace(
                            &mut self.conns[i].state,
                            ConnState::Anonymous {
                                recv_buf: Vec::new(),
                            },
                        ) {
                            ConnState::Anonymous { recv_buf } => recv_buf,
                            _ => Vec::new(),
                        };
                        self.conns[i].state = ConnState::Publisher { recv_buf };
                    }
                    Role::Subscriber { since_seq } => {
                        let mut send_buf = Vec::new();
                        self.seed_subscriber(&mut send_buf, since_seq);
                        self.conns[i].state = ConnState::Subscriber { send_buf };
                    }
                }
            }
            ClientFrame::Publish(msg) => {
                if !matches!(self.conns[i].state, ConnState::Publisher { .. }) {
                    return Err(());
                }
                self.accept_publish(msg);
            }
        }
        Ok(())
    }

    /// Build a fresh subscriber's initial send_buf. Retained replay is
    /// small (roster + a handful of topics), so we seed it all at once
    /// rather than paginating across poll ticks.
    fn seed_subscriber(&self, send_buf: &mut Vec<u8>, since_seq: Option<u64>) {
        let oldest_ring = self.ring.front().map(|e| e.seq);

        // Resume path: caller already has the retained state and only
        // missed live messages after `n`. If the ring still covers `n`,
        // replay just that tail — no `ReplayStart`/retained resync.
        if let Some(n) = since_seq {
            let covered = oldest_ring.map(|o| n + 1 >= o).unwrap_or(n + 1 >= self.next_seq);
            if covered {
                for e in &self.ring {
                    if e.seq > n && send_buf.len() < MAX_SEND_BUF {
                        send_buf.extend_from_slice(&e.frame);
                    }
                }
                return;
            }
            // Aged out — fall through to a full retained resync, prefixed
            // with a Lagged marker so the client knows it skipped a gap.
            if let Ok(b) = encode(&BusFrame::Lagged {
                requested: n,
                replay_from: oldest_ring.unwrap_or(0),
            }) {
                send_buf.extend_from_slice(&b);
            }
        }

        // Fresh subscribe (or lagged resume): replay the whole retained
        // store bracketed by ReplayStart/ReplayEnd, then live follows.
        if let Ok(b) = encode(&BusFrame::ReplayStart) {
            send_buf.extend_from_slice(&b);
        }
        for ((project, topic), val) in &self.retained {
            let frame = BusFrame::Message {
                seq: val.seq,
                msg: BusMessage {
                    project: *project,
                    topic: topic.clone(),
                    payload_json: val.payload_json.clone(),
                    sender: val.sender.clone(),
                    retain: true,
                },
            };
            if let Ok(b) = encode(&frame) {
                send_buf.extend_from_slice(&b);
            }
        }
        if let Ok(b) = encode(&BusFrame::ReplayEnd) {
            send_buf.extend_from_slice(&b);
        }
    }

    fn flush_subscriber(&mut self, i: usize) -> Result<(), ()> {
        let conn = &mut self.conns[i];
        let ConnState::Subscriber { send_buf } = &mut conn.state else {
            return Ok(());
        };
        while !send_buf.is_empty() {
            match conn.stream.write(send_buf) {
                Ok(0) => break,
                Ok(n) => {
                    send_buf.drain(0..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(()),
            }
        }
        if send_buf.len() > MAX_SEND_BUF {
            return Err(());
        }
        Ok(())
    }

    /// Core publish path: assign a seq, update + persist the retained
    /// store, push to the resume ring, and broadcast to live subscribers.
    fn accept_publish(&mut self, msg: BusMessage) {
        let seq = self.next_seq;
        self.next_seq += 1;

        if msg.retain {
            let is_tombstone = msg.payload_json.trim() == "null";
            let key = (msg.project, msg.topic.clone());
            if is_tombstone {
                self.retained.remove(&key);
            } else {
                self.retained.insert(
                    key,
                    RetainedVal {
                        seq,
                        payload_json: msg.payload_json.clone(),
                        sender: msg.sender.clone(),
                    },
                );
            }
            self.persist_retained();
        }

        let frame = match encode(&BusFrame::Message { seq, msg }) {
            Ok(b) => b,
            Err(_) => return,
        };

        if self.ring.len() == RING_CAPACITY {
            self.ring.pop_front();
        }
        self.ring.push_back(RingEntry {
            seq,
            frame: frame.clone(),
        });

        for c in &mut self.conns {
            if let ConnState::Subscriber { send_buf } = &mut c.state {
                if send_buf.len() < MAX_SEND_BUF {
                    send_buf.extend_from_slice(&frame);
                }
            }
        }
    }

    /// Expire stale `agent.hello.<id>` entries (project `None`) whose pid is
    /// gone or whose heartbeat aged out, by publishing a retained tombstone
    /// — exactly what the GUI's `sweep_stale_roster` used to do.
    fn sweep_roster(&mut self) {
        let now = now_secs();
        let stale: Vec<String> = self
            .retained
            .iter()
            .filter_map(|((project, topic), val)| {
                if project.is_some() {
                    return None; // agent bus is the global (None) channel
                }
                let id = topic.strip_prefix("agent.hello.")?;
                let payload: serde_json::Value =
                    serde_json::from_str(&val.payload_json).unwrap_or(serde_json::Value::Null);
                let pid_dead = payload
                    .get("pid")
                    .and_then(serde_json::Value::as_i64)
                    .map(|p| !pid_alive(p))
                    .unwrap_or(false);
                let ts_stale = payload
                    .get("ts")
                    .and_then(serde_json::Value::as_u64)
                    .map(|ts| now.saturating_sub(ts) > ROSTER_STALE_SECS)
                    .unwrap_or(false);
                (pid_dead || ts_stale).then(|| id.to_string())
            })
            .collect();

        for id in stale {
            self.accept_publish(BusMessage {
                project: None,
                topic: format!("agent.hello.{id}"),
                payload_json: "null".to_string(),
                sender: "roster-sweep".to_string(),
                retain: true,
            });
        }
    }

    fn persist_retained(&self) {
        let entries: Vec<PersistEntry> = self
            .retained
            .iter()
            .map(|((project, topic), val)| PersistEntry {
                project: *project,
                topic: topic.clone(),
                sender: val.sender.clone(),
                payload: serde_json::from_str(&val.payload_json)
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect();
        if let Ok(json) = serde_json::to_vec(&entries) {
            // Atomic-ish: write a temp then rename so a crash mid-write
            // can't leave a half-written store.
            let tmp = self.retained_path.with_extension("json.tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.retained_path);
            }
        }
    }
}

fn load_retained(path: &Path) -> HashMap<(Option<u64>, String), RetainedVal> {
    let mut out = HashMap::new();
    let Ok(bytes) = std::fs::read(path) else {
        return out;
    };
    let Ok(entries) = serde_json::from_slice::<Vec<PersistEntry>>(&bytes) else {
        return out;
    };
    for (i, e) in entries.into_iter().enumerate() {
        let payload_json = serde_json::to_string(&e.payload).unwrap_or_else(|_| "null".to_string());
        out.insert(
            (e.project, e.topic),
            RetainedVal {
                seq: i as u64,
                payload_json,
                sender: e.sender,
            },
        );
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `kill(pid, 0)`: 0 = alive; EPERM = alive but not ours.
fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Double-fork daemonize, mirroring `claude_bus` / `jim_daemon`. Skip when
/// `JIM_BUS_FOREGROUND=1` (tests, `tail -f` debugging, or when a host
/// already daemonized us).
pub fn daemonize_if_requested() {
    if std::env::var_os("JIM_BUS_FOREGROUND").is_some() {
        return;
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return;
    }
    if pid > 0 {
        std::process::exit(0);
    }
    unsafe {
        libc::setsid();
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return;
    }
    if pid > 0 {
        std::process::exit(0);
    }
    if let Ok(devnull) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let fd = devnull.as_raw_fd();
        unsafe {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
    }
    if let Some(path) = std::env::var_os("JIM_BUS_LOG") {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let fd = f.as_raw_fd();
            unsafe {
                libc::dup2(fd, 2);
            }
            std::mem::forget(f);
        }
    }
}

fn write_pid_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string().as_bytes())
}

pub fn run(socket: PathBuf, retained: PathBuf, pid_path: PathBuf) -> std::io::Result<()> {
    let d = Daemon::new(&socket, &retained)?;
    let _ = write_pid_file(&pid_path);
    eprintln!(
        "[jim-bus] listening on {} (pid={})",
        socket.display(),
        std::process::id()
    );
    d.run()
}
