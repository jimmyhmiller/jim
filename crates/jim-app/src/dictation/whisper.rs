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
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Reap the server after this long without a transcription.
const IDLE_SHUTDOWN: Duration = Duration::from_secs(600);
/// How long to wait for the model to load and the port to accept.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);

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

fn model_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var("HOME").ok()?).join(".jim/models/ggml-large-v3-turbo.bin"))
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
    post_inference(port, wav)
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
        if let Some(mut r) = g.take() {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }
}

/// Kill the server now — on app exit, so ~1GB doesn't outlive the GUI.
pub fn shutdown() {
    let Ok(mut g) = SERVER.lock() else { return };
    if let Some(mut r) = g.take() {
        let _ = r.child.kill();
        let _ = r.child.wait();
    }
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
                *g = None;
            }
        }
        if g.is_none() {
            *g = Some(spawn_server()?);
        }
        // Either the one just spawned, or one another caller is still
        // starting — both cases just wait for the same port below.
        g.as_ref().map(|r| r.port).ok_or("whisper server vanished")?
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
    .stderr(Stdio::null());
    // A Dock-launched Jim inherits launchd's minimal PATH — without this,
    // Homebrew's whisper-server isn't findable.
    if let Some(path) = jim_widget::subprocess::augmented_path() {
        cmd.env("PATH", path);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("whisper-server failed to launch: {e} (is it installed?)"))?;
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

/// Ask the OS for an unused port by binding one and letting it go.
///
/// Racy in principle — something else could take it in the gap before
/// whisper-server binds — but the alternative (a hardcoded port) collides
/// with a second Jim, or a leftover server, every time.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("no free port: {e}"))?;
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
fn post_inference(port: u16, wav: Vec<u8>) -> Result<String, String> {
    const BOUNDARY: &str = "----jimdictation7f3a1c";
    let mut body: Vec<u8> = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&wav);
    body.extend_from_slice(
        format!(
            "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\n\
             text\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );

    let resp = ureq::post(&format!("http://127.0.0.1:{port}/inference"))
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
    }
}
