//! A warm transcription server that outlives the GUI.
//!
//! Both engines are long-lived local HTTP servers: `whisper-server` and
//! Phonon's `phonon serve`. Loading either model is the expensive part
//! (whisper ~1GB, Phonon a cold load plus a one-off runtime compile), so the
//! server is a shared service in the same spirit as `jim-daemon` and
//! `jim-bus`: jim starts it in the background at launch ([`Shared::prewarm`]),
//! records it in `~/.jim/<name>-server`, and every later jim ADOPTS the
//! recorded server instead of starting its own. Nothing kills it on exit or
//! after idle — a restart used to throw away the loaded model, so the first
//! dictation after every `dev-restart` sat waiting on a cold load. A server is
//! only replaced when it dies, stops answering, or fails a request.
//!
//! [`Spec`] is everything that differs between the two: how to launch one,
//! how to recognise one by pid, and what it must have been started with to be
//! worth adopting.

use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(super) struct Spec {
    /// Log prefix, and the stem of `~/.jim/<name>-server{,.lock,.log}`.
    pub name: &'static str,
    /// What a recorded server must have been started with to be adopted —
    /// the model, the device, anything that changes its answers. A recorded
    /// server with a different key is replaced.
    pub key: fn() -> Result<String, String>,
    /// The command that serves on `port`. Fails with an explanation when
    /// something it needs (a model, an install) is missing.
    pub command: fn(port: u16) -> Result<Command, String>,
    /// Whether a live pid is one of these servers, so a recorded pid that was
    /// recycled by something else is never adopted or signalled.
    pub is_ours: fn(pid: i32) -> bool,
    /// How long a cold start may take to accept connections.
    pub startup_timeout: Duration,
    /// Run once on a server THIS jim just spawned, after it accepts
    /// connections — for first-request costs a client shouldn't pay.
    pub warm: Option<fn(port: u16) -> Result<(), String>>,
}

struct Server {
    pid: u32,
    port: u16,
    /// `Some` only when THIS jim spawned the server. Then it is our child and
    /// must be reaped through the handle — `kill(pid, 0)` succeeds on a
    /// zombie, so a pid probe alone would call a dead child alive. An adopted
    /// server belongs to launchd, which reaps it.
    child: Option<Child>,
    /// Whether [`Spec::warm`] still has to run. Claimed by the first caller
    /// to reach it, so it runs once even when prewarm and a dictation race.
    needs_warm: bool,
}

pub(super) struct Shared {
    spec: Spec,
    /// This process's handle on the shared server. A cache of the record
    /// file, not the source of truth: another jim may have replaced it.
    cur: Mutex<Option<Server>>,
}

impl Shared {
    pub const fn new(spec: Spec) -> Self {
        Shared {
            spec,
            cur: Mutex::new(None),
        }
    }

    /// Start (or adopt) the server in the background, so it is warm before
    /// the first dictation rather than loaded by it.
    pub fn prewarm(&'static self) {
        let name = self.spec.name;
        let spawned = std::thread::Builder::new()
            .name(format!("{name}-prewarm"))
            .spawn(move || match self.ensure_running() {
                Ok(port) => eprintln!("[{name}] server warm on port {port}"),
                Err(e) => eprintln!("[{name}] prewarm failed: {e}"),
            });
        if let Err(e) = spawned {
            eprintln!("[{name}] could not start prewarm thread: {e}");
        }
    }

    /// Port of a live, connectable server, adopting or starting one if
    /// needed.
    ///
    /// The lock is held only to find or create the server, never across the
    /// model-load wait: a dictation that arrives while prewarm is still
    /// loading should wait on the same server, not queue behind a lock.
    pub fn ensure_running(&self) -> Result<u16, String> {
        let name = self.spec.name;
        let (pid, port) = {
            let mut g = self.cur.lock().map_err(|_| format!("{name} server lock poisoned"))?;
            if let Some(s) = g.as_mut() {
                if !self.alive(s) {
                    eprintln!("[{name}] server pid={} died; replacing it", s.pid);
                    self.forget_record(s.pid);
                    *g = None;
                }
            }
            if g.is_none() {
                *g = Some(self.adopt_or_spawn()?);
            }
            let s = g.as_ref().ok_or_else(|| format!("{name} server vanished"))?;
            (s.pid, s.port)
        };

        if let Err(e) = self.wait_ready(pid, port) {
            self.discard(port);
            return Err(e);
        }

        let warm = {
            let mut g = self.cur.lock().map_err(|_| format!("{name} server lock poisoned"))?;
            match g.as_mut() {
                Some(s) if s.pid == pid && s.needs_warm => {
                    s.needs_warm = false;
                    self.spec.warm
                }
                _ => None,
            }
        };
        if let Some(warm) = warm {
            let began = Instant::now();
            if let Err(e) = warm(port) {
                self.discard(port);
                return Err(format!("{name} warm-up failed: {e}"));
            }
            eprintln!(
                "[{name}] warmed in {:.1}s",
                began.elapsed().as_secs_f32()
            );
        }
        Ok(port)
    }

    /// Kill a server that failed a request or never came up, but only if it
    /// is still the instance on that port — another caller may already have
    /// replaced it. Killing it also interrupts any stuck request before a
    /// retry starts a clean process.
    pub fn discard(&self, port: u16) {
        let Ok(mut g) = self.cur.lock() else { return };
        if g.as_ref().is_some_and(|s| s.port == port) {
            if let Some(s) = g.take() {
                self.kill(s, "discarding unhealthy server");
            }
        }
    }

    fn alive(&self, s: &mut Server) -> bool {
        match s.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => pid_alive(s.pid as i32) && (self.spec.is_ours)(s.pid as i32),
        }
    }

    /// Kill the server's whole process group and drop its record, so no
    /// other jim adopts a corpse. The group because the server is its own
    /// group leader, and a Python server's helpers would otherwise keep the
    /// model resident.
    fn kill(&self, mut s: Server, why: &str) {
        eprintln!("[{}] {why}: pid={} port={}", self.spec.name, s.pid, s.port);
        // SAFETY: a negative pid is the group; SIGKILL to a group we created
        // or verified with `is_ours`.
        unsafe { libc::kill(-(s.pid as i32), libc::SIGKILL) };
        if let Some(c) = s.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.forget_record(s.pid);
    }

    /// Adopt the recorded server if it is still a live one of ours with the
    /// right key; otherwise start one and record it.
    ///
    /// Holds an exclusive `flock` across the check and the spawn, so two jims
    /// starting together (or a jim and a test) share one server rather than
    /// each loading a model.
    fn adopt_or_spawn(&self) -> Result<Server, String> {
        let name = self.spec.name;
        let dir = state_dir().ok_or("no HOME")?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let _lock = lock_exclusive(&dir.join(format!("{name}-server.lock")))?;

        let key = (self.spec.key)()?;
        if let Some((pid, port, recorded)) = self.read_record() {
            let ours = pid_alive(pid as i32) && (self.spec.is_ours)(pid as i32);
            if ours && recorded == key {
                eprintln!("[{name}] adopting running server: pid={pid} port={port}");
                return Ok(Server {
                    pid,
                    port,
                    child: None,
                    needs_warm: false,
                });
            }
            if ours {
                // Alive but started differently: replace it rather than leave
                // a second model-sized server running unrecorded.
                eprintln!("[{name}] recorded server pid={pid} has another configuration; replacing it");
                // SAFETY: group then pid, to a verified server of ours.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }

        let server = self.spawn()?;
        self.write_record(server.pid, server.port, &key)?;
        Ok(server)
    }

    fn spawn(&self) -> Result<Server, String> {
        let name = self.spec.name;
        let port = free_port()?;
        let mut cmd = (self.spec.command)(port)?;
        // It outlives this jim, so its startup chatter goes to its own log.
        let log = state_dir()
            .map(|d| d.join(format!("{name}-server.log")))
            .and_then(|p| std::fs::File::create(p).ok());
        cmd.stdin(Stdio::null())
            .stdout(match log.as_ref().and_then(|f| f.try_clone().ok()) {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            })
            .stderr(match log {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            })
            // Its own process group: a signal to jim's group (a terminal ^C on
            // a `cargo run`) must not take the shared server down with it, and
            // `kill` can signal the server and any helper as a unit.
            .process_group(0);
        // A Dock-launched Jim inherits launchd's minimal PATH — without this,
        // Homebrew's binaries aren't findable.
        if let Some(path) = jim_widget::subprocess::augmented_path() {
            cmd.env("PATH", path);
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("{name} server failed to launch: {e}"))?;
        let pid = child.id();
        eprintln!("[{name}] server spawned: pid={pid} port={port}");
        Ok(Server {
            pid,
            port,
            child: Some(child),
            needs_warm: true,
        })
    }

    /// Block until the server accepts connections (both servers bind only
    /// once the model is loaded), or it dies / is replaced / we run out of
    /// patience. A warm server returns on the first connect.
    fn wait_ready(&self, pid: u32, port: u16) -> Result<(), String> {
        let name = self.spec.name;
        let deadline = Instant::now() + self.spec.startup_timeout;
        loop {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok() {
                return Ok(());
            }
            {
                let mut g = self.cur.lock().map_err(|_| format!("{name} server lock poisoned"))?;
                match g.as_mut() {
                    Some(s) if s.pid == pid => {
                        if !self.alive(s) {
                            self.forget_record(pid);
                            *g = None;
                            return Err(format!(
                                "{name} server pid={pid} exited during startup (see ~/.jim/{name}-server.log)"
                            ));
                        }
                    }
                    _ => return Err(format!("{name} server was replaced during startup")),
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{name} server didn't come up within {}s (see ~/.jim/{name}-server.log)",
                    self.spec.startup_timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn record_path(&self) -> Option<PathBuf> {
        Some(state_dir()?.join(format!("{}-server", self.spec.name)))
    }

    /// `pid\nport\nkey\n` of the live shared server.
    fn read_record(&self) -> Option<(u32, u16, String)> {
        let text = std::fs::read_to_string(self.record_path()?).ok()?;
        let mut lines = text.lines();
        let pid = lines.next()?.trim().parse().ok()?;
        let port = lines.next()?.trim().parse().ok()?;
        let key = lines.next()?.to_string();
        Some((pid, port, key))
    }

    /// Written via a temp file + rename, so a jim reading it mid-write never
    /// sees half a record.
    fn write_record(&self, pid: u32, port: u16, key: &str) -> Result<(), String> {
        let path = self.record_path().ok_or("no HOME")?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, format!("{pid}\n{port}\n{key}\n"))
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("record {} server in {}: {e}", self.spec.name, path.display()))
    }

    /// Drop the record if it still names `pid` — a newer server may have
    /// replaced it already.
    fn forget_record(&self, pid: u32) {
        let Some(path) = self.record_path() else { return };
        if self.read_record().is_some_and(|(p, _, _)| p == pid) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Tear down whatever server this process holds. Tests only: the real
    /// servers are meant to outlive jim.
    #[cfg(test)]
    pub fn shutdown_for_test(&self) {
        let taken = self.cur.lock().unwrap().take();
        if let Some(s) = taken {
            self.kill(s, "test teardown");
        }
    }

    #[cfg(test)]
    pub fn recorded_pid_for_test(&self) -> Option<u32> {
        self.read_record().map(|(p, _, _)| p)
    }

    /// Drop the in-process handle without killing anything, as a restarted
    /// jim would have none. Returns it so the test can still reap its child.
    #[cfg(test)]
    pub fn forget_handle_for_test(&self) -> Option<(u32, Option<Child>)> {
        self.cur.lock().unwrap().take().map(|s| (s.pid, s.child))
    }

    #[cfg(test)]
    pub fn holds_adopted_for_test(&self, pid: u32) -> bool {
        self.cur
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.pid == pid && s.child.is_none())
    }
}

/// Take an exclusive `flock` on `path`, waiting for it. Held until the
/// returned file — and every descriptor duplicated from it — is closed.
pub(super) fn lock_exclusive(path: &std::path::Path) -> Result<std::fs::File, String> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor we own.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!("flock {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    Ok(lock)
}

/// Where the shared servers' records, locks and logs live.
#[cfg(not(test))]
pub(super) fn state_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".jim"))
}

/// Tests get a private directory, so they spawn and kill their own server
/// instead of adopting — and then killing — the one the running jim uses.
#[cfg(test)]
pub(super) fn state_dir() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("jim-dictation-test-{}", std::process::id())))
}

pub(super) fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 checks for the process without delivering anything.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// The executable name behind a pid.
pub(super) fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `proc_name` writes at most `buf.len()` bytes and returns how
    // many; a non-positive return means it wrote none.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// A process's argv, via `KERN_PROCARGS2`. Needed for servers that run under
/// an interpreter, whose executable name is just `Python`.
pub(super) fn proc_args(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query with a null buffer.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is `size` bytes; the kernel writes at most that and
    // updates `size` to what it wrote.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    buf.truncate(size);
    // Layout: argc (i32), exec path, NUL padding, then argc NUL-terminated
    // argument strings (then the environment, which we ignore).
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let mut rest = &buf[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[path_end..];
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    let mut args = Vec::with_capacity(argc);
    for part in rest.split(|&b| b == 0).take(argc) {
        args.push(String::from_utf8_lossy(part).into_owned());
    }
    (args.len() == argc).then_some(args)
}

/// Ask the OS for an unused port by binding one and letting it go.
///
/// Racy in principle — something else could take it in the gap before the
/// server binds — but a hardcoded port collides with a second Jim, or a
/// leftover server, every time.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port: {e}"))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("no local addr: {e}"))
}

/// 16-bit mono WAV in memory. Nothing touches the disk in the live loop.
pub(super) fn encode_wav(samples: &[f32], rate: u32) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut w = hound::WavWriter::new(&mut buf, spec).map_err(|e| format!("wav: {e}"))?;
        for &s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .map_err(|e| format!("wav write: {e}"))?;
        }
        w.finalize().map_err(|e| format!("wav finalize: {e}"))?;
    }
    Ok(buf.into_inner())
}

/// POST a WAV plus text fields as multipart/form-data and return the body.
/// Hand-rolled because ureq 2 has no multipart builder and these requests
/// need exactly one file and a few short fields.
pub(super) fn post_wav(
    url: &str,
    wav: &[u8],
    fields: &[(&str, &str)],
    timeout: Duration,
) -> Result<String, String> {
    const BOUNDARY: &str = "----jimdictation7f3a1c";
    let mut body: Vec<u8> = Vec::with_capacity(wav.len() + 1024);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_write(timeout)
        .timeout_read(timeout)
        .build();
    let resp = agent
        .post(url)
        .set(
            "Content-Type",
            &format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .send_bytes(&body)
        .map_err(|e| format!("request to {url} failed: {e}"))?;
    resp.into_string()
        .map_err(|e| format!("response from {url} unreadable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_args_reads_our_own_argv() {
        let args = proc_args(std::process::id() as i32).expect("argv of this test process");
        let ours: Vec<String> = std::env::args().collect();
        assert_eq!(args, ours);
    }

    #[test]
    fn proc_args_of_a_child_includes_its_arguments() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let args = proc_args(child.id() as i32).expect("argv of sleep");
        assert_eq!(args.last().map(String::as_str), Some("30"));
        let _ = child.kill();
        let _ = child.wait();
    }

    fn sleeper_spec() -> Spec {
        Spec {
            name: "sleeper",
            key: || Ok("k".into()),
            command: |_| Err("sleeper servers are never spawned in this test".into()),
            is_ours: |pid| proc_name(pid).as_deref() == Some("sleep"),
            startup_timeout: Duration::from_secs(1),
            warm: None,
        }
    }

    /// A record naming a live process that is NOT one of our servers (a
    /// recycled pid) must never be adopted — jim would send audio to a port
    /// nothing is serving, or worse, later SIGKILL that process's group.
    #[test]
    fn a_recycled_pid_is_not_adopted() {
        static S: std::sync::LazyLock<Shared> = std::sync::LazyLock::new(|| {
            Shared::new(Spec {
                name: "recycled",
                is_ours: |_| false,
                ..sleeper_spec()
            })
        });
        let mut impostor = Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::create_dir_all(state_dir().unwrap()).unwrap();
        S.write_record(impostor.id(), 1, "k").unwrap();

        let err = S.adopt_or_spawn().err().expect("must not adopt an impostor");
        assert!(err.contains("never spawned"), "it tried to spawn instead: {err}");
        assert!(
            matches!(impostor.try_wait(), Ok(None)),
            "an impostor that isn't ours was signalled"
        );
        // Forgetting a different pid must leave the record alone…
        S.forget_record(impostor.id() + 1);
        assert_eq!(S.recorded_pid_for_test(), Some(impostor.id()));
        // …and forgetting this one removes it.
        S.forget_record(impostor.id());
        assert!(S.recorded_pid_for_test().is_none());

        let _ = impostor.kill();
        let _ = impostor.wait();
    }

    /// A live server of ours recorded with the right key is adopted, not
    /// respawned — the reason the record exists.
    #[test]
    fn a_matching_record_is_adopted() {
        static S: std::sync::LazyLock<Shared> =
            std::sync::LazyLock::new(|| Shared::new(sleeper_spec()));
        let mut server = Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::create_dir_all(state_dir().unwrap()).unwrap();
        S.write_record(server.id(), 4321, "k").unwrap();

        let adopted = S.adopt_or_spawn().expect("should adopt the recorded server");
        assert_eq!((adopted.pid, adopted.port), (server.id(), 4321));
        assert!(adopted.child.is_none(), "an adopted server is not our child");
        assert!(!adopted.needs_warm, "an adopted server was already warmed by its spawner");

        S.forget_record(server.id());
        let _ = server.kill();
        let _ = server.wait();
    }
}
