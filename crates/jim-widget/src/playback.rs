//! Native audio playback for the podcast editor.
//!
//! ## Why this exists instead of shelling out to `ffplay`
//!
//! The old path spawned `ffplay` and the widget had NO way to know where that
//! subprocess actually was in the audio. So the word-highlight position was
//! *estimated* from wall-clock time minus a hardcoded 90 ms "startup latency"
//! fudge — a guess that drifted (real startup jitter, `atempo` buffering, a
//! coarse 50 ms tick), so the highlight never matched the audio. Multi-clip
//! EDL playback was worse: it pre-rendered a WAV with ffmpeg first, adding a
//! variable delay the estimator couldn't see.
//!
//! Here the cpal output callback knows EXACTLY how many samples have hit the
//! speaker, so [`pos_ms`] is sample-accurate source-time — the highlight tracks
//! the real playhead. The EDL (kept clips) is spliced with short crossfades on
//! the fly, and variable speed uses pitch-preserving WSOLA time-stretch, so
//! there is no ffmpeg render and no fudge factor anywhere.
//!
//! ## Threading
//!
//! Three roles, mirroring `audio.rs`'s capture controller pattern:
//! - **controller thread** — owns the `!Send` cpal `Stream` for its lifetime,
//!   serving `Start`/`Stop` off a command channel. On `Start` it builds the
//!   spliced [`Edited`] timeline, spawns the generator, and opens the stream.
//! - **generator thread** — runs WSOLA / EDL splicing, producing finalized
//!   output samples (each tagged with its source-ms) into a bounded ring. It
//!   reads `speed`/`seek` live off atomics so those are instant without a
//!   stream rebuild, and backs off when the ring is full.
//! - **cpal callback** (its own realtime thread) — just drains the ring into
//!   the device buffer and publishes the source-ms of the last sample it
//!   played to [`pos_ms`]. Kept deliberately light so WSOLA can never cause an
//!   xrun.
//!
//! Host fns exposed to scripts (registered in `funct_widget.rs`):
//! - `audio_play_load(path)`            → kick off async decode of `path`
//! - `audio_play_ready(path)` → bool    → decoded buffer for `path` is cached
//! - `audio_play_start(start_ms, clips_json, speed)` → bool — play the loaded
//!   source over the EDL `clips` (JSON `[{from,to}]` ms) from `start_ms`
//! - `audio_play_seek(src_ms)`          → live, instant re-position
//! - `audio_play_set_speed(speed)`      → live tempo change (pitch-preserving)
//! - `audio_play_stop()`
//! - `audio_play_pos()` → f64           → current SOURCE-ms (sample-accurate)
//! - `audio_playing()` → bool
//! - `audio_play_finished()` → bool     → reached the end of the timeline

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

/// `seek_ms` sentinel meaning "no pending seek".
const NO_SEEK: i64 = i64::MIN;
/// Crossfade length at EDL clip seams, in milliseconds. Long enough to mask a
/// hard cut's click, short enough not to smear word onsets.
const XFADE_MS: f64 = 10.0;
/// Ring high-watermark, in seconds of buffered output. The generator parks
/// when the ring holds more than this, so we stay ~one tick ahead of the
/// device without running far enough ahead to make a seek feel laggy.
const RING_HIGH_SEC: f64 = 0.20;

// ---- shared state ----------------------------------------------------------

/// One decoded source: mono f32 PCM at `rate` Hz (the device's output rate, so
/// the generator never has to resample). `samples` is `Arc` so the generator
/// can borrow the (large) buffer without copying it.
struct Source {
    path: String,
    samples: Arc<Vec<f32>>,
    rate: u32,
    /// True while the background ffmpeg decode is still running.
    loading: bool,
    /// True if the decode finished but failed (bad file / ffmpeg missing). The
    /// widget polls [`is_failed`] so "preparing…" can't hang forever.
    failed: bool,
}

/// Process-global playback state. Every field is `Send + Sync` so the host-fn
/// closures and the generator/callback threads can all hold an `Arc` of it.
struct PlayState {
    cmd: Mutex<Sender<PlayCmd>>,
    source: Mutex<Option<Source>>,
    /// Device output sample rate, probed once. Decode targets this so playback
    /// is resample-free.
    out_rate: AtomicU64,
    /// Source-ms of the last sample the callback handed to the device — the
    /// sample-accurate playhead the widget reads. Stored as `f64` bits.
    pos_ms: AtomicU64,
    playing: AtomicBool,
    /// Set by the generator when it runs off the end of the timeline.
    finished: AtomicBool,
    /// Live tempo (pitch-preserving), `f64` bits. 1.0 = normal.
    speed_bits: AtomicU64,
    /// Pending live seek target in source-ms (rounded), or [`NO_SEEK`]. The
    /// generator swaps it out each block.
    seek_ms: AtomicI64,
    /// Tells the generator thread to exit (set on teardown).
    gen_stop: AtomicBool,
    /// Finalized output: `(sample, source_ms)` pairs. The generator pushes,
    /// the callback drains. Tagging every sample with its source time is how
    /// the callback reports an exact playhead for whatever it actually plays.
    ring: Mutex<VecDeque<(f32, f32)>>,
}

enum PlayCmd {
    Start {
        clips: Vec<(f64, f64)>,
        start_ms: f64,
        speed: f64,
    },
    Stop,
}

static PLAY: OnceLock<Arc<PlayState>> = OnceLock::new();

fn f64_to_bits_store(a: &AtomicU64, v: f64) {
    a.store(v.to_bits(), Ordering::Release);
}
fn f64_load(a: &AtomicU64) -> f64 {
    f64::from_bits(a.load(Ordering::Acquire))
}

fn state() -> &'static Arc<PlayState> {
    PLAY.get_or_init(|| {
        let (tx, rx) = channel::<PlayCmd>();
        // Probe the default output device's rate up front so decode produces
        // PCM at exactly the rate the stream will run — no resampling later.
        let out_rate = cpal::default_host()
            .default_output_device()
            .and_then(|d| d.default_output_config().ok())
            .map(|c| c.sample_rate().0)
            .unwrap_or(48_000);
        let st = Arc::new(PlayState {
            cmd: Mutex::new(tx),
            source: Mutex::new(None),
            out_rate: AtomicU64::new(out_rate as u64),
            pos_ms: AtomicU64::new(0f64.to_bits()),
            playing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            speed_bits: AtomicU64::new(1.0f64.to_bits()),
            seek_ms: AtomicI64::new(NO_SEEK),
            gen_stop: AtomicBool::new(false),
            ring: Mutex::new(VecDeque::new()),
        });
        let ctl = st.clone();
        std::thread::Builder::new()
            .name("audio-play".into())
            .spawn(move || controller(ctl, rx))
            .expect("spawn audio playback controller");
        st
    })
}

// ---- controller ------------------------------------------------------------

/// Owns the `!Send` cpal output stream and the generator handle for one take.
struct Active {
    stream: cpal::Stream,
    gen_thread: Option<std::thread::JoinHandle<()>>,
}

fn controller(st: Arc<PlayState>, rx: std::sync::mpsc::Receiver<PlayCmd>) {
    let mut active: Option<Active> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            PlayCmd::Start {
                clips,
                start_ms,
                speed,
            } => {
                teardown(&st, active.take());
                match start_take(&st, clips, start_ms, speed) {
                    Ok(a) => active = Some(a),
                    Err(e) => {
                        st.playing.store(false, Ordering::Release);
                        eprintln!("[playback] start failed: {e}");
                    }
                }
            }
            PlayCmd::Stop => {
                teardown(&st, active.take());
                st.playing.store(false, Ordering::Release);
            }
        }
    }
}

/// Stop the generator, drop the stream, and clear the ring. Order matters: the
/// generator is told to exit and joined FIRST so it can't push into a ring we
/// then clear, and the stream is dropped (cpal joins its callback) before we
/// return so no callback runs against torn-down state.
fn teardown(st: &PlayState, active: Option<Active>) {
    if let Some(mut a) = active {
        st.gen_stop.store(true, Ordering::Release);
        if let Some(h) = a.gen_thread.take() {
            let _ = h.join();
        }
        drop(a.stream);
    }
    if let Ok(mut g) = st.ring.lock() {
        g.clear();
    }
}

fn start_take(
    st: &Arc<PlayState>,
    clips: Vec<(f64, f64)>,
    start_ms: f64,
    speed: f64,
) -> Result<Active, String> {
    let (samples, rate) = {
        let g = st.source.lock().map_err(|_| "source poisoned")?;
        match &*g {
            Some(s) if !s.loading && !s.samples.is_empty() => (s.samples.clone(), s.rate),
            Some(_) => return Err("source still decoding".into()),
            None => return Err("no source loaded".into()),
        }
    };
    let edited = Edited::build(&samples, rate, &clips, XFADE_MS);
    if edited.len == 0 {
        return Err("empty timeline".into());
    }

    // Reset live controls + flags for the fresh take, then prime the playhead.
    st.gen_stop.store(false, Ordering::Release);
    st.finished.store(false, Ordering::Release);
    st.playing.store(true, Ordering::Release);
    st.seek_ms.store(NO_SEEK, Ordering::Release);
    f64_to_bits_store(&st.speed_bits, speed.clamp(0.25, 4.0));
    {
        let mut g = st.ring.lock().map_err(|_| "ring poisoned")?;
        g.clear();
    }

    let start_edit = edited.edit_at_src_ms(start_ms);
    f64_to_bits_store(&st.pos_ms, edited.src_ms_at(start_edit) as f64);

    // Spawn the generator that fills the ring from the spliced timeline.
    let gst = st.clone();
    let gen_handle = std::thread::Builder::new()
        .name("audio-play-gen".into())
        .spawn(move || generator(gst, edited, start_edit))
        .map_err(|e| format!("spawn generator: {e}"))?;

    // Open the output stream (its callback just drains the ring).
    let stream = build_output_stream(st.clone())?;
    stream.play().map_err(|e| format!("play: {e}"))?;
    Ok(Active {
        stream,
        gen_thread: Some(gen_handle),
    })
}

// ---- output stream (realtime callback) -------------------------------------

fn build_output_stream(st: Arc<PlayState>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("no output config: {e}"))?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.config();
    let channels = config.channels as usize;
    let err = |e: cpal::StreamError| eprintln!("[playback] stream error: {e}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let st = st.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    fill(&st, data, channels, |v, out| *out = v);
                },
                err,
                None,
            )
        }
        SampleFormat::I16 => {
            let st = st.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    fill(&st, data, channels, |v, out| {
                        *out = (v.clamp(-1.0, 1.0) * 32767.0) as i16
                    });
                },
                err,
                None,
            )
        }
        SampleFormat::U16 => {
            let st = st.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                    fill(&st, data, channels, |v, out| {
                        *out = ((v.clamp(-1.0, 1.0) * 32767.0) as i32 + 32768) as u16
                    });
                },
                err,
                None,
            )
        }
        other => return Err(format!("unsupported output format: {other:?}")),
    }
    .map_err(|e| format!("build output stream: {e}"))?;
    Ok(stream)
}

/// Drain `frames = data.len()/channels` mono samples from the ring, fanning
/// each out to every channel, and publish the source-ms of the last real
/// sample played. An empty ring (generator behind, or finished) yields
/// silence. The lock is held only to pop into a small local buffer.
fn fill<T: Copy>(st: &PlayState, data: &mut [T], channels: usize, write: impl Fn(f32, &mut T)) {
    if channels == 0 {
        return;
    }
    let frames = data.len() / channels;
    let mut last_ms: Option<f32> = None;
    let mut popped: Vec<(f32, f32)> = Vec::with_capacity(frames);
    if let Ok(mut ring) = st.ring.lock() {
        for _ in 0..frames {
            match ring.pop_front() {
                Some(p) => popped.push(p),
                None => break,
            }
        }
    }
    for (f, frame) in data.chunks_mut(channels).enumerate() {
        let v = match popped.get(f) {
            Some(&(s, ms)) => {
                last_ms = Some(ms);
                s
            }
            None => 0.0,
        };
        for slot in frame.iter_mut() {
            write(v, slot);
        }
    }
    if let Some(ms) = last_ms {
        f64_to_bits_store(&st.pos_ms, ms as f64);
    }
}

// ---- the spliced, source-mapped timeline -----------------------------------

/// One kept clip placed on the edited timeline. `edit_start`/`len` are in
/// samples (relative to [`Edited::base`]); `src_start_ms` is where this clip
/// begins in the ORIGINAL file, so any edited sample maps back to source-time.
struct ClipMark {
    edit_start: usize,
    len: usize,
    src_start_ms: f64,
}

/// The kept clips concatenated (with seam crossfades) into one mono signal,
/// plus the map back to source-time. To avoid copying the whole file in the
/// common single-clip case (just head/tail trimmed, or untouched), `buf` is the
/// shared source `Arc` itself and `base` offsets into it — zero extra memory.
/// Only a genuine multi-clip EDL (internal cuts) allocates a fresh spliced
/// buffer.
struct Edited {
    buf: Arc<Vec<f32>>,
    base: usize,
    len: usize,
    rate: u32,
    marks: Vec<ClipMark>,
}

impl Edited {
    #[inline]
    fn get(&self, i: usize) -> f32 {
        // Callers guard `i < len`; this stays branch-light on the hot path.
        self.buf[self.base + i]
    }

    fn build(samples: &Arc<Vec<f32>>, rate: u32, clips: &[(f64, f64)], xfade_ms: f64) -> Edited {
        let ms_to_s = |ms: f64| ((ms / 1000.0) * rate as f64).round().max(0.0) as usize;
        let n = samples.len();
        // Normalize clips to in-bounds sample ranges, dropping empties.
        let ranges: Vec<(usize, usize, f64)> = clips
            .iter()
            .filter_map(|&(from, to)| {
                let a = ms_to_s(from).min(n);
                let b = ms_to_s(to).min(n);
                if b > a {
                    Some((a, b, from))
                } else {
                    None
                }
            })
            .collect();

        if ranges.len() == 1 {
            // Fast path: a single contiguous kept span IS source[a..b]. Reuse
            // the source buffer directly — no allocation, no copy.
            let (a, b, from_ms) = ranges[0];
            return Edited {
                buf: samples.clone(),
                base: a,
                len: b - a,
                rate,
                marks: vec![ClipMark {
                    edit_start: 0,
                    len: b - a,
                    src_start_ms: from_ms,
                }],
            };
        }

        // Multi-clip: concatenate with an equal-power-ish triangular crossfade
        // over the seam so hard cuts don't click. Each new clip overlaps the
        // last `xf` samples already written.
        let xf = ms_to_s(xfade_ms);
        let mut out: Vec<f32> = Vec::new();
        let mut marks: Vec<ClipMark> = Vec::new();
        for (a, b, from_ms) in ranges {
            let clip_len = b - a;
            if out.is_empty() {
                marks.push(ClipMark {
                    edit_start: 0,
                    len: clip_len,
                    src_start_ms: from_ms,
                });
                out.extend_from_slice(&samples[a..b]);
                continue;
            }
            let ov = xf.min(out.len()).min(clip_len);
            let seam = out.len() - ov; // edited index where this clip begins
            // Blend the overlap: fade the tail of `out` out, the head of the
            // new clip in.
            for k in 0..ov {
                let t = (k as f32 + 1.0) / (ov as f32 + 1.0);
                let prev = out[seam + k];
                let cur = samples[a + k];
                out[seam + k] = prev * (1.0 - t) + cur * t;
            }
            // Append the remainder of the new clip past the overlap.
            out.extend_from_slice(&samples[(a + ov)..b]);
            marks.push(ClipMark {
                edit_start: seam,
                len: clip_len,
                src_start_ms: from_ms,
            });
        }
        let len = out.len();
        Edited {
            buf: Arc::new(out),
            base: 0,
            len,
            rate,
            marks,
        }
    }

    /// Edited-sample index → source-ms. Searches clips back-to-front so a
    /// sample inside a seam overlap resolves to the later (incoming) clip.
    fn src_ms_at(&self, i: usize) -> f64 {
        for m in self.marks.iter().rev() {
            if i >= m.edit_start {
                let off = (i - m.edit_start) as f64;
                return m.src_start_ms + off / self.rate as f64 * 1000.0;
            }
        }
        self.marks.first().map(|m| m.src_start_ms).unwrap_or(0.0)
    }

    /// Source-ms → edited-sample index. If `src_ms` falls in a removed gap,
    /// snap to the start of the next kept clip; past the end, clamp to `len`.
    fn edit_at_src_ms(&self, src_ms: f64) -> usize {
        for m in &self.marks {
            let clip_ms = m.len as f64 / self.rate as f64 * 1000.0;
            let end_ms = m.src_start_ms + clip_ms;
            if src_ms < m.src_start_ms {
                return m.edit_start; // inside a removed gap → next clip
            }
            if src_ms <= end_ms {
                let off = ((src_ms - m.src_start_ms) / 1000.0 * self.rate as f64).round() as usize;
                return (m.edit_start + off).min(self.len);
            }
        }
        self.len
    }
}

// ---- generator: WSOLA + EDL, filling the ring ------------------------------

/// WSOLA (Waveform-Similarity Overlap-Add) pitch-preserving time-stretch over
/// the [`Edited`] timeline. Window/hop sized from the device rate.
struct Wsola {
    w: usize,         // analysis/synthesis window length
    hs: usize,        // synthesis hop (output advance per frame) = w/2
    delta: usize,     // cross-correlation search radius around the ideal hop
    hann: Vec<f32>,   // synthesis window
    ola: Vec<f32>,    // length-w overlap-add accumulator of not-yet-emitted output
    read: f64,        // current analysis read position, in edited samples
}

impl Wsola {
    fn new(rate: u32) -> Wsola {
        let w = ((0.030 * rate as f64) as usize).max(8) & !1; // ~30ms, even
        let hs = w / 2;
        let delta = ((0.010 * rate as f64) as usize).max(1); // ~10ms search
        let hann: Vec<f32> = (0..w)
            .map(|k| {
                let x = std::f32::consts::PI * k as f32 / (w as f32 - 1.0);
                let s = x.sin();
                s * s // sin² Hann; at 50% overlap consecutive windows sum to 1
            })
            .collect();
        Wsola {
            w,
            hs,
            delta,
            hann,
            ola: vec![0.0; w],
            read: 0.0,
        }
    }

    fn reset_to(&mut self, edit_pos: f64) {
        self.read = edit_pos;
        for v in self.ola.iter_mut() {
            *v = 0.0;
        }
    }

    /// Best-match analysis offset near `read + ha`: the candidate window most
    /// cross-correlated with the natural continuation of the last frame. This
    /// phase-alignment is what keeps the pitch intact through the stretch.
    fn best_offset(&self, ed: &Edited, ha: f64) -> usize {
        let ref_base = self.read as isize + self.hs as isize;
        if ref_base < 0 {
            return (self.read + ha).max(0.0) as usize;
        }
        let ref_base = ref_base as usize;
        if ref_base + self.w >= ed.len {
            return ((self.read + ha) as usize).min(ed.len.saturating_sub(self.w + 1));
        }
        let ideal = (self.read + ha).max(0.0) as isize;
        let lo = (ideal - self.delta as isize).max(0);
        let hi = (ideal + self.delta as isize).max(0);
        let mut best = ideal.max(0) as usize;
        let mut best_score = f32::NEG_INFINITY;
        // Stride the correlation: voice is band-limited, so every-other sample
        // is plenty and halves the search cost in the realtime-adjacent thread.
        let stride = 2;
        let mut cand = lo;
        while cand <= hi {
            let cu = cand as usize;
            if cu + self.w < ed.len {
                let mut score = 0.0f32;
                let mut k = 0;
                while k < self.w {
                    score += ed.get(cu + k) * ed.get(ref_base + k);
                    k += stride;
                }
                if score > best_score {
                    best_score = score;
                    best = cu;
                }
            }
            cand += 1;
        }
        best
    }
}

/// Generator loop: produce finalized output samples (tagged with source-ms)
/// into the ring, honoring live seek/speed and backing off when the ring is
/// full. Exits on `gen_stop` or when the timeline is exhausted.
fn generator(st: Arc<PlayState>, ed: Edited, start_edit: usize) {
    let rate = ed.rate;
    let ring_high = (RING_HIGH_SEC * rate as f64) as usize;
    let mut wsola = Wsola::new(rate);
    let mut pos_edit: f64 = start_edit as f64; // edited read position (1× path)
    wsola.reset_to(pos_edit);

    loop {
        if st.gen_stop.load(Ordering::Acquire) {
            return;
        }
        // Live seek: reposition and drop already-buffered audio so the jump is
        // immediate rather than after the ring drains.
        let sk = st.seek_ms.swap(NO_SEEK, Ordering::AcqRel);
        if sk != NO_SEEK {
            let target = ed.edit_at_src_ms(sk as f64);
            pos_edit = target as f64;
            wsola.reset_to(pos_edit);
            if let Ok(mut ring) = st.ring.lock() {
                ring.clear();
            }
        }
        // Backpressure: stay ~one tick ahead, no further.
        let queued = st.ring.lock().map(|r| r.len()).unwrap_or(0);
        if queued >= ring_high {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }

        let speed = f64_load(&st.speed_bits);
        let done = if (speed - 1.0).abs() < 0.01 {
            produce_passthrough(&st, &ed, &mut pos_edit, &mut wsola)
        } else {
            produce_wsola(&st, &ed, speed, &mut pos_edit, &mut wsola)
        };
        if done {
            st.finished.store(true, Ordering::Release);
            st.playing.store(false, Ordering::Release);
            return;
        }
    }
}

/// 1× path: copy a block straight through (sample-perfect, no WSOLA artifacts).
/// Keeps `wsola.read` synced so a later speed change resumes cleanly.
fn produce_passthrough(
    st: &PlayState,
    ed: &Edited,
    pos_edit: &mut f64,
    wsola: &mut Wsola,
) -> bool {
    const BLOCK: usize = 512;
    let start = *pos_edit as usize;
    if start >= ed.len {
        return true;
    }
    let end = (start + BLOCK).min(ed.len);
    if let Ok(mut ring) = st.ring.lock() {
        for i in start..end {
            ring.push_back((ed.get(i), ed.src_ms_at(i) as f32));
        }
    }
    *pos_edit = end as f64;
    wsola.read = *pos_edit;
    end >= ed.len
}

/// WSOLA path: emit one synthesis frame (`hs` samples) of pitch-preserving
/// time-stretched output, then advance the analysis position to the best-match
/// frame near the ideal hop `ha = hs * speed`.
fn produce_wsola(
    st: &PlayState,
    ed: &Edited,
    speed: f64,
    pos_edit: &mut f64,
    wsola: &mut Wsola,
) -> bool {
    let ri = wsola.read as usize;
    if ri + wsola.w >= ed.len {
        return true;
    }
    // Overlap-add the windowed current frame.
    for k in 0..wsola.w {
        wsola.ola[k] += ed.get(ri + k) * wsola.hann[k];
    }
    // Emit the finalized prefix (ola[0..hs]); tag with this frame's source-ms.
    let src_ms = ed.src_ms_at(ri) as f32;
    if let Ok(mut ring) = st.ring.lock() {
        for k in 0..wsola.hs {
            ring.push_back((wsola.ola[k], src_ms));
        }
    }
    // Slide the accumulator left by hs, zero-filling the tail.
    for k in 0..(wsola.w - wsola.hs) {
        wsola.ola[k] = wsola.ola[k + wsola.hs];
    }
    for k in (wsola.w - wsola.hs)..wsola.w {
        wsola.ola[k] = 0.0;
    }
    // Advance to the phase-aligned next analysis frame.
    let ha = wsola.hs as f64 * speed;
    let next = wsola.best_offset(ed, ha);
    wsola.read = next as f64;
    *pos_edit = wsola.read;
    wsola.read as usize + wsola.w >= ed.len
}

// ---- decode (background) ---------------------------------------------------

/// Decode any audio file to mono f32 PCM at `rate` Hz via ffmpeg. One-time,
/// off the playback path — the timing-critical work is all native; this just
/// gets samples into memory in whatever format the source happens to be.
fn decode(path: &str, rate: u32) -> Result<Vec<f32>, String> {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-v", "error", "-i", path, "-ar", &rate.to_string(), "-ac", "1", "-f", "f32le", "-",
    ]);
    // Finder/Dock-launched `.app` inherits launchd's minimal PATH (no
    // /opt/homebrew/bin), so resolve ffmpeg via the same augmented PATH the
    // subprocess host uses, or playback silently fails only from the Dock.
    if let Some(p) = crate::subprocess::augmented_path() {
        cmd.env("PATH", p);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn ffmpeg: {e}"))?;
    let mut buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg exit {:?}", status.code()));
    }
    // Reinterpret the little-endian f32 byte stream as samples.
    let mut out = Vec::with_capacity(buf.len() / 4);
    for chunk in buf.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

// ---- public API (registered as host fns) -----------------------------------

/// Kick off an async decode of `path` (idempotent: a no-op if it is already
/// loaded or currently loading). The widget polls [`is_ready`].
pub fn load(path: &str) {
    let st = state();
    let rate = st.out_rate.load(Ordering::Acquire) as u32;
    {
        let mut g = match st.source.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(s) = &*g {
            if s.path == path && (s.loading || !s.samples.is_empty()) {
                return; // already loaded or in flight
            }
        }
        *g = Some(Source {
            path: path.to_string(),
            samples: Arc::new(Vec::new()),
            rate,
            loading: true,
            failed: false,
        });
    }
    let path = path.to_string();
    let stc = st.clone();
    std::thread::Builder::new()
        .name("audio-decode".into())
        .spawn(move || {
            let result = decode(&path, rate);
            let mut g = match stc.source.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            // Only store if the user hasn't switched to a different source.
            let still_wanted = matches!(&*g, Some(s) if s.path == path);
            if !still_wanted {
                return;
            }
            match result {
                Ok(samples) => {
                    *g = Some(Source {
                        path,
                        samples: Arc::new(samples),
                        rate,
                        loading: false,
                        failed: false,
                    })
                }
                Err(e) => {
                    eprintln!("[playback] decode failed: {e}");
                    *g = Some(Source {
                        path,
                        samples: Arc::new(Vec::new()),
                        rate,
                        loading: false,
                        failed: true,
                    });
                }
            }
        })
        .ok();
}

/// True once `path` is decoded and ready to play.
pub fn is_ready(path: &str) -> bool {
    let st = state();
    st.source
        .lock()
        .map(|g| matches!(&*g, Some(s) if s.path == path && !s.loading && !s.samples.is_empty()))
        .unwrap_or(false)
}

/// True if `path`'s decode finished but failed — so the widget can drop out of
/// "preparing…" instead of polling [`is_ready`] forever.
pub fn is_failed(path: &str) -> bool {
    let st = state();
    st.source
        .lock()
        .map(|g| matches!(&*g, Some(s) if s.path == path && !s.loading && s.failed))
        .unwrap_or(false)
}

/// Play the loaded source over `clips` (`(from_ms, to_ms)` kept spans) starting
/// at `start_ms` source-time, at `speed`. Returns false if nothing is loaded.
pub fn start(start_ms: f64, clips: Vec<(f64, f64)>, speed: f64) -> bool {
    let st = state();
    {
        let g = st.source.lock();
        let ok = matches!(&g, Ok(o) if matches!(&**o, Some(s) if !s.loading && !s.samples.is_empty()));
        if !ok {
            return false;
        }
    }
    st.cmd
        .lock()
        .ok()
        .map(|tx| {
            tx.send(PlayCmd::Start {
                clips,
                start_ms,
                speed,
            })
            .is_ok()
        })
        .unwrap_or(false)
}

/// Live, instant re-position to `src_ms` (no stream rebuild).
pub fn seek(src_ms: f64) {
    state()
        .seek_ms
        .store(src_ms.round() as i64, Ordering::Release);
}

/// Live tempo change, pitch-preserving (no stream rebuild).
pub fn set_speed(speed: f64) {
    f64_to_bits_store(&state().speed_bits, speed.clamp(0.25, 4.0));
}

pub fn stop() {
    let st = state();
    st.playing.store(false, Ordering::Release);
    if let Ok(tx) = st.cmd.lock() {
        let _ = tx.send(PlayCmd::Stop);
    }
}

/// Sample-accurate source-ms of what the speaker is playing right now.
pub fn pos_ms() -> f64 {
    f64_load(&state().pos_ms)
}

pub fn is_playing() -> bool {
    state().playing.load(Ordering::Acquire)
}

pub fn is_finished() -> bool {
    state().finished.load(Ordering::Acquire)
}
