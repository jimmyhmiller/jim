//! A warm whisper.cpp server, spawned on demand.
//!
//! Live dictation re-transcribes the whole clip several times per second,
//! and `whisper-cli` can't do that: measured on this machine, a 1s clip and
//! a 2.8s clip BOTH take ~1.05s, because the time is model load, not
//! inference. Spawning it per pass would put a ~1s floor of pure waste
//! under every update.
//!
//! `whisper-server` loads the model once and holds it. The same passes then
//! cost ~0.35s for a 5s clip and ~0.43s for 10s — inference only. That's
//! what makes a live preview feel live.
//!
//! The tradeoff is ~1GB resident while it's warm, so it is NOT started with
//! the app: the first dictation spawns it (paying the ~1s load once) and
//! [`idle_shutdown`] reaps it after [`IDLE_SHUTDOWN`] of disuse.
//!
//! Inference cost scales with clip length (30s → ~1.2s, 60s → ~2.1s), which
//! is why the caller stops issuing live passes on a long clip rather than
//! this module trying to be clever about it.

use std::io::Cursor;
use std::net::TcpStream;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Reap the server after this long without a transcription.
const IDLE_SHUTDOWN: Duration = Duration::from_secs(600);
/// How long to wait for the model to load and the port to accept.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
/// A live pass should normally take well under three seconds. Without an
/// HTTP timeout, one wedged whisper request blocks the sole dictation worker
/// forever while the microphone visibly keeps recording.
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(10);

struct Running {
    child: Child,
    port: u16,
    last_use: Instant,
    /// False between spawn and the port accepting. Tracked because the
    /// startup wait deliberately happens OUTSIDE the lock — see
    /// [`ensure_running`] — so another caller can see a server that exists
    /// but isn't usable yet.
    ready: bool,
}

static SERVER: Mutex<Option<Running>> = Mutex::new(None);

/// PID of the live whisper-server — which is also its process-group id,
/// since [`spawn_server`] gives it a group of its own — or 0 when there is
/// none.
///
/// A plain atomic rather than a read through `SERVER`, because the two
/// hooks that need it ([`handle_term_signal`] and [`kill_child_at_exit`])
/// run on paths where taking a lock is either unsafe or already too late.
///
/// The pid is remembered rather than re-derived, so in principle it could
/// name a recycled process if the server died without us noticing. Every
/// path that observes the exit clears it, which leaves only the window
/// between an unobserved crash and jim's own exit.
static CHILD_PGID: AtomicI32 = AtomicI32::new(0);

/// Previous disposition of each signal we hook (indexed by
/// [`prev_handler_slot`]), captured at install time so we CHAIN to it
/// rather than replace it — see `jim_emacs::native`, which hooks the same
/// signals for the same reason and whichever of the two installs second
/// ends up calling the first.
static PREV_SIG_HANDLERS: [AtomicUsize; 3] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

fn model_path() -> Option<std::path::PathBuf> {
    Some(
        std::path::PathBuf::from(std::env::var("HOME").ok()?)
            .join(".jim/models/ggml-large-v3-turbo.bin"),
    )
}

/// Transcribe mono `samples` captured at `rate` Hz. Blocking — call from a
/// worker thread, never a Bevy system.
///
/// The WAV goes over at the device's native rate; whisper-server resamples
/// to 16k itself (verified with a 48kHz upload), which keeps ffmpeg out of
/// a loop that runs several times a second.
pub fn transcribe(samples: &[f32], rate: u32) -> Result<String, String> {
    let port = ensure_running()?;
    let wav = encode_wav(samples, rate)?;
    match post_inference(port, &wav) {
        Ok(text) => Ok(text),
        Err(first) => {
            eprintln!(
                "[whisper] inference failed on port {port}; replacing server and retrying: {first}"
            );
            discard_server(port);
            let retry_port = ensure_running()?;
            post_inference(retry_port, &wav)
                .map_err(|retry| format!("{first}; retry failed: {retry}"))
        }
    }
}

/// Remove a server that failed an inference, but only if it is still the
/// instance that served that request. Killing it also interrupts any stuck
/// inference before the retry starts a clean process.
fn discard_server(port: u16) {
    let Ok(mut g) = SERVER.lock() else { return };
    if g.as_ref().is_some_and(|r| r.port == port) {
        if let Some(r) = g.take() {
            kill_running(r, "killing failed server");
        }
    }
}

/// Kill the server if it hasn't been used in a while. Called every frame
/// the app is idle; that's an uncontended lock and nothing else, since the
/// dictation worker only holds it for the length of a `transcribe` call.
pub fn idle_shutdown() {
    let Ok(mut g) = SERVER.lock() else { return };
    let idle = match g.as_ref() {
        Some(r) => r.last_use.elapsed() > IDLE_SHUTDOWN,
        None => false,
    };
    if idle {
        if let Some(r) = g.take() {
            kill_running(r, "idle shutdown");
        }
    }
}

/// Kill the server now — on app exit, so ~1GB doesn't outlive the GUI.
pub fn shutdown() {
    let Ok(mut g) = SERVER.lock() else { return };
    if let Some(r) = g.take() {
        kill_running(r, "app shutdown");
    }
}

/// The one kill path: signal the server's whole process group, reap it, and
/// drop the crash-recovery record so a later [`reap_orphans`] has nothing
/// to chase.
///
/// The group rather than the pid alone because the server is its own group
/// leader; whisper-server forks nothing today, but a stray grandchild would
/// otherwise keep the model resident.
fn kill_running(mut r: Running, why: &str) {
    let pid = r.child.id();
    eprintln!("[whisper] {why}: pid={pid} port={}", r.port);
    // SAFETY: a negative pid is the group; SIGKILL to a group we created.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    let _ = r.child.kill();
    let _ = r.child.wait();
    forget_child(pid);
}

/// Note that the server with this pid is gone: clear the pid the exit hooks
/// would otherwise signal, and delete its crash-recovery record.
fn forget_child(pid: u32) {
    let _ = CHILD_PGID.compare_exchange(pid as i32, 0, Ordering::SeqCst, Ordering::SeqCst);
    let Some(dir) = registry_dir() else { return };
    let _ = std::fs::remove_file(dir.join(pid.to_string()));
}

/// Port of the live server, starting it if needed.
///
/// The lock is taken in short bursts and deliberately NOT held across the
/// startup wait. Model load takes ~1s (and is allowed up to
/// [`STARTUP_TIMEOUT`]); holding the lock through it would mean a [`shutdown`]
/// on the main thread — i.e. quitting Jim during your first dictation —
/// blocking the GUI until the load finished.
fn ensure_running() -> Result<u16, String> {
    let port = {
        let mut g = SERVER.lock().map_err(|_| "whisper server lock poisoned")?;
        // Reuse a live, ready one; drop it if it died under us (crash, OOM,
        // manual kill).
        if let Some(r) = g.as_mut() {
            let alive = matches!(r.child.try_wait(), Ok(None));
            if alive && r.ready {
                r.last_use = Instant::now();
                return Ok(r.port);
            }
            if !alive {
                forget_child(r.child.id());
                *g = None;
            }
        }
        if g.is_none() {
            *g = Some(spawn_server()?);
        }
        // Either the one just spawned, or one another caller is still
        // starting — both cases just wait for the same port below.
        g.as_ref()
            .map(|r| r.port)
            .ok_or("whisper server vanished")?
    };

    wait_ready(port)?;

    let mut g = SERVER.lock().map_err(|_| "whisper server lock poisoned")?;
    match g.as_mut() {
        Some(r) if r.port == port => {
            r.ready = true;
            r.last_use = Instant::now();
            Ok(port)
        }
        // Killed while we were waiting (app quit, idle reap).
        _ => Err("whisper-server was shut down during startup".into()),
    }
}

fn spawn_server() -> Result<Running, String> {
    let model = model_path().ok_or("no HOME")?;
    if !model.exists() {
        return Err(format!("whisper model missing: {}", model.display()));
    }
    let port = free_port()?;
    let mut cmd = Command::new("whisper-server");
    cmd.args([
        "-m",
        &model.to_string_lossy(),
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
    ])
    // Its startup chatter (Metal init, model load) is noise in Jim's log.
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    // Its own process group, so every teardown path can signal the server
    // and any grandchild as a unit, and so a terminal signal to jim's group
    // doesn't reach it at some other moment.
    .process_group(0);
    // A Dock-launched Jim inherits launchd's minimal PATH — without this,
    // Homebrew's whisper-server isn't findable.
    if let Some(path) = jim_widget::subprocess::augmented_path() {
        cmd.env("PATH", path);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("whisper-server failed to launch: {e} (is it installed?)"))?;
    eprintln!("[whisper] server spawned: pid={} port={port}", child.id());
    // Arm the teardown paths that `Drop` and `AppExit` never reach, and
    // leave a record for the one path nothing in-process can reach.
    CHILD_PGID.store(child.id() as i32, Ordering::SeqCst);
    install_exit_hooks();
    register_child(child.id(), port);
    Ok(Running {
        child,
        port,
        last_use: Instant::now(),
        ready: false,
    })
}

/// Block until the server accepts connections (it binds only once the model
/// is loaded), or it dies / is killed / we run out of patience.
///
/// Takes the lock only for the liveness peek between connect attempts, so a
/// concurrent [`shutdown`] can always get in.
fn wait_ready(port: u16) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        {
            let mut g = SERVER.lock().map_err(|_| "whisper server lock poisoned")?;
            match g.as_mut() {
                Some(r) if r.port == port => {
                    if let Ok(Some(status)) = r.child.try_wait() {
                        forget_child(r.child.id());
                        *g = None;
                        return Err(format!("whisper-server exited during startup ({status})"));
                    }
                }
                // Someone shut it down (or replaced it) while we waited.
                _ => return Err("whisper-server was shut down during startup".into()),
            }
        }
        if Instant::now() >= deadline {
            return Err("whisper-server didn't come up in time".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ============================================================
// Outliving jim
// ============================================================
//
// `shutdown` runs from an `AppExit` reader, and in practice almost nothing
// reaches it: of 158 spawns in one log there were 3 app shutdowns. SIGTERM
// — what `scripts/dev-restart.sh` sends — is not one of them, because
// bevy's `TerminalCtrlCHandlerPlugin` builds `ctrlc` without its
// `termination` feature and so only turns SIGINT into an `AppExit`; ⌘Q
// terminates through LaunchServices without another `App::update()`; and a
// SIGKILL or a panic-abort observes nothing at all.
//
// Every one of those orphans a ~1GB server onto PID 1, where nothing ever
// reaps it. `dev-restart.sh` looks for whisper children of the GUI it is
// about to kill, which only helps while that GUI is still alive and only
// when jim is restarted through the script at all — a Dock-launched
// session never is.
//
// So the server is torn down from three places instead: `shutdown` on the
// graceful path, a signal handler and an `atexit` hook for the paths where
// jim still gets to run code, and `reap_orphans` at startup for the paths
// where it doesn't.

fn prev_handler_slot(sig: i32) -> Option<usize> {
    match sig {
        libc::SIGTERM => Some(0),
        libc::SIGINT => Some(1),
        libc::SIGHUP => Some(2),
        _ => None,
    }
}

/// SIGTERM/SIGINT/SIGHUP handler: kill the server's process group, then
/// hand off to whatever handler was installed before us — bevy's ctrl-c
/// handler, or `jim_emacs`'s equivalent — so the graceful path this may be
/// stealing still happens. If there was none, restore the default
/// disposition and re-raise so jim dies as it normally would.
///
/// ASYNC-SIGNAL-SAFE: an atomic load, `kill`, `signal`, `raise`, and a call
/// into the previous handler. No allocation, no locks, no `wait` — which is
/// why it leaves a zombie and the registry file behind for `reap_orphans`
/// rather than tidying up here.
extern "C" fn handle_term_signal(sig: i32) {
    kill_child_now();
    let prev = prev_handler_slot(sig)
        .map(|i| PREV_SIG_HANDLERS[i].load(Ordering::SeqCst))
        .unwrap_or(libc::SIG_DFL);
    if prev == libc::SIG_IGN {
        return;
    }
    if prev != libc::SIG_DFL && prev != libc::SIG_ERR {
        // SAFETY: `prev` came from `signal`, so it is a handler of this type.
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(prev) };
        f(sig);
        return;
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// `atexit` hook — the ⌘Q path, where `-[NSApplication terminate:]` calls
/// `exit` without the app loop running another frame.
extern "C" fn kill_child_at_exit() {
    kill_child_now();
}

/// SIGKILL the server's process group if there is one. Shared by the two
/// hooks, so it must stay async-signal-safe.
fn kill_child_now() {
    let pgid = CHILD_PGID.swap(0, Ordering::SeqCst);
    if pgid > 0 {
        // SAFETY: a negative pid is the group; SIGKILL to a group we created.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// Install the signal handlers and the `atexit` hook, once per process,
/// capturing the handlers that were there first so `handle_term_signal` can
/// chain to them.
fn install_exit_hooks() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let h = handle_term_signal as *const () as libc::sighandler_t;
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            let prev = libc::signal(sig, h);
            if let Some(i) = prev_handler_slot(sig) {
                PREV_SIG_HANDLERS[i].store(prev, Ordering::SeqCst);
            }
        }
        libc::atexit(kill_child_at_exit);
    });
}

/// Where a live server records itself, one file per server pid holding the
/// pid of the jim that owns it. Deliberately not the same thing as the
/// `SERVER` mutex: this survives the process, and is only read by the jim
/// that starts next.
fn registry_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".jim/whisper-servers"))
}

fn register_child(pid: u32, port: u16) {
    let Some(dir) = registry_dir() else { return };
    let write = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(pid.to_string()), format!("{}\n", std::process::id())));
    if let Err(e) = write {
        // Not fatal — the server still works, it just can't be recovered if
        // this jim is SIGKILLed. Say so rather than leak in silence.
        eprintln!("[whisper] could not record server pid={pid} port={port} for orphan cleanup: {e}");
    }
}

/// Kill whisper servers left behind by a jim that died without running any
/// of its exit hooks. Called once at startup.
///
/// A record whose owner is still alive belongs to another running jim and
/// is left alone. Everything else is cleared out — but only after checking
/// that the pid is still a whisper-server, since a recorded pid that has
/// been recycled would otherwise name an innocent process.
pub fn reap_orphans() {
    let Some(dir) = registry_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(pid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let owner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<i32>().ok());
        if owner.is_some_and(pid_alive) {
            continue;
        }
        if pid_alive(pid) && proc_name(pid).as_deref() == Some("whisper-server") {
            eprintln!("[whisper] reaping server orphaned by a dead jim: pid={pid}");
            // SAFETY: SIGTERM by group (it is its own leader) and by pid, so
            // this also reaches servers spawned before jim used groups.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 checks for the process without delivering anything.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// The executable name behind a pid, used to make sure a recorded pid still
/// names the server we recorded and not whatever reused the number.
fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `proc_name` writes at most `buf.len()` bytes and returns how
    // many; a non-positive return means it wrote none.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Ask the OS for an unused port by binding one and letting it go.
///
/// Racy in principle — something else could take it in the gap before
/// whisper-server binds — but the alternative (a hardcoded port) collides
/// with a second Jim, or a leftover server, every time.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port: {e}"))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("no local addr: {e}"))
}

/// 16-bit mono WAV in memory. Nothing touches the disk in the live loop.
fn encode_wav(samples: &[f32], rate: u32) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = Cursor::new(Vec::<u8>::new());
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

/// POST the clip to `/inference` as multipart/form-data and return the
/// plain-text transcript. Hand-rolled because ureq 2 has no multipart
/// builder and this needs exactly two fields.
fn post_inference(port: u16, wav: &[u8]) -> Result<String, String> {
    const BOUNDARY: &str = "----jimdictation7f3a1c";
    let mut body: Vec<u8> = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(
        format!(
            "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\n\
             text\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_write(INFERENCE_TIMEOUT)
        .timeout_read(INFERENCE_TIMEOUT)
        .build();
    let resp = agent
        .post(&format!("http://127.0.0.1:{port}/inference"))
        .set(
            "Content-Type",
            &format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .send_bytes(&body)
        .map_err(|e| format!("whisper request failed: {e}"))?;
    resp.into_string()
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("whisper response unreadable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record whose owner is gone, but whose pid has been recycled by
    /// something that is not a whisper-server, must be cleaned up WITHOUT
    /// signalling that process.
    #[test]
    fn reap_orphans_wont_kill_a_recycled_pid() {
        let Some(dir) = registry_dir() else { return };
        std::fs::create_dir_all(&dir).unwrap();

        // A live process that is definitely not whisper-server, recorded
        // under an owner pid that cannot exist.
        let mut victim = Command::new("sleep").arg("30").spawn().unwrap();
        let record = dir.join(victim.id().to_string());
        std::fs::write(&record, format!("{}\n", DEAD_OWNER_PID)).unwrap();

        reap_orphans();

        assert!(
            !record.exists(),
            "a record with a dead owner should be cleared out"
        );
        assert!(
            matches!(victim.try_wait(), Ok(None)),
            "reap_orphans killed a recycled pid that was not a whisper-server"
        );
        let _ = victim.kill();
        let _ = victim.wait();
    }

    /// A record whose owner is still running belongs to another live jim,
    /// and must be left completely alone.
    #[test]
    fn reap_orphans_leaves_a_live_owners_record_alone() {
        let Some(dir) = registry_dir() else { return };
        std::fs::create_dir_all(&dir).unwrap();

        let record = dir.join(format!("{}", DEAD_OWNER_PID + 1));
        // This process stands in for the other jim: it is certainly alive.
        std::fs::write(&record, format!("{}\n", std::process::id())).unwrap();

        reap_orphans();

        assert!(
            record.exists(),
            "reap_orphans deleted a record still owned by a live process"
        );
        let _ = std::fs::remove_file(&record);
    }

    /// A pid no live process can have: `kern.maxproc` is far below this and
    /// pids are allocated below `PID_MAX` (99999).
    const DEAD_OWNER_PID: i32 = 900_000;

    /// The kill both the signal handler and the `atexit` hook perform. They
    /// signal the GROUP, which only reaches anything if the child was
    /// actually given one — drop the `process_group(0)` in `spawn_server`
    /// and this fails.
    #[test]
    fn kill_child_now_kills_the_whole_group() {
        // A group leader with a child of its own, standing in for the
        // server: `sh` is the leader, `sleep` the grandchild.
        let mut leader = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 & echo $!; wait")
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let grandchild: i32 = {
            use std::io::BufRead as _;
            let out = leader.stdout.take().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(out).read_line(&mut line).unwrap();
            line.trim().parse().unwrap()
        };

        CHILD_PGID.store(leader.id() as i32, Ordering::SeqCst);
        kill_child_now();

        let _ = leader.wait();
        assert_eq!(
            CHILD_PGID.load(Ordering::SeqCst),
            0,
            "the pid must be cleared so a second hook can't signal a recycled one"
        );
        // The leader is reaped above; the grandchild had no one to wait for
        // it, so give the kill a moment to land before looking.
        let deadline = Instant::now() + Duration::from_secs(2);
        while pid_alive(grandchild) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pid_alive(grandchild),
            "grandchild {grandchild} survived — the kill did not reach the group"
        );
    }

    /// These share one process-global server, so they must not run
    /// concurrently:
    ///   cargo test -p jim_app --lib dictation::whisper \
    ///       -- --ignored --nocapture --test-threads=1
    fn quiet_tone(rate: u32) -> Vec<f32> {
        (0..rate)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.05)
            .collect()
    }

    /// Quitting Jim during your first dictation must not hang the GUI.
    ///
    /// `shutdown` runs on the main thread; the model load takes ~1s. If the
    /// startup wait held the server lock (it used to), this shutdown would
    /// block for the whole load.
    #[test]
    #[ignore]
    fn shutdown_during_startup_doesnt_block() {
        shutdown();
        let worker = std::thread::spawn(|| {
            let _ = transcribe(&quiet_tone(48_000), 48_000);
        });
        // Land inside the model load, while wait_ready is spinning.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            SERVER.lock().unwrap().as_ref().is_some_and(|r| !r.ready),
            "expected a server mid-startup; without one this test proves nothing"
        );

        let began = Instant::now();
        shutdown();
        let took = began.elapsed();
        assert!(
            took < Duration::from_millis(200),
            "shutdown blocked {took:?} while the server was starting — \
             the startup wait is holding the lock again"
        );
        let _ = worker.join();
    }

    /// Exercises the whole server path against a REAL whisper-server: spawn,
    /// wait for the port, hand-rolled multipart upload, response parse, and
    /// reuse of the warm server on a second call.
    ///
    /// It asserts the plumbing, not the words — a synthetic clip has nothing
    /// to say, so any `Ok` (including an empty transcript) means the request
    /// was well-formed. A malformed multipart body, a wrong content type, or
    /// a botched startup wait all surface here as `Err`.
    ///
    /// Ignored by default: it spawns a server and loads a ~1GB model.
    /// Run with:
    ///   cargo test -p jim_app --lib dictation::whisper -- --ignored --nocapture
    #[test]
    #[ignore]
    fn round_trips_a_clip_through_a_real_server() {
        if model_path().map(|p| !p.exists()).unwrap_or(true) {
            panic!("no whisper model at ~/.jim/models — can't run this test");
        }
        // 1s of quiet 440Hz at 48k: whisper hears nothing meaningful, which
        // is fine — we're testing the transport, not the transcript.
        let rate = 48_000u32;
        let samples = quiet_tone(rate);

        let first = transcribe(&samples, rate).expect("first transcribe should succeed");
        println!("first pass returned: {first:?}");

        // While it is warm it must be recoverable: the record is the only
        // thing that lets the next jim clean up after a SIGKILL.
        let record = registry_dir()
            .unwrap()
            .join(SERVER.lock().unwrap().as_ref().unwrap().child.id().to_string());
        assert!(
            record.exists(),
            "no crash-recovery record at {} — a SIGKILL here would strand the server",
            record.display()
        );

        // Second call must reuse the warm server rather than spawn another.
        let began = Instant::now();
        let second = transcribe(&samples, rate).expect("second transcribe should succeed");
        let warm = began.elapsed();
        println!("warm pass returned {second:?} in {warm:?}");
        assert!(
            warm < Duration::from_secs(3),
            "warm pass took {warm:?} — the server is being respawned per call"
        );

        shutdown();
        assert!(
            SERVER.lock().unwrap().is_none(),
            "shutdown should drop the server"
        );
        assert!(
            !record.exists(),
            "the crash-recovery record outlived the server it describes"
        );
        assert_eq!(
            CHILD_PGID.load(Ordering::SeqCst),
            0,
            "shutdown left a pid the exit hooks would still signal"
        );
    }
}
