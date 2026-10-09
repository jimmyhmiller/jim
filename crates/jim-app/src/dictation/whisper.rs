//! Whisper (large-v3-turbo via whisper.cpp's `whisper-server`) as a live
//! dictation engine.
//!
//! Whisper is not a streaming model: every request is a fresh transcription
//! of whatever audio it is handed, padded internally to a 30 s window. So
//! the encoder costs about the same for a 2 s clip as for a 20 s one, and
//! shrinking the window with `audio_ctx` is not an option — on turbo it
//! produced garbage and 12 s passes.
//!
//! What we CAN control is how much audio each pass carries and how long
//! text keeps moving. [`WhisperStream`] is the LocalAgreement policy from
//! Macháček et al., "Turning Whisper into Real-Time Transcription System"
//! (the `whisper_streaming` project), adapted to what whisper-server can
//! cheaply tell us:
//!
//! - Each pass transcribes the current audio window. The prompt is the
//!   committed text from BEFORE the window, so context survives a trim —
//!   never text whose audio is still in the window: whisper treats a prompt
//!   as already said, and skipped straight past a sentence it was handed.
//! - The pass is aligned (edit distance) against the committed words whose
//!   audio is still in the window; what's past them is new.
//! - New words that two consecutive passes agree on (their longest common
//!   prefix) are **committed**: frozen, never rewritten.
//! - Once the window passes [`TRIM_SECS`], audio is dropped at a whisper
//!   segment boundary that follows committed words AND that the audio itself
//!   says is a pause, so the cut can't clip a word.
//!
//! The window stays a few seconds long, so every pass costs the same on
//! minute nine as on minute one, and text stops churning a pass or two after
//! it was spoken.
//!
//! Requests ask for `srt`, which carries segment times for free. Per-word
//! times (`verbose_json`) turn on whisper.cpp's token timestamps, which
//! measured +0.3–0.7 s a pass — and drifted too much between passes to
//! decide what was new.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use super::server::{self, Shared, Spec};
use super::{RATE, Transcriber};

/// A live pass should normally take well under three seconds. Without an
/// HTTP timeout, one wedged request blocks the dictation worker forever
/// while the microphone visibly keeps recording.
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Don't transcribe a window shorter than this — there's nothing in it yet,
/// and whisper hallucinates on near-silence.
const MIN_PASS_SECS: f32 = 0.8;
/// Floor on the gap between passes, so a short window can't pin a core.
const MIN_PASS_GAP: Duration = Duration::from_millis(250);
/// Don't re-run a pass until at least this much new audio has arrived; the
/// same audio twice gives the same answer.
const MIN_NEW_SECS: f32 = 0.3;
/// Start trimming committed audio off the window past this length.
const TRIM_SECS: f32 = 8.0;
/// Past this, trim at a committed segment boundary even without a
/// confirmed pause, and failing that commit the oldest segments outright.
/// Whisper's window is 30 s; staying under it keeps every pass one window.
const FORCE_SECS: f32 = 20.0;
/// How far either side of a segment boundary to look for the pause.
const PAUSE_SEARCH_SECS: f64 = 0.3;
/// RMS frame for the pause search.
const FRAME_SECS: f64 = 0.02;
/// A frame counts as a pause below this fraction of the window's loudest
/// frame (or the absolute floor, in a quiet room).
const PAUSE_RATIO: f32 = 0.12;
const PAUSE_FLOOR: f32 = 0.005;
/// Characters of committed text sent as the prompt.
const PROMPT_CHARS: usize = 200;
/// Live passes that may fail in a row before the session gives up. One bad
/// window (a decode stuck in a repetition loop until the timeout) shouldn't
/// end a dictation; a server that keeps failing should.
const MAX_PASS_FAILURES: u32 = 3;

static SERVER: Shared = Shared::new(Spec {
    name: "whisper",
    key: || {
        model_path()
            .map(|p| p.display().to_string())
            .ok_or_else(|| "no HOME".to_string())
    },
    command: spawn_command,
    is_ours: |pid| server::proc_name(pid).as_deref() == Some("whisper-server"),
    startup_timeout: Duration::from_secs(45),
    warm: None,
});

fn model_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".jim/models/ggml-large-v3-turbo.bin"))
}

fn spawn_command(port: u16) -> Result<Command, String> {
    let model = model_path().ok_or("no HOME")?;
    if !model.exists() {
        return Err(format!("whisper model missing: {}", model.display()));
    }
    let mut cmd = Command::new("whisper-server");
    cmd.args([
        "-m",
        &model.to_string_lossy(),
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
    ]);
    Ok(cmd)
}

pub fn prewarm() {
    SERVER.prewarm();
}

/// Load the model and verify the server before opening the microphone, so
/// a failure is reported before capture rather than on the first pass.
pub fn start() -> Result<Box<dyn Transcriber>, String> {
    SERVER.ensure_running()?;
    Ok(Box::new(WhisperStream::default()))
}

// ============================================================
// The stream
// ============================================================

/// One word of a hypothesis.
#[derive(Clone, Debug, PartialEq)]
struct Word {
    /// As whisper wrote it, punctuation attached.
    text: String,
    /// `Some(end)` on the last word of a whisper segment: a phrase boundary,
    /// at that absolute time — the only places the window is trimmed.
    seg_end: Option<f64>,
}

#[derive(Default)]
struct WhisperStream {
    /// The window, at [`RATE`].
    audio: Vec<f32>,
    /// Absolute time of `audio[0]`.
    audio_start: f64,
    /// Frozen words. Never rewritten.
    committed: Vec<Word>,
    /// Index of the first committed word whose audio is still in the window:
    /// a pass re-transcribes these before saying anything new.
    window_from: usize,
    /// The latest pass's new words — the next pass's agreement candidates,
    /// and the moving tail of the preview.
    hyp: Vec<Word>,
    /// Samples pushed in total, and the total as of the last pass.
    pushed: usize,
    pushed_at_pass: usize,
    last_pass: Option<Instant>,
    /// Live passes failed in a row.
    failures: u32,
}

impl Transcriber for WhisperStream {
    fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        self.audio.extend_from_slice(samples);
        self.pushed += samples.len();
        Ok(())
    }

    fn step(&mut self) -> Result<Option<String>, String> {
        let window = self.window_secs();
        let fresh = (self.pushed - self.pushed_at_pass) as f32 / RATE as f32;
        let gap_ok = self.last_pass.is_none_or(|t| t.elapsed() >= MIN_PASS_GAP);
        if window < MIN_PASS_SECS || fresh < MIN_NEW_SECS || !gap_ok {
            return Ok(None);
        }
        let before = self.text();
        let began = Instant::now();
        let words = match self.pass() {
            Ok(w) => {
                self.failures = 0;
                w
            }
            Err(e) => {
                self.failures += 1;
                if self.failures >= MAX_PASS_FAILURES {
                    return Err(e);
                }
                // The next step retries with more audio, on a fresh server.
                eprintln!("[dictation] whisper pass {} failed; continuing: {e}", self.failures);
                return Ok(None);
            }
        };
        self.absorb(words);
        self.trim();
        eprintln!(
            "[dictation] whisper pass: window={window:.2}s took={:.2}s committed={} tentative={}",
            began.elapsed().as_secs_f32(),
            self.committed.len(),
            self.hyp.len()
        );
        let after = self.text();
        Ok((after != before).then_some(after))
    }

    fn finish(&mut self) -> Result<String, String> {
        if self.window_secs() >= MIN_PASS_SECS {
            match self.pass().or_else(|first| {
                eprintln!("[dictation] final whisper pass failed; retrying once: {first}");
                self.pass().map_err(|e| format!("{first}; retry failed: {e}"))
            }) {
                Ok(words) => {
                    let fresh = self.new_words(words);
                    self.committed.extend(fresh);
                    self.hyp.clear();
                }
                // Losing the tail is only fatal if it's all we had;
                // otherwise ship what the user last saw rather than nothing.
                Err(e) if self.committed.is_empty() && self.hyp.is_empty() => return Err(e),
                Err(e) => {
                    eprintln!("[dictation] final whisper pass failed; keeping the last preview: {e}");
                }
            }
        }
        let text = self.text();
        self.committed.extend(std::mem::take(&mut self.hyp));
        Ok(text)
    }
}

impl WhisperStream {
    fn window_secs(&self) -> f32 {
        self.audio.len() as f32 / RATE as f32
    }

    /// `committed + tentative`, as it should read at the caret.
    fn text(&self) -> String {
        words_text(self.committed.iter().chain(&self.hyp))
    }

    /// Transcribe the window. A failed request discards the server — that
    /// also kills a decode stuck past the timeout — so whoever retries gets
    /// a fresh one.
    fn pass(&mut self) -> Result<Vec<Word>, String> {
        self.pushed_at_pass = self.pushed;
        self.last_pass = Some(Instant::now());
        let wav = server::encode_wav(&self.audio, RATE)?;
        let prompt = self.prompt();
        let port = SERVER.ensure_running()?;
        let body = request(port, &wav, &prompt).inspect_err(|e| {
            eprintln!("[whisper] inference failed on port {port}; discarding the server: {e}");
            SERVER.discard(port);
        })?;
        parse_srt(&body, self.audio_start)
    }

    /// Committed text whose audio has left the window — see the module docs
    /// for why never anything still in it.
    fn prompt(&self) -> String {
        tail_chars(&words_text(self.committed[..self.window_from].iter()), PROMPT_CHARS)
    }

    /// The part of a pass that isn't already committed: everything after the
    /// point where it best aligns with the committed words still in the
    /// window.
    fn new_words(&self, words: Vec<Word>) -> Vec<Word> {
        let skip = align(&self.committed[self.window_from..], &words);
        words.into_iter().skip(skip).collect()
    }

    /// LocalAgreement-2: commit the prefix this pass shares with the last.
    fn absorb(&mut self, words: Vec<Word>) {
        let fresh = self.new_words(words);
        let agreed = common_prefix(&self.hyp, &fresh);
        self.committed.extend(fresh[..agreed].iter().cloned());
        self.hyp = fresh[agreed..].to_vec();
    }

    /// Keep the window short by dropping audio the committed words no longer
    /// need — see [`TRIM_SECS`] and [`FORCE_SECS`].
    fn trim(&mut self) {
        let window = self.window_secs();
        if window <= TRIM_SECS {
            return;
        }
        let force = window > FORCE_SECS;
        if let Some((idx, cut)) = self.find_cut(force) {
            self.cut_at(idx, cut);
            return;
        }
        if !force {
            return;
        }
        // Past FORCE_SECS with nothing committed to cut after: the passes
        // keep disagreeing (noise, or speech whisper can't settle on). Commit
        // the hypothesis up to its last segment boundary in the older half of
        // the window, so the window can move.
        let horizon = self.audio_start + window as f64 / 2.0;
        let boundary = self
            .hyp
            .iter()
            .rposition(|w| w.seg_end.is_some_and(|t| t <= horizon));
        let n = match boundary {
            Some(i) => i + 1,
            // One unbroken segment over half the window: commit it all.
            None => self.hyp.len(),
        };
        eprintln!("[dictation] whisper window at {window:.1}s with no agreement; forcing {n} word(s)");
        let forced: Vec<Word> = self.hyp.drain(..n).collect();
        self.committed.extend(forced);
        match self.find_cut(true) {
            Some((idx, cut)) => self.cut_at(idx, cut),
            None => {
                // Nothing marks a boundary at all: everything heard so far is
                // committed, so drop the whole window.
                let idx = self.committed.len().saturating_sub(1);
                let end = self.audio_start + window as f64;
                self.cut_at(idx, end);
            }
        }
    }

    /// The latest committed segment boundary inside the window whose
    /// surroundings are quiet, as (word index, cut time). When forced, the
    /// latest committed boundary regardless, at its quietest nearby frame.
    fn find_cut(&self, force: bool) -> Option<(usize, f64)> {
        let peak = frame_rms(&self.audio).fold(0.0f32, f32::max);
        let quiet = (peak * PAUSE_RATIO).max(PAUSE_FLOOR);
        let boundaries = (self.window_from..self.committed.len())
            .rev()
            .filter_map(|i| Some((i, self.committed[i].seg_end?)))
            .filter(|(_, t)| *t > self.audio_start);
        let mut latest = None;
        for (i, t) in boundaries {
            let Some((at, level)) = self.quietest_near(t) else { continue };
            if level <= quiet {
                return Some((i, at));
            }
            latest.get_or_insert((i, at));
        }
        if force { latest } else { None }
    }

    /// The quietest frame within [`PAUSE_SEARCH_SECS`] of absolute time `t`:
    /// (its centre, its RMS).
    fn quietest_near(&self, t: f64) -> Option<(f64, f32)> {
        let frame = (FRAME_SECS * RATE as f64) as usize;
        let lo = ((t - PAUSE_SEARCH_SECS - self.audio_start).max(0.0) * RATE as f64) as usize;
        let hi = (((t + PAUSE_SEARCH_SECS - self.audio_start) * RATE as f64) as usize)
            .min(self.audio.len());
        (lo..hi.saturating_sub(frame))
            .step_by(frame / 2)
            .map(|s| (s, rms(&self.audio[s..s + frame])))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(s, level)| {
                (self.audio_start + (s + frame / 2) as f64 / RATE as f64, level)
            })
    }

    /// Drop the window's audio before absolute time `cut`; committed words up
    /// to and including `idx` leave the window with it.
    fn cut_at(&mut self, idx: usize, cut: f64) {
        let drop = (((cut - self.audio_start) * RATE as f64) as usize).min(self.audio.len());
        self.audio.drain(..drop);
        self.audio_start += drop as f64 / RATE as f64;
        self.window_from = (idx + 1).min(self.committed.len());
    }
}

fn request(port: u16, wav: &[u8], prompt: &str) -> Result<String, String> {
    let mut fields = vec![
        ("response_format", "srt"),
        // No temperature fallback: a window whisper can't decode cleanly at
        // temperature 0 gets re-decoded up to five more times, which is how a
        // repetition loop ran a pass into the timeout. Live passes are
        // revised by the next one anyway.
        ("temperature_inc", "0"),
        // Keep "*sounds of water*" and friends out of the transcript.
        ("suppress_nst", "true"),
    ];
    if !prompt.is_empty() {
        fields.push(("prompt", prompt));
    }
    server::post_wav(
        &format!("http://127.0.0.1:{port}/inference"),
        wav,
        &fields,
        INFERENCE_TIMEOUT,
    )
}

/// whisper-server's SRT → words, with segment ends in absolute time.
/// Bracketed annotations (`[BLANK_AUDIO]`, `*sounds of water*`) are dropped:
/// whisper describing the audio rather than transcribing it.
fn parse_srt(body: &str, offset: f64) -> Result<Vec<Word>, String> {
    if body.trim_start().starts_with('{') {
        // whisper-server reports failures as JSON whatever format was asked.
        return Err(format!("whisper error: {}", body.trim()));
    }
    let mut out = Vec::new();
    for block in body.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.lines().map(str::trim).filter(|l| !l.is_empty());
        let (Some(_index), Some(times)) = (lines.next(), lines.next()) else {
            continue;
        };
        let (_, end) = times
            .split_once("-->")
            .ok_or_else(|| format!("malformed SRT timing line {times:?}"))?;
        let end = offset + srt_time(end.trim())?;
        let text = lines.collect::<Vec<_>>().join(" ");
        if is_annotation(&text) {
            continue;
        }
        let first = out.len();
        out.extend(text.split_whitespace().map(|w| Word {
            text: w.to_string(),
            seg_end: None,
        }));
        if out.len() > first {
            if let Some(w) = out.last_mut() {
                w.seg_end = Some(end);
            }
        }
    }
    Ok(out)
}

/// `HH:MM:SS,mmm` → seconds.
fn srt_time(s: &str) -> Result<f64, String> {
    let bad = || format!("malformed SRT time {s:?}");
    let (hms, ms) = s.split_once(',').ok_or_else(bad)?;
    let mut parts = hms.split(':').map(|p| p.parse::<f64>().map_err(|_| bad()));
    let (h, m, sec) = (
        parts.next().ok_or_else(bad)??,
        parts.next().ok_or_else(bad)??,
        parts.next().ok_or_else(bad)??,
    );
    let ms: f64 = ms.parse().map_err(|_| bad())?;
    Ok(h * 3600.0 + m * 60.0 + sec + ms / 1000.0)
}

fn is_annotation(text: &str) -> bool {
    let t = text.trim();
    let wrapped = |o: char, c: char| t.len() >= 2 && t.starts_with(o) && t.ends_with(c);
    wrapped('[', ']') || wrapped('(', ')') || wrapped('*', '*')
}

/// A word as compared between passes: punctuation and case don't count, so
/// "way" and "way," agree.
fn norm(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .flat_map(char::to_lowercase)
        .collect()
}

fn same(a: &Word, b: &Word) -> bool {
    norm(&a.text) == norm(&b.text)
}

fn common_prefix(a: &[Word], b: &[Word]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| same(x, y)).count()
}

/// How many leading words of `pass` re-transcribe `known`: the prefix
/// length `j` minimising the edit distance between `known` and `pass[..j]`.
/// Ties go to the SHORTER prefix — a word shown twice is fixable by eye; a
/// word silently dropped is not.
fn align(known: &[Word], pass: &[Word]) -> usize {
    if known.is_empty() {
        return 0;
    }
    // row[j] = distance(known[..i], pass[..j]), rolled over i.
    let mut row: Vec<usize> = (0..=pass.len()).collect();
    for (i, k) in known.iter().enumerate() {
        let mut next = vec![i + 1; pass.len() + 1];
        for (j, p) in pass.iter().enumerate() {
            let sub = row[j] + usize::from(!same(k, p));
            next[j + 1] = sub.min(row[j + 1] + 1).min(next[j] + 1);
        }
        row = next;
    }
    (0..=pass.len()).min_by_key(|&j| (row[j], j)).unwrap_or(0)
}

fn words_text<'a>(words: impl Iterator<Item = &'a Word>) -> String {
    words.map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

fn frame_rms(audio: &[f32]) -> impl Iterator<Item = f32> + '_ {
    audio
        .chunks((FRAME_SECS * RATE as f64) as usize)
        .map(rms)
}

/// The last `max` characters of `s`, on a character boundary.
fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(max)).collect()
}

// ============================================================
// Legacy per-GUI servers
// ============================================================
//
// Before the server was shared, each jim spawned its own and recorded it in
// `~/.jim/whisper-servers/<server pid>` (contents: the owning jim's pid) so
// the next jim could kill it if the owner died without cleaning up. Those
// servers are never adopted — nothing knows their port — so a dead owner's
// server is still killed here. Once the directory empties it is removed and
// this is a no-op.

fn legacy_registry_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".jim/whisper-servers"))
}

fn is_whisper_server(pid: i32) -> bool {
    server::pid_alive(pid) && server::proc_name(pid).as_deref() == Some("whisper-server")
}

/// Kill per-GUI whisper servers left behind by a jim that has since died.
/// Called once at startup.
///
/// A record whose owner is still alive belongs to another running (old)
/// jim and is left alone. Everything else is cleared out — but only after
/// checking that the pid is still a whisper-server, since a recorded pid
/// that has been recycled would otherwise name an innocent process.
pub fn reap_orphans() {
    let Some(dir) = legacy_registry_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
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
        if owner.is_some_and(server::pid_alive) {
            continue;
        }
        if is_whisper_server(pid) {
            eprintln!("[whisper] reaping legacy server orphaned by a dead jim: pid={pid}");
            // SAFETY: SIGTERM by group (it is its own leader) and by pid, so
            // this also reaches servers spawned before jim used groups.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    // Only succeeds once empty.
    let _ = std::fs::remove_dir(&dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(s: &str) -> Vec<Word> {
        s.split_whitespace()
            .map(|t| Word {
                text: t.to_string(),
                seg_end: None,
            })
            .collect()
    }

    fn texts(words: &[Word]) -> String {
        words_text(words.iter())
    }

    const SRT: &str = "1\n00:00:00,000 --> 00:00:02,800\n All right, testing one, two, three.\n\n\
                       2\n00:00:02,800 --> 00:00:03,500\n [BLANK_AUDIO]\n\n\
                       3\n00:00:03,500 --> 00:01:06,740\n Hi, so today\n I'm gonna talk\n";

    #[test]
    fn srt_becomes_words_with_segment_ends() {
        let w = parse_srt(SRT, 10.0).unwrap();
        assert_eq!(texts(&w), "All right, testing one, two, three. Hi, so today I'm gonna talk");
        assert_eq!(w[5].seg_end, Some(12.8), "end of the first segment");
        assert_eq!(w.last().unwrap().seg_end, Some(76.74), "minutes count");
        assert_eq!(w.iter().filter(|w| w.seg_end.is_some()).count(), 2, "annotation dropped");
    }

    #[test]
    fn an_error_body_is_an_error() {
        assert!(parse_srt(r#"{"error":"bad audio"}"#, 0.0).is_err());
        assert!(parse_srt("1\n00:00:00 -> nonsense\n hi\n", 0.0).is_err());
        assert_eq!(parse_srt("", 0.0).unwrap(), vec![]);
    }

    /// LocalAgreement: a word is committed only once two consecutive passes
    /// agree on it, and committed words are never re-emitted.
    #[test]
    fn agreement_commits_the_shared_prefix() {
        let mut s = WhisperStream::default();
        s.absorb(ws("config fill"));
        assert!(s.committed.is_empty(), "one pass is never enough to commit");
        assert_eq!(s.text(), "config fill");

        s.absorb(ws("config file is"));
        assert_eq!(texts(&s.committed), "config");
        assert_eq!(s.text(), "config file is");

        s.absorb(ws("config file is here"));
        assert_eq!(texts(&s.committed), "config file is");
        assert_eq!(texts(&s.hyp), "here");
    }

    #[test]
    fn agreement_ignores_case_and_punctuation() {
        let mut s = WhisperStream::default();
        s.absorb(ws("way with"));
        s.absorb(ws("way, With"));
        assert_eq!(texts(&s.committed), "way, With");
    }

    /// The failure the timestamp filter had: a re-transcription that moves
    /// words around in time must still be recognised as the same words.
    #[test]
    fn committed_words_in_the_window_are_not_repeated() {
        let mut s = WhisperStream {
            committed: ws("The first thing I want to"),
            ..Default::default()
        };
        s.absorb(ws("The first thing I want to mention is the elephant"));
        assert_eq!(texts(&s.hyp), "mention is the elephant");
        assert_eq!(s.text(), "The first thing I want to mention is the elephant");
    }

    #[test]
    fn the_prompt_is_only_text_whose_audio_is_gone() {
        let mut s = WhisperStream {
            committed: ws("Old sentence. New words still in the window"),
            window_from: 2,
            ..Default::default()
        };
        assert_eq!(s.prompt(), "Old sentence.");
        s.window_from = 0;
        assert_eq!(s.prompt(), "", "everything committed is still being heard");
    }

    #[test]
    fn alignment_survives_a_revised_committed_word() {
        // Whisper re-hears "could find" as "could not find" — the pass still
        // lines up with what's committed, and only the new words are new.
        let known = ws("Fifth, I could find my umbrella");
        let pass = ws("Fifth, I could not find my umbrella anywhere this morning.");
        assert_eq!(align(&known, &pass), 7);
    }

    #[test]
    fn alignment_handles_a_dropped_committed_word() {
        // Whisper didn't re-hear committed "the" this time.
        let known = ws("talks to the bus");
        let pass = ws("talks to bus over coil");
        assert_eq!(align(&known, &pass), 3);
    }

    #[test]
    fn alignment_prefers_showing_a_word_twice_to_losing_it() {
        // Is "x" a mishearing of committed "c" (skip 3) or a new word after
        // a dropped "c" (skip 2)? Both cost one edit; keep "x" visible.
        assert_eq!(align(&ws("a b c"), &ws("a b x")), 2);
    }

    #[test]
    fn alignment_skips_a_fragment_before_the_known_words() {
        assert_eq!(align(&ws("over coil"), &ws("bus over coil Ask")), 3);
        assert_eq!(align(&[], &ws("anything")), 0);
    }

    /// Speech, a pause, speech — at RATE.
    fn speech_pause_speech(a: f32, pause: f32, b: f32) -> Vec<f32> {
        let tone = |secs: f32| -> Vec<f32> {
            (0..(secs * RATE as f32) as usize)
                .map(|i| (i as f32 * 220.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.3)
                .collect()
        };
        let mut v = tone(a);
        v.extend(std::iter::repeat_n(0.0005, (pause * RATE as f32) as usize));
        v.extend(tone(b));
        v
    }

    fn seg(text: &str, end: f64) -> Vec<Word> {
        let mut w = ws(text);
        w.last_mut().unwrap().seg_end = Some(end);
        w
    }

    #[test]
    fn trims_at_a_committed_boundary_inside_a_pause() {
        let mut s = WhisperStream {
            audio: speech_pause_speech(5.0, 0.6, 4.0),
            ..Default::default()
        };
        // Whisper put the boundary a little late, at 5.4s; the pause is
        // 5.0–5.6s, so the cut must still land inside it.
        s.committed = seg("one two three", 5.4);
        s.hyp = ws("four five");
        s.trim();
        assert!(
            s.audio_start > 5.0 && s.audio_start < 5.6,
            "cut at {:.2}s, outside the pause",
            s.audio_start
        );
        assert_eq!(s.window_from, 3, "the committed words left the window");
        assert_eq!(s.text(), "one two three four five", "trimming never changes the text");
    }

    #[test]
    fn will_not_cut_through_speech_unless_forced() {
        let mut s = WhisperStream {
            audio: speech_pause_speech(9.0, 0.0, 0.0),
            committed: seg("one two three", 5.0),
            ..Default::default()
        };
        s.trim();
        assert_eq!(s.audio_start, 0.0, "cut a boundary the audio says is mid-speech");

        s.audio = speech_pause_speech(FORCE_SECS + 1.0, 0.0, 0.0);
        s.trim();
        assert!(s.audio_start > 4.6 && s.audio_start < 5.4, "forced cut at {}", s.audio_start);
    }

    /// A window that never reaches agreement must still be bounded.
    #[test]
    fn forced_trim_moves_a_window_with_no_agreement() {
        let mut hyp = seg("a b c", 4.0);
        hyp.extend(seg("d e f", 9.0));
        hyp.extend(seg("g h i", 15.0));
        let mut s = WhisperStream {
            audio: speech_pause_speech(FORCE_SECS + 2.0, 0.0, 0.0),
            hyp,
            ..Default::default()
        };
        s.trim();
        assert_eq!(texts(&s.committed), "a b c d e f", "commits through the last boundary before the midpoint");
        assert!(s.audio_start > 8.0, "window didn't move: {}", s.audio_start);
        assert_eq!(s.text(), "a b c d e f g h i");
    }

    #[test]
    fn annotations_are_recognised() {
        assert!(is_annotation("[BLANK_AUDIO]"));
        assert!(is_annotation(" *sounds of water* "));
        assert!(is_annotation("(music)"));
        assert!(!is_annotation("I said (quietly) hello"));
        assert!(!is_annotation("*"));
    }

    /// A legacy record whose owner is gone, but whose pid has been recycled
    /// by something that is not a whisper-server, must be cleaned up
    /// WITHOUT signalling that process.
    #[test]
    fn reap_orphans_wont_kill_a_recycled_pid() {
        let Some(dir) = legacy_registry_dir() else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let mut victim = Command::new("sleep").arg("30").spawn().unwrap();
        let record = dir.join(victim.id().to_string());
        std::fs::write(&record, format!("{}\n", DEAD_OWNER_PID)).unwrap();

        reap_orphans();

        assert!(!record.exists(), "a record with a dead owner should be cleared out");
        assert!(
            matches!(victim.try_wait(), Ok(None)),
            "reap_orphans killed a recycled pid that was not a whisper-server"
        );
        let _ = victim.kill();
        let _ = victim.wait();
    }

    /// A legacy record whose owner is still running belongs to another live
    /// jim, and must be left completely alone.
    #[test]
    fn reap_orphans_leaves_a_live_owners_record_alone() {
        let Some(dir) = legacy_registry_dir() else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let record = dir.join(format!("{}", DEAD_OWNER_PID + 1));
        std::fs::write(&record, format!("{}\n", std::process::id())).unwrap();

        reap_orphans();

        assert!(record.exists(), "reap_orphans deleted a record still owned by a live process");
        let _ = std::fs::remove_file(&record);
    }

    /// A pid no live process can have: pids are allocated below 99999.
    const DEAD_OWNER_PID: i32 = 900_000;

    /// Spawn → adopt against a REAL whisper-server, in the test-private
    /// state dir: a jim with no in-process handle (a restarted GUI) must
    /// pick up the recorded server rather than load the model again.
    ///
    /// Ignored by default: loads a ~1GB model. Run with:
    ///   cargo test -p jim_app --lib dictation::whisper -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn round_trips_and_adopts_a_real_server() {
        if model_path().map(|p| !p.exists()).unwrap_or(true) {
            panic!("no whisper model at ~/.jim/models — can't run this test");
        }
        let port = SERVER.ensure_running().expect("server must start");
        let pid = SERVER.recorded_pid_for_test().expect("a spawned server must be recorded");
        let quiet: Vec<f32> = (0..RATE).map(|i| (i as f32 * 0.05).sin() * 0.01).collect();
        let wav = server::encode_wav(&quiet, RATE).unwrap();
        parse_srt(&request(port, &wav, "").expect("request"), 0.0).expect("parse");

        let (_, child) = SERVER.forget_handle_for_test().expect("server cached");
        let began = Instant::now();
        let adopted = SERVER.ensure_running().expect("should adopt the recorded server");
        assert_eq!(adopted, port, "a fresh handle respawned instead of adopting");
        assert!(SERVER.holds_adopted_for_test(pid));
        assert!(began.elapsed() < Duration::from_millis(500), "that was a model load, not an adopt");

        SERVER.shutdown_for_test();
        if let Some(mut c) = child {
            let _ = c.wait();
        }
        assert!(SERVER.recorded_pid_for_test().is_none(), "the record outlived its server");
        let _ = std::fs::remove_dir_all(server::state_dir().unwrap());
    }

    /// The whole stream against a real server over a clip long enough to
    /// trim many times: every distinctive noun must survive the windowing.
    #[test]
    #[ignore]
    fn streams_a_long_clip_without_losing_words() {
        let mut t = start().expect("whisper must start");
        let report = super::super::tests::stream_spoken_clip(t.as_mut());
        SERVER.shutdown_for_test();
        let _ = std::fs::remove_dir_all(server::state_dir().unwrap());
        report.assert_complete();
    }
}
