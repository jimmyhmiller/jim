//! Phonon-2 (Fermion Research's 164 MB distillation of NVIDIA Parakeet TDT
//! 0.6B) as a live dictation engine, on its CPU engine.
//!
//! Measured against our warm whisper-server on an M2 Max: 5 s of audio in
//! ~0.06 s (whisper ~0.6 s), 60 s in ~1.2 s (whisper ~4.8 s), at about half
//! whisper's resident memory, with comparable accuracy on our recordings. It
//! is English-only, and more verbatim than whisper — it keeps "um" and
//! writes "gonna".
//!
//! The CPU engine rather than MLX on purpose: MLX was no faster on
//! dictation-length clips, grew to 9 GB after a five-minute file and never
//! gave it back, and would compete with Bevy for the GPU.
//!
//! Phonon runs as `phonon serve` from the pinned virtualenv that
//! `scripts/install-phonon.sh` builds in `~/.jim/phonon/venv`, managed as a
//! shared server exactly like whisper's (see [`super::server`]). That script
//! is compiled in: when Phonon is picked and its install is missing or for
//! an older pin, jim runs it in the background and says on the pill when
//! Phonon is ready (or why the install failed).
//!
//! Live text comes from its streaming endpoint, `GET /v1/audio/stream`
//! (a WebSocket): we send 16 kHz f32 PCM as it's captured, and it sends back
//! `partial` (the whole in-flight phrase, replacing the last partial),
//! `final` (a phrase closed by a ~0.7 s pause — frozen) and, after we send
//! `end`, `done` with the whole transcript. The server does the segmenting
//! and re-decoding; this side only moves bytes and assembles text.

use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::{Message, WebSocket};

use super::server::{self, Shared, Spec};
use super::{RATE, Transcriber, join};

/// A one-shot transcription of a long recovery span, or the first request
/// on a cold server, can take a while.
const BATCH_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `finish` waits for the last phrase's decode and `done`.
const FINISH_TIMEOUT: Duration = Duration::from_secs(30);

static SERVER: Shared = Shared::new(Spec {
    name: "phonon",
    key: server_key,
    command: spawn_command,
    is_ours: is_phonon_server,
    // A cold load is ~9 s; the first ever run in an environment also
    // compiles the CPU runtime, which `warm` absorbs.
    startup_timeout: Duration::from_secs(120),
    warm: Some(warm),
});

/// The installer, compiled in so jim can install Phonon itself.
const INSTALL_SCRIPT: &str = include_str!("../../../../scripts/install-phonon.sh");

/// What starting a dictation says while the install runs.
const INSTALLING: &str = "Installing Phonon (first use, a minute or two) — try again when it says it's ready";

fn phonon_home() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".jim/phonon"))
}

fn venv_dir() -> Option<PathBuf> {
    Some(phonon_home()?.join("venv"))
}

fn phonon_bin() -> Option<PathBuf> {
    Some(venv_dir()?.join("bin/phonon"))
}

fn not_installed() -> String {
    "Phonon isn't installed — pick \"Dictation: Use Phonon\" to install it".into()
}

/// The package pin the compiled-in installer installs: its `PIN="…"` line.
fn expected_pin() -> Result<&'static str, String> {
    INSTALL_SCRIPT
        .lines()
        .find_map(|l| l.strip_prefix("PIN=\"")?.strip_suffix('"'))
        .ok_or_else(|| "install-phonon.sh has no PIN=\"…\" line".into())
}

/// Installed, completely, for the pin this jim was built with. The
/// installer writes the pin to `installed` as its very last step, so an
/// interrupted install doesn't count, and bumping the pin reinstalls.
pub fn installed() -> bool {
    let (Some(home), Some(bin), Ok(pin)) = (phonon_home(), phonon_bin(), expected_pin()) else {
        return false;
    };
    let marker = std::fs::read_to_string(home.join("installed")).unwrap_or_default();
    marker.trim() == pin && bin.exists()
}

/// This process is running the installer.
static INSTALL_RUNNING: AtomicBool = AtomicBool::new(false);
/// How the last install (and the warm-up after it) went, until the pill
/// takes it.
static INSTALL_REPORT: Mutex<Option<Result<(), String>>> = Mutex::new(None);

/// The outcome of a finished background install, once.
pub fn take_install_report() -> Option<Result<(), String>> {
    INSTALL_REPORT.lock().ok()?.take()
}

/// Install in the background (then warm the server), unless an install is
/// already running here.
fn begin_install() {
    if INSTALL_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("phonon-install".into())
        .spawn(|| {
            let result = install().and_then(|()| SERVER.ensure_running().map(|_| ()));
            match &result {
                Ok(()) => eprintln!("[phonon] installed and warm"),
                Err(e) => eprintln!("[phonon] install failed: {e}"),
            }
            if let Ok(mut r) = INSTALL_REPORT.lock() {
                *r = Some(result);
            }
            INSTALL_RUNNING.store(false, Ordering::Release);
            // Jim idles without frames; wake it so the pill shows the news.
            jim_widget::request_main_loop_wakeup();
        });
    if let Err(e) = spawned {
        INSTALL_RUNNING.store(false, Ordering::Release);
        eprintln!("[phonon] could not start the install thread: {e}");
    }
}

/// Run the compiled-in installer, logging to `~/.jim/phonon/install.log`.
///
/// Under an exclusive lock that the installer itself inherits, so the lock
/// outlives jim: a jim that quits mid-install leaves the script running,
/// and the next one waits for it instead of deleting the venv it is
/// building.
fn install() -> Result<(), String> {
    let home = phonon_home().ok_or("no HOME")?;
    std::fs::create_dir_all(&home).map_err(|e| format!("create {}: {e}", home.display()))?;
    let lock = server::lock_exclusive(&home.join("install.lock"))?;
    if installed() {
        // Another jim finished it while we waited.
        return Ok(());
    }
    let log_path = home.join("install.log");
    let log = std::fs::File::create(&log_path)
        .map_err(|e| format!("create {}: {e}", log_path.display()))?;
    let log_err = log.try_clone().map_err(|e| format!("{}: {e}", log_path.display()))?;
    eprintln!("[phonon] installing (log: {})", log_path.display());
    let began = Instant::now();

    let mut cmd = Command::new("/bin/bash");
    cmd.args(["-c", INSTALL_SCRIPT, "install-phonon.sh"])
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    // A Dock-launched jim has launchd's PATH, without ~/.local/bin's uv.
    if let Some(path) = jim_widget::subprocess::augmented_path() {
        cmd.env("PATH", path);
    }
    let fd = lock.as_raw_fd();
    // SAFETY: only async-signal-safe fcntl calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = cmd.status().map_err(|e| format!("run the Phonon installer: {e}"))?;
    eprintln!(
        "[phonon] installer exited {status} after {:.0}s",
        began.elapsed().as_secs_f32()
    );
    if !status.success() {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let last = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("Phonon install failed: {last} (log: {})", log_path.display()));
    }
    if !installed() {
        return Err(format!(
            "the Phonon installer succeeded but didn't record pin {} (log: {})",
            expected_pin()?,
            log_path.display()
        ));
    }
    Ok(())
}

/// The entry point's mtime is in the key, so reinstalling (or upgrading the
/// pin) replaces a server still running the old install.
fn server_key() -> Result<String, String> {
    let bin = phonon_bin().ok_or("no HOME")?;
    let mtime = std::fs::metadata(&bin)
        .and_then(|m| m.modified())
        .map_err(|_| not_installed())?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(format!("phonon-2 cpu {} {mtime}", bin.display()))
}

fn spawn_command(port: u16) -> Result<Command, String> {
    let bin = phonon_bin().ok_or("no HOME")?;
    if !bin.exists() {
        return Err(not_installed());
    }
    let mut cmd = Command::new(&bin);
    cmd.args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
        .env("FERMION_DEVICE", "cpu")
        // The installer fetched and verified the model; a server that
        // phones the Hub on every start would fail to start offline.
        .env("HF_HUB_OFFLINE", "1");
    Ok(cmd)
}

/// It runs under Python, so the executable name is just `Python`; the argv
/// is what identifies it.
fn is_phonon_server(pid: i32) -> bool {
    let (Some(bin), Some(args)) = (phonon_bin(), server::proc_args(pid)) else {
        return false;
    };
    let bin = bin.to_string_lossy();
    args.iter().any(|a| a == bin.as_ref()) && args.iter().any(|a| a == "serve")
}

/// The first decode in a fresh environment compiles the CPU runtime (~20 s
/// measured); pay it here rather than on the user's first words.
fn warm(port: u16) -> Result<(), String> {
    let quiet: Vec<f32> = (0..RATE / 2)
        .map(|i| (i as f32 * 0.37).sin() * 0.001)
        .collect();
    batch(port, &quiet, Duration::from_secs(180)).map(|_| ())
}

/// Get the server warm — installing Phonon first if it needs it.
pub fn prewarm() {
    if installed() {
        SERVER.prewarm();
    } else {
        begin_install();
    }
}

/// Bring the server up and open the stream before the microphone opens, so
/// a missing install or a busy engine is reported before capture.
///
/// Doesn't wait on an install: that takes minutes, with the pill sitting on
/// "Starting…" the whole time. It starts one (if one isn't running) and
/// says so instead.
pub fn start() -> Result<Box<dyn Transcriber>, String> {
    if !installed() {
        begin_install();
        return Err(INSTALLING.into());
    }
    let port = SERVER.ensure_running()?;
    Ok(Box::new(PhononStream::connect(port)?))
}

/// One-shot transcription over `POST /v1/audio/transcriptions`.
fn batch(port: u16, samples: &[f32], timeout: Duration) -> Result<String, String> {
    if samples.is_empty() {
        return Ok(String::new());
    }
    let wav = server::encode_wav(samples, RATE)?;
    server::post_wav(
        &format!("http://127.0.0.1:{port}/v1/audio/transcriptions"),
        &wav,
        &[("model", "phonon-2"), ("response_format", "text")],
        timeout,
    )
    .map(|t| t.trim().to_string())
}

struct PhononStream {
    ws: WebSocket<TcpStream>,
    /// Text recovered from streams that broke mid-session; the live stream's
    /// text follows it.
    base: String,
    /// Closed phrases of the live stream. Frozen.
    finals: Vec<String>,
    /// The in-flight phrase, replaced wholesale by each `partial`.
    partial: String,
    /// Everything sent on the live stream, so a broken stream's audio can
    /// still be transcribed in one shot rather than lost.
    sent: Vec<f32>,
    /// Text changed since the last `step` reported it.
    dirty: bool,
}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(
        io.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ))
}

impl PhononStream {
    fn connect(port: u16) -> Result<Self, String> {
        Ok(PhononStream {
            ws: open_stream(port)?,
            base: String::new(),
            finals: Vec::new(),
            partial: String::new(),
            sent: Vec::new(),
            dirty: false,
        })
    }

    fn text(&self) -> String {
        join(&self.base, &join(&self.finals.join(" "), &self.partial))
    }

    /// Apply one server message. Returns the `done` transcript when this was
    /// the end of the stream.
    fn handle(&mut self, raw: &str) -> Result<Option<String>, String> {
        let v: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("phonon sent non-JSON: {e}: {raw:.200}"))?;
        let text = || v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        match v.get("type").and_then(|t| t.as_str()) {
            Some("partial") => {
                self.partial = text();
                self.dirty = true;
            }
            Some("final") => {
                let t = text();
                if !t.is_empty() {
                    self.finals.push(t);
                }
                self.partial.clear();
                self.dirty = true;
            }
            Some("done") => return Ok(Some(text())),
            Some("error") => {
                let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
                return Err(format!("phonon: {msg}"));
            }
            other => return Err(format!("phonon sent an unknown message type {other:?}: {raw:.200}")),
        }
        Ok(None)
    }

    /// The transport broke (the server died, or the socket did). Transcribe
    /// what the dead stream had heard in one shot, freeze it as `base`, and
    /// carry on with a fresh stream — the user keeps talking throughout.
    fn recover(&mut self, why: String) -> Result<(), String> {
        eprintln!(
            "[dictation] phonon stream broke ({why}); recovering {:.1}s of audio",
            self.sent.len() as f32 / RATE as f32
        );
        let port = SERVER.ensure_running()?;
        let recovered = batch(port, &self.sent, BATCH_TIMEOUT)
            .map_err(|e| format!("phonon stream broke ({why}) and recovery failed: {e}"))?;
        self.base = join(&self.base, &recovered);
        self.finals.clear();
        self.partial.clear();
        self.sent.clear();
        self.ws = open_stream(port)?;
        self.dirty = true;
        Ok(())
    }

    /// Write whatever is buffered; a full socket just means "later".
    fn flush(&mut self) -> Result<(), tungstenite::Error> {
        match self.ws.flush() {
            Err(e) if would_block(&e) => Ok(()),
            r => r,
        }
    }
}

/// Connect, handshake, send the config frame, and switch to non-blocking so
/// [`Transcriber::step`] can poll for messages without stalling the worker.
fn open_stream(port: u16) -> Result<WebSocket<TcpStream>, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("phonon: connect to port {port}: {e}"))?;
    let _ = sock.set_nodelay(true);
    sock.set_read_timeout(Some(Duration::from_secs(5)))
        .and_then(|()| sock.set_write_timeout(Some(Duration::from_secs(5))))
        .map_err(|e| format!("phonon: socket timeouts: {e}"))?;
    let (mut ws, _) = tungstenite::client(format!("ws://127.0.0.1:{port}/v1/audio/stream"), sock)
        .map_err(|e| format!("phonon: stream handshake: {e}"))?;
    let config = serde_json::json!({ "sample_rate": RATE, "format": "pcm_f32le" });
    ws.send(Message::Text(config.to_string()))
        .map_err(|e| format!("phonon: send stream config: {e}"))?;
    ws.get_ref()
        .set_nonblocking(true)
        .map_err(|e| format!("phonon: non-blocking socket: {e}"))?;
    Ok(ws)
}

impl Transcriber for PhononStream {
    fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        self.sent.extend_from_slice(samples);
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        // A WouldBlock from `write` still leaves the frame queued in
        // tungstenite's buffer; the next flush sends it.
        let result = match self.ws.write(Message::Binary(bytes)) {
            Err(e) if !would_block(&e) => Err(e),
            _ => self.flush(),
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => self.recover(format!("send: {e}")),
        }
    }

    fn step(&mut self) -> Result<Option<String>, String> {
        if let Err(e) = self.flush() {
            self.recover(format!("send: {e}"))?;
        }
        loop {
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    if self.handle(&t)?.is_some() {
                        return Err("phonon ended the stream before we asked it to".into());
                    }
                }
                Ok(Message::Close(frame)) => {
                    self.recover(format!("server closed the stream: {frame:?}"))?;
                    break;
                }
                Ok(_) => {}
                Err(e) if would_block(&e) => break,
                Err(e) => {
                    self.recover(format!("receive: {e}"))?;
                    break;
                }
            }
        }
        if !self.dirty {
            return Ok(None);
        }
        self.dirty = false;
        Ok(Some(self.text()))
    }

    fn finish(&mut self) -> Result<String, String> {
        match self.finish_stream() {
            Ok(done) => Ok(join(&self.base, &done)),
            Err(e) => {
                // The last phrase only exists server-side until `done`, so
                // fall back to one shot over everything this stream heard.
                eprintln!("[dictation] phonon stream failed at the end ({e}); transcribing it in one shot");
                let port = SERVER.ensure_running()?;
                let rest = batch(port, &self.sent, BATCH_TIMEOUT)
                    .map_err(|b| format!("{e}; one-shot fallback failed: {b}"))?;
                Ok(join(&self.base, &rest))
            }
        }
    }
}

impl PhononStream {
    /// Send `end` and block until `done`.
    fn finish_stream(&mut self) -> Result<String, String> {
        let sock = self.ws.get_ref();
        sock.set_nonblocking(false)
            .and_then(|()| sock.set_read_timeout(Some(FINISH_TIMEOUT)))
            .map_err(|e| format!("socket: {e}"))?;
        self.ws
            .send(Message::Text(r#"{"type":"end"}"#.into()))
            .map_err(|e| format!("send end: {e}"))?;
        loop {
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    if let Some(done) = self.handle(&t)? {
                        let _ = self.ws.close(None);
                        let _ = self.ws.flush();
                        return Ok(done);
                    }
                }
                Ok(Message::Close(frame)) => return Err(format!("closed before done: {frame:?}")),
                Ok(_) => {}
                Err(e) => return Err(format!("waiting for done: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A stand-in for `phonon serve`'s stream endpoint, speaking its
    /// protocol: config frame, binary PCM, `end` → final + done.
    fn fake_server(script: fn(&mut WebSocket<TcpStream>, usize)) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(sock).unwrap();
            let config = ws.read().unwrap();
            let v: serde_json::Value = serde_json::from_str(config.to_text().unwrap()).unwrap();
            assert_eq!(v["sample_rate"], RATE);
            assert_eq!(v["format"], "pcm_f32le");
            let mut samples = 0usize;
            loop {
                match ws.read() {
                    Ok(Message::Binary(b)) => {
                        assert_eq!(b.len() % 4, 0, "f32 frames must be whole samples");
                        samples += b.len() / 4;
                        script(&mut ws, samples);
                    }
                    Ok(Message::Text(t)) => {
                        assert_eq!(t, r#"{"type":"end"}"#);
                        let send = |ws: &mut WebSocket<TcpStream>, s: &str| {
                            ws.send(Message::Text(s.into())).unwrap()
                        };
                        send(&mut ws, r#"{"type":"final","text":"world.","segment":2}"#);
                        send(&mut ws, r#"{"type":"done","text":"Hello there. world."}"#);
                        let _ = ws.close(None);
                        return;
                    }
                    _ => return,
                }
            }
        });
        port
    }

    fn pump_until(s: &mut PhononStream, want: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        while std::time::Instant::now() < deadline {
            if let Some(t) = s.step().unwrap() {
                last = t;
                if last == want {
                    return last;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("never saw {want:?}; last text was {last:?}");
    }

    #[test]
    fn assembles_partials_finals_and_done() {
        let port = fake_server(|ws, samples| {
            let send = |ws: &mut WebSocket<TcpStream>, s: &str| ws.send(Message::Text(s.into())).unwrap();
            if samples == 1600 {
                send(ws, r#"{"type":"partial","text":"Hello the"}"#);
            } else if samples == 3200 {
                send(ws, r#"{"type":"partial","text":"Hello there"}"#);
                send(ws, r#"{"type":"final","text":"Hello there.","segment":1}"#);
                send(ws, r#"{"type":"partial","text":"wor"}"#);
            }
        });
        let mut s = PhononStream::connect(port).unwrap();
        s.push(&[0.1; 1600]).unwrap();
        assert_eq!(pump_until(&mut s, "Hello the"), "Hello the");
        s.push(&[0.1; 1600]).unwrap();
        assert_eq!(pump_until(&mut s, "Hello there. wor"), "Hello there. wor");
        assert_eq!(s.finish().unwrap(), "Hello there. world.");
    }

    #[test]
    fn a_server_error_message_is_reported() {
        let port = fake_server(|ws, _| {
            ws.send(Message::Text(r#"{"type":"error","message":"engine busy — one stream at a time"}"#.into()))
                .unwrap();
        });
        let mut s = PhononStream::connect(port).unwrap();
        s.push(&[0.0; 160]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match s.step() {
                Err(e) => {
                    assert!(e.contains("engine busy"), "{e}");
                    break;
                }
                Ok(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Ok(_) => panic!("the server's error never surfaced"),
            }
        }
    }

    #[test]
    fn reads_the_pin_from_the_compiled_in_installer() {
        let pin = expected_pin().unwrap();
        assert!(pin.starts_with("fermion-research=="), "{pin}");
    }

    #[test]
    fn identifies_a_server_by_argv_not_name() {
        // `sleep` is not a phonon server, whatever its name.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(!is_phonon_server(child.id() as i32));
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The whole engine against a REAL `phonon serve` in the test-private
    /// state dir. Needs Phonon installed (jim does it on first use, or run
    /// `scripts/install-phonon.sh`).
    ///
    ///   cargo test -p jim_app --lib dictation::phonon -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn streams_a_long_clip_without_losing_words() {
        let mut t = start().expect("phonon must start (is it installed?)");
        let report = super::super::tests::stream_spoken_clip(t.as_mut());
        SERVER.shutdown_for_test();
        let _ = std::fs::remove_dir_all(server::state_dir().unwrap());
        report.assert_complete();
    }

    /// Kill the server mid-dictation: the stream must recover — respawn,
    /// transcribe what the dead stream heard, reconnect — and lose nothing.
    #[test]
    #[ignore]
    fn survives_the_server_dying_mid_stream() {
        let mut t = start().expect("phonon must start (is it installed?)");
        let mut killed = false;
        let report = super::super::tests::stream_spoken_clip_with(t.as_mut(), &mut |secs| {
            if !killed && secs > 12.0 {
                killed = true;
                let pid = SERVER.recorded_pid_for_test().expect("recorded server");
                eprintln!("killing phonon server pid={pid} at {secs:.1}s");
                // SAFETY: test-private server we spawned.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
        });
        SERVER.shutdown_for_test();
        let _ = std::fs::remove_dir_all(server::state_dir().unwrap());
        assert!(killed, "the clip ended before the kill point");
        report.assert_complete();
    }
}
