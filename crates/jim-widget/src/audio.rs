//! Native audio capture for the audio-recorder widget.
//!
//! Why this exists instead of shelling out to ffmpeg: an `ffmpeg`
//! subprocess that both records to a file AND streams level data back over
//! a pipe couples the two — if the GUI/worker is slow to drain the pipe,
//! ffmpeg blocks on its write and the capture thread stalls, dropping
//! samples (audible pops/dropouts). Here the capture runs on cpal's own
//! realtime CoreAudio thread, writes the WAV directly, and only pushes a
//! tiny downsampled level stream into a shared buffer the widget polls.
//! Recording quality is therefore independent of anything the GUI does.
//!
//! ## Threading
//!
//! `cpal::Stream` is `!Send`, but funct host-fn closures must be
//! `Send + Sync`. So a single **controller thread** owns the host and the
//! active stream; the host fns only touch `Send + Sync` handles (a
//! `Mutex<Sender>` of commands + the shared level/status state). Commands:
//! [`AudioCmd`]. The controller keeps the stream alive in a local
//! `Option<Active>` across `recv()` calls; dropping it stops capture, after
//! which the WAV is finalized.
//!
//! Host fns exposed to scripts (registered in `funct_widget.rs`):
//! - `audio_inputs()` → `[{ id, name }]` available input devices
//! - `audio_record_start(device_name, path)` → bool (""=default device)
//! - `audio_record_stop()` → bool
//! - `audio_levels()` → `[f32]` new 0..1 envelope samples since last call
//! - `audio_recording()` → bool
//! - `audio_status()` → string (last error, or "saved → <path>")

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

/// How many envelope samples to emit per second (independent of cpal's
/// callback buffer size). ~30/s is plenty for a smooth scrolling waveform
/// and keeps the shared buffer tiny.
const LEVELS_PER_SEC: f32 = 30.0;
/// Cap on buffered envelope samples, so a widget that stops polling can't
/// grow this without bound (~13s at 30/s).
const MAX_LEVELS: usize = 400;

/// Watchdog: how long a recording may go WITHOUT the owning widget polling
/// it (via `audio_levels()`) before the controller assumes the widget is
/// gone (closed / hot-reloaded / faulted / crashed) and auto-stops, so the
/// microphone is never held open by an orphaned stream. A recording widget
/// polls every animation tick (~50ms) while `● recording`, so this only
/// fires when nothing is driving the capture anymore. Kept generous so a
/// briefly-janky frame can't false-trip it.
const IDLE_WATCHDOG: Duration = Duration::from_secs(2);
/// How often the controller wakes to check the watchdog while a stream is
/// live (it otherwise blocks on the command channel).
const WATCHDOG_TICK: Duration = Duration::from_millis(250);

/// Commands sent from host-fn closures to the controller thread.
enum AudioCmd {
    Start {
        device: String,
        path: String,
        /// Duplicate a mono input into a stereo (L=R) file, so playback
        /// comes out of both speakers instead of just the left.
        dual: bool,
    },
    Stop,
}

/// Process-global audio state. All fields are `Send + Sync` so the host-fn
/// closures (which must be) can hold an `Arc` of this.
struct AudioState {
    cmd: Mutex<Sender<AudioCmd>>,
    levels: Mutex<VecDeque<f32>>,
    recording: AtomicBool,
    status: Mutex<String>,
    /// Last time the owning widget polled the capture (via `take_levels`) or
    /// (re)started it. The controller's watchdog auto-stops a stream that
    /// hasn't been polled within [`IDLE_WATCHDOG`], so a widget that vanishes
    /// mid-recording can't leave the mic on. See [`touch_keepalive`].
    last_poll: Mutex<Instant>,
    /// True between a [`record_stop`] request and the controller actually
    /// finishing the WAV. See [`wait_until_finalized`].
    finalizing: AtomicBool,
    /// Raw mono PCM for consumers that need the samples themselves rather
    /// than an envelope — live dictation re-transcribes the clip while it's
    /// still being spoken, so it can't wait for the WAV. Off by default:
    /// gated by `pcm_tap` so the recorder/podcast widgets, which only want
    /// levels, don't pay to buffer it. See [`set_pcm_tap`] / [`take_pcm`].
    pcm: Mutex<Vec<f32>>,
    pcm_tap: AtomicBool,
    pcm_rate: AtomicU32,
}

/// Hard bound on the un-drained PCM tap (~150s at 48kHz, ~28MB). A consumer
/// that stops draining drops the OLDEST audio rather than growing forever.
const MAX_PCM_SAMPLES: usize = 48_000 * 150;

static AUDIO: OnceLock<Arc<AudioState>> = OnceLock::new();

/// A live recording the controller thread keeps alive. Dropping `stream`
/// stops capture; the writer is finalized afterwards.
struct Active {
    stream: cpal::Stream,
    writer: Arc<Mutex<Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>>>,
    path: String,
}

fn state() -> &'static Arc<AudioState> {
    AUDIO.get_or_init(|| {
        let (tx, rx) = channel::<AudioCmd>();
        let st = Arc::new(AudioState {
            cmd: Mutex::new(tx),
            levels: Mutex::new(VecDeque::new()),
            recording: AtomicBool::new(false),
            status: Mutex::new(String::new()),
            last_poll: Mutex::new(Instant::now()),
            finalizing: AtomicBool::new(false),
            pcm: Mutex::new(Vec::new()),
            pcm_tap: AtomicBool::new(false),
            pcm_rate: AtomicU32::new(0),
        });
        let controller = st.clone();
        // The controller thread owns the !Send cpal stream for its whole
        // lifetime, serving Start/Stop off the command channel.
        std::thread::Builder::new()
            .name("audio-rec".into())
            .spawn(move || {
                let mut active: Option<Active> = None;
                loop {
                    // Block on the command channel, but only until the next
                    // watchdog tick while a stream is live — so an orphaned
                    // recording (owning widget gone) can't hold the mic open.
                    let timeout = if active.is_some() {
                        WATCHDOG_TICK
                    } else {
                        // No stream: nothing to watchdog, wait a long time.
                        Duration::from_secs(3600)
                    };
                    match rx.recv_timeout(timeout) {
                        Ok(AudioCmd::Start { device, path, dual }) => {
                            // Tear down any prior take first.
                            if let Some(a) = active.take() {
                                finish(a);
                            }
                            touch_keepalive(&controller);
                            match start_stream(&controller, &device, &path, dual) {
                                Ok(a) => {
                                    active = Some(a);
                                    controller.recording.store(true, Ordering::Release);
                                    set_status(&controller, format!("● recording → {path}"));
                                }
                                Err(e) => {
                                    controller.recording.store(false, Ordering::Release);
                                    set_status(&controller, format!("error: {e}"));
                                }
                            }
                        }
                        Ok(AudioCmd::Stop) => {
                            controller.recording.store(false, Ordering::Release);
                            if let Some(a) = active.take() {
                                let p = finish(a);
                                set_status(&controller, format!("saved → {p}"));
                            }
                            // Cleared unconditionally: the watchdog may have
                            // already finalized this clip, leaving nothing to
                            // take — a waiter must not hang on that.
                            controller.finalizing.store(false, Ordering::Release);
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            // Watchdog: the owning widget hasn't polled the
                            // capture within IDLE_WATCHDOG. Assume it's gone and
                            // release the mic rather than leak the stream.
                            if active.is_some() && idle_elapsed(&controller) > IDLE_WATCHDOG {
                                if let Some(a) = active.take() {
                                    let p = finish(a);
                                    controller.recording.store(false, Ordering::Release);
                                    set_status(
                                        &controller,
                                        format!("recording auto-stopped (widget idle) → {p}"),
                                    );
                                    eprintln!(
                                        "[audio] watchdog: no poll for >{}s, released mic (saved {p})",
                                        IDLE_WATCHDOG.as_secs()
                                    );
                                }
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("spawn audio controller thread");
        st
    })
}

fn set_status(st: &AudioState, s: String) {
    if let Ok(mut g) = st.status.lock() {
        *g = s;
    }
}

/// Mark the capture as freshly driven by its owning widget. Resets the
/// watchdog clock. Called on start and on every `take_levels` poll.
fn touch_keepalive(st: &AudioState) {
    if let Ok(mut g) = st.last_poll.lock() {
        *g = Instant::now();
    }
}

/// How long since the capture was last polled/started.
fn idle_elapsed(st: &AudioState) -> Duration {
    st.last_poll
        .lock()
        .map(|g| g.elapsed())
        .unwrap_or_default()
}

/// Stop the stream and finalize the WAV. Returns the file path.
///
/// `pause()` (→ `AudioOutputUnitStop`) is called BEFORE drop, and this is
/// load-bearing: cpal 0.15's macOS **input** stream keeps an internal
/// reference cycle — `add_disconnect_listener` stores a *clone of the
/// Stream* inside its own `StreamInner` — so simply dropping the `Stream`
/// never disposes the underlying `AudioUnit`. The unit stays initialized and
/// the microphone reads as "in use" (the macOS menu-bar indicator stays lit)
/// until the process exits. Explicitly stopping the unit releases the mic
/// immediately; the unit itself still leaks (small, bounded per take), which
/// is a fine trade against holding the mic open. The stop also guarantees no
/// capture callback runs during `finalize()`.
fn finish(a: Active) -> String {
    let Active {
        stream,
        writer,
        path,
    } = a;
    // Release the microphone hardware even though cpal won't dispose the unit.
    if let Err(e) = stream.pause() {
        eprintln!("[audio] pause on stop failed (mic may linger): {e}");
    }
    drop(stream);
    if let Ok(mut g) = writer.lock() {
        if let Some(w) = g.take() {
            let _ = w.finalize();
        }
    }
    path
}

/// Feed the raw-PCM tap, mixing interleaved channels down to mono.
///
/// Called from the realtime audio callback, so it does no allocation beyond
/// the `Vec` push and bails immediately when the tap is off. cpal always
/// delivers whole frames, so mixing per callback never splits a frame.
fn tap_pcm(st: &AudioState, samples: impl Iterator<Item = f32>, channels: u16) {
    if !st.pcm_tap.load(Ordering::Relaxed) {
        return;
    }
    let ch = channels.max(1) as usize;
    let Ok(mut g) = st.pcm.lock() else { return };
    if ch == 1 {
        g.extend(samples);
    } else {
        let mut sum = 0.0f32;
        let mut n = 0usize;
        for s in samples {
            sum += s;
            n += 1;
            if n == ch {
                g.push(sum / ch as f32);
                sum = 0.0;
                n = 0;
            }
        }
    }
    if g.len() > MAX_PCM_SAMPLES {
        let excess = g.len() - MAX_PCM_SAMPLES;
        g.drain(..excess);
    }
}

/// Turn the raw-PCM tap on/off. Enabling clears any stale audio, so a
/// consumer always starts from the moment it asked.
pub fn set_pcm_tap(on: bool) {
    let st = state();
    st.pcm_tap.store(on, Ordering::Release);
    if let Ok(mut g) = st.pcm.lock() {
        g.clear();
        if !on {
            g.shrink_to_fit();
        }
    }
}

/// Drain the mono PCM captured since the last call. Empty unless
/// [`set_pcm_tap`] is on.
pub fn take_pcm() -> Vec<f32> {
    let st = state();
    touch_keepalive(st);
    st.pcm
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

/// Sample rate of the tapped audio, or 0 if nothing has been captured yet.
/// It's the device's native rate — the tap does not resample.
pub fn pcm_rate() -> u32 {
    state().pcm_rate.load(Ordering::Acquire)
}

/// Find an input device by exact name, falling back to the default input.
fn find_device(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    if !name.is_empty() {
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if d.name().map(|n| n == name).unwrap_or(false) {
                    return Some(d);
                }
            }
        }
    }
    host.default_input_device()
}

fn start_stream(
    st: &Arc<AudioState>,
    device_name: &str,
    path: &str,
    dual: bool,
) -> Result<Active, String> {
    let host = cpal::default_host();
    let device = find_device(&host, device_name).ok_or("no input device")?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("no input config: {e}"))?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.config();
    let channels = config.channels;
    let sample_rate = config.sample_rate.0;

    // "Both channels": a mono input written as stereo (each sample to L and
    // R) so playback isn't stuck in the left speaker. Only meaningful for a
    // mono source — a real multi-channel input is written as-is.
    let duplicate = dual && channels == 1;
    let out_channels = if duplicate { 2 } else { channels };

    // Create the WAV (16-bit PCM).
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let spec = hound::WavSpec {
        channels: out_channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let writer = hound::WavWriter::create(path, spec).map_err(|e| format!("create wav: {e}"))?;
    let writer = Arc::new(Mutex::new(Some(writer)));

    // Reset the level buffer for a fresh take.
    if let Ok(mut g) = st.levels.lock() {
        g.clear();
    }
    // Publish the device rate before any callback fires — a PCM consumer
    // needs it to interpret the samples it drains.
    st.pcm_rate.store(sample_rate, Ordering::Release);

    let samples_per_level =
        ((sample_rate as f32 * channels as f32) / LEVELS_PER_SEC).max(1.0) as usize;

    // Only LOG stream errors — never finalize the writer here. cpal can
    // report transient/non-fatal errors mid-stream; finalizing on one would
    // freeze the WAV early while the stream keeps capturing (a truncated
    // recording). The writer is finalized exactly once, on Stop.
    let err_cb = move |e: cpal::StreamError| {
        eprintln!("[audio] stream error: {e}");
    };

    // One data-callback factory per sample format. Each accumulates a
    // sum-of-squares and emits one RMS-derived 0..1 level every
    // `samples_per_level` samples, pushing into the shared ring.
    let stream = match sample_format {
        SampleFormat::F32 => {
            let w = writer.clone();
            let lv = st.clone();
            let mut acc_sq = 0.0f32;
            let mut acc_n = 0usize;
            device
                .build_input_stream(
                    &config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut g) = w.lock() {
                            if let Some(ww) = g.as_mut() {
                                for &s in data {
                                    let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                                    let _ = ww.write_sample(v);
                                    if duplicate {
                                        let _ = ww.write_sample(v);
                                    }
                                }
                            }
                        }
                        tap_pcm(&lv, data.iter().copied(), channels);
                        for &s in data {
                            acc_sq += s * s;
                            acc_n += 1;
                            if acc_n >= samples_per_level {
                                emit_level(&lv, acc_sq, acc_n);
                                acc_sq = 0.0;
                                acc_n = 0;
                            }
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| format!("build stream: {e}"))?
        }
        SampleFormat::I16 => {
            let w = writer.clone();
            let lv = st.clone();
            let mut acc_sq = 0.0f32;
            let mut acc_n = 0usize;
            device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut g) = w.lock() {
                            if let Some(ww) = g.as_mut() {
                                for &s in data {
                                    let _ = ww.write_sample(s);
                                    if duplicate {
                                        let _ = ww.write_sample(s);
                                    }
                                }
                            }
                        }
                        tap_pcm(&lv, data.iter().map(|&s| s as f32 / 32768.0), channels);
                        for &s in data {
                            let f = s as f32 / 32768.0;
                            acc_sq += f * f;
                            acc_n += 1;
                            if acc_n >= samples_per_level {
                                emit_level(&lv, acc_sq, acc_n);
                                acc_sq = 0.0;
                                acc_n = 0;
                            }
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| format!("build stream: {e}"))?
        }
        SampleFormat::U16 => {
            let w = writer.clone();
            let lv = st.clone();
            let mut acc_sq = 0.0f32;
            let mut acc_n = 0usize;
            device
                .build_input_stream(
                    &config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut g) = w.lock() {
                            if let Some(ww) = g.as_mut() {
                                for &s in data {
                                    let v = (s as i32 - 32768) as i16;
                                    let _ = ww.write_sample(v);
                                    if duplicate {
                                        let _ = ww.write_sample(v);
                                    }
                                }
                            }
                        }
                        tap_pcm(
                            &lv,
                            data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0),
                            channels,
                        );
                        for &s in data {
                            let f = (s as f32 - 32768.0) / 32768.0;
                            acc_sq += f * f;
                            acc_n += 1;
                            if acc_n >= samples_per_level {
                                emit_level(&lv, acc_sq, acc_n);
                                acc_sq = 0.0;
                                acc_n = 0;
                            }
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| format!("build stream: {e}"))?
        }
        other => return Err(format!("unsupported sample format: {other:?}")),
    };

    stream.play().map_err(|e| format!("play: {e}"))?;
    Ok(Active {
        stream,
        writer,
        path: path.to_string(),
    })
}

/// Convert an accumulated sum-of-squares to a 0..1 envelope value (dB
/// mapped over [-60, 0]) and push it into the shared ring.
fn emit_level(st: &AudioState, sum_sq: f32, n: usize) {
    let rms = (sum_sq / n as f32).sqrt();
    let db = 20.0 * (rms + 1e-9).log10();
    let norm = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    if let Ok(mut g) = st.levels.lock() {
        g.push_back(norm);
        while g.len() > MAX_LEVELS {
            g.pop_front();
        }
    }
}

// ---- public API used by the funct host-fn registrations ------------------

/// Available input devices as `(id, name)` pairs. cpal identifies devices
/// by name, so id == name here.
pub fn inputs() -> Vec<(String, String)> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Ok(name) = d.name() {
                out.push((name.clone(), name));
            }
        }
    }
    out
}

/// Begin recording `device` (empty = default) to `path`. Returns false if
/// the controller channel is gone (never, in practice).
///
/// `recording` is flipped true SYNCHRONOUSLY here, before the controller
/// thread has actually built the stream — otherwise a caller polling
/// `is_recording()` right after start (e.g. on its first reactive tick)
/// would see false and assume the take ended. The controller flips it back
/// to false only if the stream fails to build.
pub fn record_start(device: &str, path: &str, dual: bool) -> bool {
    let st = state();
    st.recording.store(true, Ordering::Release);
    // Prime the watchdog so a slow first poll can't trip it right after start.
    touch_keepalive(st);
    let sent = st
        .cmd
        .lock()
        .ok()
        .map(|tx| {
            tx.send(AudioCmd::Start {
                device: device.to_string(),
                path: path.to_string(),
                dual,
            })
            .is_ok()
        })
        .unwrap_or(false);
    if !sent {
        st.recording.store(false, Ordering::Release);
    }
    sent
}

/// Stop the current recording and finalize the file. Flips `recording`
/// false synchronously so a poller sees the stop immediately; the
/// controller finalizes the WAV asynchronously.
pub fn record_stop() -> bool {
    let st = state();
    st.recording.store(false, Ordering::Release);
    st.finalizing.store(true, Ordering::Release);
    let sent = st
        .cmd
        .lock()
        .ok()
        .map(|tx| tx.send(AudioCmd::Stop).is_ok())
        .unwrap_or(false);
    if !sent {
        // Nobody will clear it — don't strand a waiter.
        st.finalizing.store(false, Ordering::Release);
    }
    sent
}

/// Block until a [`record_stop`] has actually finished writing the WAV.
/// Returns false on timeout.
///
/// [`record_stop`] only *asks* the controller to stop; `finish` drops the
/// stream and writes the WAV header on the controller thread a moment
/// later. Anything that then READS the file (ffmpeg, whisper) must wait for
/// this first, or it can catch a clip whose header still claims zero
/// samples. Blocking — call it off the main thread.
pub fn wait_until_finalized(timeout: Duration) -> bool {
    let st = state();
    let deadline = Instant::now() + timeout;
    while st.finalizing.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Drain and return all envelope samples accumulated since the last call.
pub fn take_levels() -> Vec<f32> {
    let st = state();
    // A poll is the widget's heartbeat — keep the watchdog from stopping a
    // recording that's still being actively driven.
    touch_keepalive(st);
    if let Ok(mut g) = st.levels.lock() {
        g.drain(..).collect()
    } else {
        Vec::new()
    }
}

pub fn is_recording() -> bool {
    state().recording.load(Ordering::Acquire)
}

pub fn status() -> String {
    state()
        .status
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}
