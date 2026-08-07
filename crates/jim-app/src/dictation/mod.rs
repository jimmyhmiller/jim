//! Dictation into whatever owns the keyboard, transcribed live.
//!
//! Hold **⌘⇧M** and talk. Words appear at the caret while you're still
//! speaking and settle as whisper gets more context; release and the final
//! text lands. It goes into whatever was focused when you started talking —
//! a widget `Input`/`TextArea`, an editor pane, the command palette, or a
//! terminal.
//!
//! For hands-free dictation, press **⌘⇧T** once and release the keys.
//! Press **Escape** (or **⌘⇧T** again) to stop. Escape is consumed by
//! dictation, so it does not also reach the focused target or another overlay.
//!
//! There is no time limit. Talk for as long as you like.
//!
//! ## How "live" works
//!
//! Each pass re-transcribes the *uncommitted tail* of the clip and rewrites
//! everything written since the last commit. Re-running a whole span rather
//! than appending chunks means every pass has full context over that span,
//! so the text converges on the best transcript instead of accumulating
//! boundary mistakes it could never go back and fix.
//!
//! The tail can't grow without bound, because pass cost grows with it
//! (measured against a warm [`whisper`] server: 5s → ~0.35s, 10s → ~0.43s,
//! 30s → ~1.2s, 60s → ~2.1s). So once the tail passes
//! [`COMMIT_TARGET_SECS`] we look for a pause in it — a low-energy stretch
//! of at least [`SILENCE_MIN_SECS`], which is a gap between phrases and so
//! never mid-word — transcribe everything before it, and *commit* that
//! text: it's frozen, and its audio is dropped. What's displayed is
//! `committed + tail`.
//!
//! That keeps the per-pass cost flat no matter how long you talk, and it
//! makes release *faster* on a long clip than it used to be on a short
//! one, because the final pass only has the tail left to do.
//!
//! Committing is the one irreversible step, so it deliberately stays
//! [`COMMIT_KEEP_SECS`] back from the end — the last few seconds are where
//! whisper is still revising itself, and a commit there would freeze a
//! guess that the next pass would have fixed.
//!
//! The consequence you can see: text near the caret churns. "config fill"
//! becomes "config file" a pass later. That's inherent to previewing a
//! non-streaming model, and it's the tradeoff this mode chooses. Text
//! behind a commit point stops moving.
//!
//! ## The things that make it safe
//!
//! **The target is snapshotted when recording STARTS**, not when text
//! arrives — focus may have moved by then.
//!
//! **Every rewrite verifies what it's replacing.** We remember the exact
//! text last written at the anchor; if what's there now differs, the user
//! typed under us, so we detach rather than clobber their edit.
//!
//! **A terminal is only revised when the child can take it.** There's no
//! anchor to inspect in a terminal — it's a byte stream, and a paste can't
//! be un-sent — so a revision is `DEL × n` followed by the new text, which
//! only means "delete what we wrote" to a line editor. We send it solely
//! when the child has bracketed paste on, isn't on the alternate screen and
//! isn't grabbing the mouse (see [`jim_terminal::terminal_write_state`]) —
//! true of a shell prompt or Claude Code, false of vim, less and htop —
//! Terminal output and redraws do not detach an active dictation. Codex can
//! update its UI while the user is speaking, so cursor movement is not a
//! reliable signal that the transcript should stop.
//! A child that fails the test never gets preview bytes at all; its
//! transcript shows in the status pill and lands as one paste on release.

mod whisper;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::camera::visibility::RenderLayers;
use bevy::input::ButtonInput;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;

use editor_core::selection::Selection;
use editor_core::transaction::{Change, Transaction};
use jim_editor::EditorStateComp;
use jim_pane::{FocusedPane, PaneFont, PaneFontMetrics, PaneKindMarker};
use jim_terminal::TerminalStore;
use jim_terminal::worker::WorkerMsg;
use jim_widget::protocol::{Align, Border, Edges, Element, HostEvent, Shadow, Style, Weight};
use jim_widget::render::{self, LayoutCtx, WidgetPalette};
use jim_widget::script_widget::ScriptWidget;
use jim_widget::{WidgetIO, WidgetInputFocus, WidgetTargets, audio};

use crate::MENU_OVERLAY_LAYER;
use crate::actions::{ActionRegistry, Keymap};
use crate::command_palette::{self, CommandPalette, PaletteUsage};

/// Push-to-talk key, held with ⌘ and ⇧.
const HOLD_HOTKEY: KeyCode = KeyCode::KeyM;
/// Hands-free toggle, pressed with ⌘ and ⇧. This intentionally replaces
/// the old theme-editor shortcut.
const TOGGLE_HOTKEY: KeyCode = KeyCode::KeyT;
/// How long a failure message stays on screen.
const ERROR_SECS: f64 = 5.0;
/// Don't transcribe a fragment shorter than this — there's nothing in it
/// yet, and whisper tends to hallucinate on near-silence.
const MIN_LIVE_SECS: f32 = 0.8;
/// Floor on the gap between live passes, so a short clip (~0.35s a pass)
/// can't pin a core at 100% duty cycle.
const MIN_PASS_GAP: Duration = Duration::from_millis(250);

/// Start looking for somewhere to commit once the uncommitted tail passes
/// this. Sets the steady-state pass cost: a ~20s window is ~0.7s a pass,
/// which still reads as live.
const COMMIT_TARGET_SECS: f32 = 20.0;
/// Never commit within this much of the end. The last few seconds are
/// exactly where whisper is still revising itself, so committing them
/// would freeze a guess the next pass would have corrected.
const COMMIT_KEEP_SECS: f32 = 6.0;
/// If the tail reaches this with no pause to cut at, cut at the quietest
/// point anyway — a rising pass cost is worse than one clipped word.
const COMMIT_FORCE_SECS: f32 = 45.0;
/// A low-energy stretch at least this long reads as a pause between
/// phrases, so cutting in the middle of it can't split a word.
const SILENCE_MIN_SECS: f32 = 0.45;
/// Per-frame RMS below this counts as silence. Set well above the noise
/// floor of a quiet room but below speech.
const SILENCE_RMS: f32 = 0.01;
/// Frame size for the RMS scan.
const SILENCE_FRAME_SECS: f32 = 0.02;

const PILL_W: f32 = 300.0;
const PILL_TOP: f32 = 64.0;
/// Z within the overlay layer — above the screenshot toast (760).
const PILL_Z: f32 = 770.0;

/// Where a transcript gets written. Resolved once, when recording starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// The command palette's query line (it owns the keyboard when open).
    Palette,
    /// A widget pane with a focused `Input`/`TextArea`.
    Widget(Entity),
    /// An editor pane.
    Editor(Entity),
    /// A terminal pane. Batch only — see the module docs.
    Terminal(Entity),
}

impl Target {
    /// Whether a preview can be written into this target *right now*.
    ///
    /// Constant for everything except a terminal, where it depends on what
    /// the child is currently doing and so has to be re-asked every write —
    /// a shell prompt is revisable, the vim the user just launched is not.
    fn accepts_preview(&self, world: &World) -> bool {
        match self {
            Target::Terminal(e) => world
                .get_resource::<TerminalStore>()
                .and_then(|s| jim_terminal::terminal_write_state(s, *e))
                .is_some_and(|w| w.revisable),
            _ => true,
        }
    }
}

/// Where in the target our text starts, captured at recording start so a
/// rewrite always replaces the same span.
#[derive(Clone, Debug)]
enum Anchor {
    /// The query as it was before we touched it; our text is appended.
    Palette { base: String },
    /// Char offset into the focused input's value, plus which input it was.
    Widget { id: String, at: usize },
    /// Char offset into the rope.
    Editor { at: usize },
    /// Nothing to anchor — a paste has no span to revise.
    Terminal,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum Phase {
    #[default]
    Idle,
    /// Key held: capturing, preview passes running.
    Recording,
    /// Key released: final pass in flight.
    Finishing,
}

#[derive(Clone, Copy, Debug)]
enum FinishReason {
    PushToTalkReleased,
    HandsFreeEscape,
    HandsFreeToggle,
    EnterSubmit,
    AudioCaptureStopped,
}

/// What the worker thread sends back.
enum Msg {
    /// A preview transcript of the clip so far.
    Update(String),
    /// The transcript of the whole clip; the session is over.
    Final(String),
    Error(String),
}

/// A dictation in flight: its worker thread's channel, plus the flag that
/// tells it to do the final pass and exit.
struct Session {
    /// `Mutex` only because a `Receiver` is `Send` but not `Sync`, and a
    /// Bevy resource must be both.
    rx: Mutex<Receiver<Msg>>,
    stop: Arc<AtomicBool>,
}

#[derive(Resource, Default)]
pub struct Dictation {
    phase: Phase,
    /// True when the recording was started by the hands-free toggle. Modifier
    /// and T-key releases must not finish this kind of session.
    hands_free: bool,
    target: Option<Target>,
    anchor: Option<Anchor>,
    /// Exactly the text we last wrote at the anchor. Doubles as the span to
    /// replace on the next pass and as the check that the user hasn't
    /// edited under us.
    inserted: String,
    /// Set when a rewrite found something other than [`Self::inserted`] at
    /// the anchor: the user typed (or the pane went away) mid-dictation, so
    /// we stop writing rather than clobber it.
    detached: bool,
    session: Option<Session>,
    /// The clip's WAV. `audio` always writes one; we only want the samples,
    /// so it's deleted when the session ends.
    wav: Option<PathBuf>,
    /// `Time::elapsed` when capture began — drives the readout.
    started: f64,
    /// Most recent capture level, 0..1, for the pill's meter.
    level: f32,
    /// The newest transcript, whether or not it could be written into the
    /// target. The pill falls back to showing this when it couldn't.
    preview: String,
    /// False once we've found the target won't take preview bytes (an
    /// alt-screen TUI). Drives that pill fallback.
    in_place: bool,
    /// Enter ended this recording. Deliver it to the captured target only
    /// after the final transcription has been written there.
    submit_after_finish: bool,
    /// Failure text plus the `Time::elapsed` at which it should vanish.
    error: Option<(String, f64)>,
    /// Spawned overlay root, and a signature so it only re-renders when the
    /// visible content changes.
    root: Option<Entity>,
    last_sig: u64,
}

impl Dictation {
    /// True while the winit loop must keep waking us.
    ///
    /// Not decoration: the idle baseline is `reactive(5s)`, and the capture's
    /// idle watchdog auto-stops a stream nobody polls within ~2s. Without a
    /// Continuous pin, recording would die mid-sentence any time the user
    /// held the key without moving the mouse.
    pub fn needs_frames(&self) -> bool {
        self.phase != Phase::Idle || self.error.is_some()
    }

    /// True only while the final pass is in flight. Reported to the
    /// continuous-pin canary as a *transient* reason, so a wedged whisper
    /// shows up as a named yellow bar instead of silently burning 60fps.
    /// Recording isn't transient — it ends when the user lets go of the
    /// key, however long that takes.
    pub fn is_transcribing(&self) -> bool {
        self.phase == Phase::Finishing
    }
}

pub struct DictationPlugin;

impl Plugin for DictationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Dictation>()
            // Run immediately after Bevy gathers input. A hands-free Escape
            // is removed here before any Update keyboard consumer can see it.
            .add_systems(
                PreUpdate,
                dictation_hotkey
                    .after(bevy::input::InputSystems)
                    .after(crate::reconcile_macos_modifiers),
            )
            .add_systems(Update, dictation_tick)
            .add_systems(Last, shutdown_whisper_on_exit);
    }
}

/// The whole feature, as ONE exclusive system.
///
/// Each stage needs broad `&mut World` access (writing touches palette
/// resources, widget components, editor state and the terminal store), and
/// every exclusive system is a scheduler sync point. Three of them would be
/// three barriers per frame on an app tuned to idle cheaply, so they're one
/// call chain: press → drain/write → draw.
fn dictation_tick(world: &mut World) {
    dictation_pump(world);
    render_pill(world);
}

/// Don't let a ~1GB model outlive the GUI.
fn shutdown_whisper_on_exit(mut exit: MessageReader<AppExit>) {
    if exit.read().next().is_some() {
        whisper::shutdown();
    }
}

// ============================================================
// Hotkey
// ============================================================

/// Hold ⌘⇧M for push-to-talk, or press ⌘⇧T to latch recording on.
/// A latched session stops on Escape (or another ⌘⇧T). Escape is cleared
/// from both input representations before Update systems can observe it.
fn dictation_hotkey(world: &mut World) {
    let (start_hold, toggle, escape, enter, recording, hands_free, release_hold) = {
        let keys = world.resource::<ButtonInput<KeyCode>>();
        let cmd = keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight);
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
        let d = world.resource::<Dictation>();
        let recording = d.phase == Phase::Recording;
        (
            keys.just_pressed(HOLD_HOTKEY) && cmd && shift,
            keys.just_pressed(TOGGLE_HOTKEY) && cmd && shift,
            keys.just_pressed(KeyCode::Escape),
            keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter),
            recording,
            d.hands_free,
            keys.just_released(HOLD_HOTKEY) || !cmd || !shift,
        )
    };

    if recording && enter {
        // The physical Enter must not reach the target yet: its transcript
        // is still being finalized. Replay the target's submit action only
        // after Msg::Final has been written.
        {
            let mut keys = world.resource_mut::<ButtonInput<KeyCode>>();
            keys.clear_just_pressed(KeyCode::Enter);
            keys.clear_just_pressed(KeyCode::NumpadEnter);
        }
        world.resource_mut::<Messages<KeyboardInput>>().clear();
        world.resource_mut::<Dictation>().submit_after_finish = true;
        begin_finish(world, FinishReason::EnterSubmit);
    } else if recording && hands_free && escape {
        // ButtonInput and raw KeyboardInput messages are separate paths in
        // this app. Consume both so Escape cannot close a palette/dialog,
        // reach a pane, or trigger another global keyboard handler.
        world
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear_just_pressed(KeyCode::Escape);
        world.resource_mut::<Messages<KeyboardInput>>().clear();
        begin_finish(world, FinishReason::HandsFreeEscape);
    } else if recording && hands_free && toggle {
        begin_finish(world, FinishReason::HandsFreeToggle);
    } else if recording && !hands_free && release_hold {
        begin_finish(world, FinishReason::PushToTalkReleased);
    } else if !recording && toggle {
        start_recording(world, true);
    } else if !recording && start_hold {
        start_recording(world, false);
    }
}

fn start_recording(world: &mut World, hands_free: bool) {
    let Some((target, anchor)) = resolve_target(world) else {
        fail(world, "nothing focused to dictate into".into());
        return;
    };
    let Some(dir) = dictation_dir() else {
        fail(world, "no HOME — can't stage the recording".into());
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        fail(world, format!("can't create {}: {e}", dir.display()));
        return;
    }
    let now = world.resource::<Time>().elapsed_secs_f64();
    let wav = dir.join(format!("dictate-{}.wav", (now * 1000.0) as u64));

    // The tap is what live passes read; enabling clears any stale audio.
    audio::set_pcm_tap(true);
    // "" = system default input. Mono is what whisper wants, so there's no
    // reason to duplicate up to stereo the way a clip meant for playback would.
    if !audio::record_start("", &wav.to_string_lossy(), false) {
        audio::set_pcm_tap(false);
        let why = audio::status();
        fail(
            world,
            if why.is_empty() {
                "could not start recording".into()
            } else {
                why
            },
        );
        return;
    }
    let _ = audio::take_levels(); // drop anything stale from a prior clip

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let stop_w = stop.clone();
    if std::thread::Builder::new()
        .name("dictate-worker".into())
        .spawn(move || worker(stop_w, tx))
        .is_err()
    {
        audio::record_stop();
        audio::set_pcm_tap(false);
        fail(world, "could not start the transcription worker".into());
        return;
    }

    eprintln!(
        "[dictation] recording started: mode={} target={target:?} wav={}",
        if hands_free {
            "hands-free"
        } else {
            "push-to-talk"
        },
        wav.display()
    );
    let mut d = world.resource_mut::<Dictation>();
    d.phase = Phase::Recording;
    d.hands_free = hands_free;
    d.target = Some(target);
    d.anchor = Some(anchor);
    d.inserted.clear();
    d.detached = false;
    d.session = Some(Session {
        rx: Mutex::new(rx),
        stop,
    });
    d.wav = Some(wav);
    d.started = now;
    d.level = 0.0;
    d.error = None;
    d.preview.clear();
    d.in_place = true;
    d.submit_after_finish = false;
}

/// Key released: stop the mic and let the worker do its final pass. The
/// session stays alive until that lands.
fn begin_finish(world: &mut World, reason: FinishReason) {
    let (elapsed, audio_status) = {
        let d = world.resource::<Dictation>();
        (
            world.resource::<Time>().elapsed_secs_f64() - d.started,
            audio::status(),
        )
    };
    eprintln!(
        "[dictation] finishing: reason={reason:?} elapsed={elapsed:.2}s audio_status={audio_status:?}"
    );
    audio::record_stop();
    let mut d = world.resource_mut::<Dictation>();
    d.phase = Phase::Finishing;
    d.level = 0.0;
    if let Some(s) = d.session.as_ref() {
        s.stop.store(true, Ordering::Release);
    }
}

/// Tear down a finished (or failed) session.
fn end_session(world: &mut World) {
    audio::set_pcm_tap(false);
    let mut d = world.resource_mut::<Dictation>();
    d.phase = Phase::Idle;
    d.hands_free = false;
    d.session = None;
    d.target = None;
    d.anchor = None;
    d.inserted.clear();
    d.detached = false;
    d.level = 0.0;
    d.preview.clear();
    d.in_place = true;
    d.submit_after_finish = false;
    // The samples came from the tap; the WAV was only ever a byproduct.
    if let Some(w) = d.wav.take() {
        let _ = std::fs::remove_file(w);
    }
}

/// Whatever currently owns the keyboard, plus where our text will start.
///
/// The palette wins because it forces `KeyboardOwner::Modal` while open —
/// nothing else is taking keys. Otherwise it's the focused pane, and a
/// widget only counts if some input inside it actually holds the caret.
fn resolve_target(world: &mut World) -> Option<(Target, Anchor)> {
    if let Some(p) = world.get_resource::<CommandPalette>() {
        if p.open {
            return Some((
                Target::Palette,
                Anchor::Palette {
                    base: p.query.clone(),
                },
            ));
        }
    }
    let focused = world.get_resource::<FocusedPane>()?.0?;
    if let Some(focus) = world.get::<WidgetInputFocus>(focused) {
        return Some((
            Target::Widget(focused),
            Anchor::Widget {
                id: focus.id.clone(),
                at: focus.caret,
            },
        ));
    }
    let kind = world.get::<PaneKindMarker>(focused)?.0;
    if kind == jim_editor::PANE_KIND {
        let at = world
            .get::<EditorStateComp>(focused)?
            .0
            .selection
            .primary_range()
            .from();
        return Some((Target::Editor(focused), Anchor::Editor { at }));
    }
    if kind == jim_terminal::PANE_KIND {
        return Some((Target::Terminal(focused), Anchor::Terminal));
    }
    None
}

fn fail(world: &mut World, msg: String) {
    eprintln!("[dictation] session failed: {msg}");
    let now = world.resource::<Time>().elapsed_secs_f64();
    end_session(world);
    world.resource_mut::<Dictation>().error = Some((msg, now + ERROR_SECS));
}

// ============================================================
// Worker thread
// ============================================================

/// Accumulate tapped audio and transcribe it until told to stop.
///
/// Self-clocked rather than on a timer: the next pass starts when the last
/// one returns (subject to [`MIN_PASS_GAP`]), so previews come as fast as
/// the clip allows, with no queue building up behind a slow pass.
///
/// `samples` only ever holds the *uncommitted* tail — see the module docs.
/// That's what keeps a pass costing the same on minute nine as on minute
/// one, and it's why there's no recording limit to hit.
fn worker(stop: Arc<AtomicBool>, tx: Sender<Msg>) {
    let mut roll = Rolling::default();
    // What we last sent, so an unchanged transcript doesn't wake the main
    // thread into re-rendering and re-writing identical text.
    let mut sent = String::new();

    loop {
        roll.push(audio::take_pcm());
        let rate = audio::pcm_rate().max(1);

        if stop.load(Ordering::Acquire) {
            // `record_stop` only *asks* the controller to stop; wait for it
            // to actually finish, so the last callbacks' audio is in the tap
            // before the final pass reads it.
            if !audio::wait_until_finalized(Duration::from_secs(5)) {
                eprintln!("[dictation] timed out waiting 5s for audio finalization");
            }
            roll.push(audio::take_pcm());
            let rate = audio::pcm_rate().max(1);
            let full = match roll.text(rate) {
                Ok(t) => t,
                // Losing the tail is only fatal if it's all we had;
                // otherwise ship what was committed rather than the lot.
                Err(e) if roll.committed.is_empty() => {
                    eprintln!("[dictation] final transcription failed: {e}");
                    let _ = tx.send(Msg::Error(e));
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "[dictation] final tail transcription failed; using committed text: {e}"
                    );
                    roll.committed.clone()
                }
            };
            let _ = tx.send(if full.is_empty() {
                Msg::Error("heard nothing".into())
            } else {
                Msg::Final(full)
            });
            return;
        }

        if roll.secs(rate) < MIN_LIVE_SECS {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // A commit costs this iteration's pass; the next one is short.
        if roll.try_commit(rate) {
            continue;
        }

        let began = Instant::now();
        // A failed preview isn't worth reporting: the next pass may
        // succeed, and the final pass reports for real if it can't.
        match roll.text(rate) {
            Ok(full) => {
                eprintln!(
                    "[dictation] preview pass: audio={:.2}s took={:.2}s chars={} changed={}",
                    roll.secs(rate),
                    began.elapsed().as_secs_f32(),
                    full.chars().count(),
                    !full.is_empty() && full != sent
                );
                if !full.is_empty() && full != sent {
                    // A closed channel means the session ended (app quit,
                    // error path) — stop working for nobody.
                    if tx.send(Msg::Update(full.clone())).is_err() {
                        return;
                    }
                    sent = full;
                }
            }
            Err(e) => eprintln!(
                "[dictation] preview pass failed: audio={:.2}s took={:.2}s error={e}",
                roll.secs(rate),
                began.elapsed().as_secs_f32()
            ),
        }
        if let Some(rest) = MIN_PASS_GAP.checked_sub(began.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// The rolling transcript: text already frozen, plus the audio tail that
/// hasn't been. Split out of [`worker`] so the commit-and-rejoin sequence
/// can be driven against a real clip in a test without a mic.
#[derive(Default)]
struct Rolling {
    committed: String,
    samples: Vec<f32>,
}

impl Rolling {
    fn push(&mut self, s: impl IntoIterator<Item = f32>) {
        self.samples.extend(s);
    }

    /// Length of the *uncommitted* tail — what a pass actually costs.
    fn secs(&self, rate: u32) -> f32 {
        self.samples.len() as f32 / rate as f32
    }

    /// Freeze the front of the tail if it's grown past
    /// [`COMMIT_TARGET_SECS`] and there's a pause to cut at. True if
    /// anything moved into `committed`.
    ///
    /// A transcription failure just leaves the audio alone: the tail stays
    /// long, which is slow, but nothing is lost and the next pass retries.
    fn try_commit(&mut self, rate: u32) -> bool {
        let secs = self.secs(rate);
        if secs <= COMMIT_TARGET_SECS {
            return false;
        }
        let latest = ((secs - COMMIT_KEEP_SECS).max(0.0) * rate as f32) as usize;
        let Some(cut) = commit_point(&self.samples, rate, latest, secs > COMMIT_FORCE_SECS) else {
            return false;
        };
        let Ok(t) = whisper::transcribe(&self.samples[..cut], rate) else {
            return false;
        };
        // An empty result means that span really was silence — dropping
        // its audio is exactly the point.
        self.committed = join(&self.committed, t.trim());
        self.samples.drain(..cut);
        true
    }

    /// Everything transcribed so far: the committed text plus a fresh pass
    /// over the tail. A tail too short to be worth a pass contributes
    /// nothing rather than a hallucination.
    fn text(&self, rate: u32) -> Result<String, String> {
        if self.secs(rate) < MIN_LIVE_SECS {
            return Ok(self.committed.clone());
        }
        whisper::transcribe(&self.samples, rate).map(|t| join(&self.committed, t.trim()))
    }
}

/// Glue two transcript fragments, keeping exactly one space between them.
fn join(a: &str, b: &str) -> String {
    match (a.trim(), b.trim()) {
        ("", b) => b.to_string(),
        (a, "") => a.to_string(),
        (a, b) => format!("{a} {b}"),
    }
}

/// Where to split `samples` so the front can be transcribed and frozen.
///
/// Returns a point inside the last low-energy stretch ending at or before
/// `latest`, so the cut lands within a pause and neither side starts or
/// ends mid-word. `None` when the speaker hasn't paused — unless `force`,
/// in which case the quietest single frame is used instead, which may clip
/// a word but stops the pass cost from climbing forever.
fn commit_point(samples: &[f32], rate: u32, latest: usize, force: bool) -> Option<usize> {
    let frame = (rate as f32 * SILENCE_FRAME_SECS).max(1.0) as usize;
    let need = (SILENCE_MIN_SECS / SILENCE_FRAME_SECS).ceil() as usize;
    let end = latest.min(samples.len()) / frame;
    if end == 0 {
        return None;
    }

    let rms = |i: usize| -> f32 {
        let s = &samples[i * frame..((i + 1) * frame).min(samples.len())];
        if s.is_empty() {
            return 0.0;
        }
        (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt()
    };

    // Walk backwards for the last run of quiet frames long enough to be a
    // pause; the later the cut, the more we get to freeze.
    let mut run_end: Option<usize> = None;
    let mut quiet = 0usize;
    for i in (0..end).rev() {
        if rms(i) < SILENCE_RMS {
            if quiet == 0 {
                run_end = Some(i);
            }
            quiet += 1;
            if quiet >= need {
                // Cut mid-pause: leaves trailing silence on the frozen
                // part and leading silence on the tail, which whisper is
                // happy with either way.
                let start = i;
                let mid = (start + run_end.unwrap_or(start)) / 2;
                return Some(((mid + 1) * frame).min(samples.len()));
            }
        } else {
            quiet = 0;
            run_end = None;
        }
    }

    if !force {
        return None;
    }
    let quietest = (0..end).min_by(|a, b| rms(*a).total_cmp(&rms(*b)))?;
    Some(((quietest + 1) * frame).min(samples.len()))
}

// ============================================================
// Pump
// ============================================================

fn dictation_pump(world: &mut World) {
    let now = world.resource::<Time>().elapsed_secs_f64();

    {
        let mut d = world.resource_mut::<Dictation>();
        if d.error.as_ref().map(|(_, at)| now >= *at).unwrap_or(false) {
            d.error = None;
        }
    }

    let phase = world.resource::<Dictation>().phase;
    if phase == Phase::Idle {
        whisper::idle_shutdown();
        return;
    }

    if phase == Phase::Recording {
        // Draining levels IS the capture keepalive — see `needs_frames`.
        let levels = audio::take_levels();
        let mut d = world.resource_mut::<Dictation>();
        if let Some(last) = levels.last() {
            d.level = *last;
        }
        drop(d);
        // There's no time limit, so the only thing that ends a recording
        // besides the key is the device stopping itself (unplugged, or
        // taken by something else). The clip so far is still worth having.
        if !audio::is_recording() {
            eprintln!(
                "[dictation] audio capture stopped while dictation was active: status={:?}",
                audio::status()
            );
            begin_finish(world, FinishReason::AudioCaptureStopped);
        }
    }

    // Drain everything queued: on a slow frame several previews may have
    // landed, and only the newest matters.
    loop {
        let msg = {
            let d = world.resource::<Dictation>();
            let Some(session) = d.session.as_ref() else {
                return;
            };
            match session.rx.lock() {
                Ok(rx) => match rx.try_recv() {
                    Ok(m) => Some(m),
                    Err(TryRecvError::Empty) => None,
                    // The worker died without reporting — don't hang here.
                    Err(TryRecvError::Disconnected) => {
                        Some(Msg::Error("transcription worker died".into()))
                    }
                },
                Err(_) => Some(Msg::Error("transcription channel poisoned".into())),
            }
        };
        match msg {
            None => return,
            Some(Msg::Update(text)) => write_text(world, &text, false),
            Some(Msg::Final(text)) => {
                write_text(world, &text, true);
                let (submit, target, detached) = {
                    let d = world.resource::<Dictation>();
                    (d.submit_after_finish, d.target, d.detached)
                };
                if submit && !detached {
                    submit_target(world, target);
                }
                end_session(world);
                return;
            }
            Some(Msg::Error(e)) => {
                fail(world, e);
                return;
            }
        }
    }
}

/// Perform the Enter action that was held back while Whisper finalized.
/// The captured target is used rather than current focus, matching where the
/// transcript itself was written.
fn submit_target(world: &mut World, target: Option<Target>) {
    match target {
        Some(Target::Terminal(pane)) => {
            if let Some(data) = world
                .get_resource::<TerminalStore>()
                .and_then(|store| store.map.get(&pane))
            {
                data.worker.send(WorkerMsg::Input(vec![b'\r']));
            }
        }
        Some(Target::Widget(pane)) => {
            let Some(focus) = world.get::<WidgetInputFocus>(pane) else {
                return;
            };
            let id = focus.id.clone();
            let value = focus.value.clone();
            if let Some(io) = world.get::<WidgetIO>(pane) {
                let event = HostEvent::InputSubmit {
                    id: id.clone(),
                    value: value.clone(),
                };
                if let Ok(json) = serde_json::to_string(&event) {
                    let _ = io.tx.send(json);
                }
            }
            if let Some(widget) = world.get::<ScriptWidget>(pane) {
                widget.send_input_submit(id, value);
            }
        }
        Some(Target::Editor(pane)) => {
            let Some(mut comp) = world.get_mut::<EditorStateComp>(pane) else {
                return;
            };
            let at = comp.0.selection.primary_range().from();
            let tr = Transaction::new()
                .change(Change::new(at, at, "\n"))
                .select(Selection::cursor(at + 1));
            comp.0 = comp.0.apply_with_history(&tr);
        }
        // The command palette's Enter behavior selects an action rather than
        // submitting a text input, so dictation does not synthesize it.
        Some(Target::Palette) | None => {}
    }
}

// ============================================================
// Writing
// ============================================================

/// Replace the text we wrote last pass with `text`.
///
/// `final_pass` only matters to the editor, where the tentative rewrites
/// deliberately bypass undo history and the last one has to leave a single
/// clean entry on the stack.
fn write_text(world: &mut World, text: &str, final_pass: bool) {
    let (target, anchor, detached) = {
        let d = world.resource::<Dictation>();
        (d.target, d.anchor.clone(), d.detached)
    };
    if detached {
        return;
    }
    let (Some(target), Some(anchor)) = (target, anchor) else {
        return;
    };
    // Whisper breaks its output into segments and can put newlines between
    // them. Speech has no line breaks in it, so those are an artifact — and
    // an actively harmful one at a shell prompt. Flatten to single spaces,
    // which also makes the character count we backspace over unambiguous.
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.as_str();

    world.resource_mut::<Dictation>().preview = text.to_string();

    // A target that can't take a revisable preview (an alt-screen TUI) gets
    // nothing until release; the pill carries the text in the meantime.
    if !final_pass && !target.accepts_preview(world) {
        world.resource_mut::<Dictation>().in_place = false;
        return;
    }

    let result = match (target, &anchor) {
        (Target::Palette, Anchor::Palette { base }) => write_palette(world, base, text),
        (Target::Widget(e), Anchor::Widget { id, at }) => write_widget(world, e, id, *at, text),
        (Target::Editor(e), Anchor::Editor { at }) => write_editor(world, e, *at, text, final_pass),
        (Target::Terminal(e), Anchor::Terminal) => write_terminal(world, e, text, final_pass),
        _ => Err("dictation target and anchor disagree".into()),
    };

    match result {
        Ok(()) => {
            let mut d = world.resource_mut::<Dictation>();
            d.inserted = text.to_string();
            d.in_place = true;
        }
        Err(e) => {
            // Detaching isn't a failure of the transcript — it means the
            // user moved on. Say so and stop writing, but keep what's there.
            let now = world.resource::<Time>().elapsed_secs_f64();
            let mut d = world.resource_mut::<Dictation>();
            d.detached = true;
            d.error = Some((e, now + ERROR_SECS));
        }
    }
}

fn write_palette(world: &mut World, base: &str, text: &str) -> Result<(), String> {
    let expected = format!("{base}{}", world.resource::<Dictation>().inserted);
    {
        let p = world
            .get_resource::<CommandPalette>()
            .ok_or("the palette went away")?;
        if !p.open {
            return Err("the palette closed — dictation stopped".into());
        }
        if p.query != expected {
            return Err("you typed in the palette — dictation stopped".into());
        }
    }
    let next = format!("{base}{text}");
    world.resource_scope(|world, mut palette: Mut<CommandPalette>| {
        let registry = world.resource::<ActionRegistry>();
        let usage = world.resource::<PaletteUsage>();
        let keymap = world.resource::<Keymap>();
        command_palette::set_query(&mut palette, registry, usage, keymap, next);
    });
    Ok(())
}

fn write_widget(
    world: &mut World,
    pane: Entity,
    id: &str,
    at: usize,
    text: &str,
) -> Result<(), String> {
    let prev = world.resource::<Dictation>().inserted.clone();
    let (new_value, changed_id) = {
        let mut focus = world
            .get_mut::<WidgetInputFocus>(pane)
            .ok_or("that input lost focus — dictation stopped")?;
        if focus.id != id {
            return Err("focus moved to another input — dictation stopped".into());
        }
        let chars: Vec<char> = focus.value.chars().collect();
        let end = at + prev.chars().count();
        if end > chars.len() || chars[at..end].iter().collect::<String>() != prev {
            return Err("you edited that input — dictation stopped".into());
        }
        let before: String = chars[..at].iter().collect();
        let after: String = chars[end..].iter().collect();
        focus.value = format!("{before}{text}{after}");
        focus.caret = at + text.chars().count();
        focus.blink = 0.0;
        (focus.value.clone(), focus.id.clone())
    };
    // The script's own state is the source of truth for what it re-renders,
    // so a rewrite the widget never hears about would vanish next frame.
    if let Some(io) = world.get::<WidgetIO>(pane) {
        let evt = HostEvent::InputChange {
            id: changed_id.clone(),
            value: new_value.clone(),
        };
        if let Ok(json) = serde_json::to_string(&evt) {
            let _ = io.tx.send(json);
        }
    }
    if let Some(sw) = world.get::<ScriptWidget>(pane) {
        sw.send_input_change(changed_id, new_value);
    }
    Ok(())
}

/// Rewrite the editor span.
///
/// Previews use `EditorState::apply`, which does NOT touch history — a
/// dozen passes must not become a dozen undo steps. The final pass reverts
/// the preview (still without history) and re-inserts the text with
/// `apply_with_history`, so the whole dictation collapses to exactly one
/// undoable edit.
fn write_editor(
    world: &mut World,
    pane: Entity,
    at: usize,
    text: &str,
    final_pass: bool,
) -> Result<(), String> {
    let prev = world.resource::<Dictation>().inserted.clone();
    let mut comp = world
        .get_mut::<EditorStateComp>(pane)
        .ok_or("that editor pane is gone — dictation stopped")?;
    let state = &mut comp.0;

    let end = at + prev.chars().count();
    if end > state.doc.len_chars() {
        return Err("that editor changed — dictation stopped".into());
    }
    if state.doc.slice(at..end).to_string() != prev {
        return Err("you edited that text — dictation stopped".into());
    }

    if final_pass {
        // Take the preview back out with no history entry...
        if !prev.is_empty() {
            let clear = Transaction::new().change(Change::new(at, end, ""));
            *state = state.apply(&clear);
        }
        // ...then land the real text as the one undoable edit.
        let tr = Transaction::new()
            .change(Change::new(at, at, text.to_string()))
            .select(Selection::cursor(at + text.chars().count()));
        *state = state.apply_with_history(&tr);
    } else {
        let tr = Transaction::new()
            .change(Change::new(at, end, text.to_string()))
            .select(Selection::cursor(at + text.chars().count()));
        *state = state.apply(&tr);
    }
    Ok(())
}

/// Write, or revise, the transcript in a terminal.
///
/// There's no document to inspect here — a terminal is a byte stream — so
/// Terminal output is allowed to arrive between revisions. Codex redraws
/// while the user speaks, so cursor movement must never cancel dictation.
/// A revision is `DEL × n` over what we wrote followed by the new text,
/// which is byte-for-byte what the user pressing Delete would send.
fn write_terminal(
    world: &mut World,
    pane: Entity,
    text: &str,
    final_pass: bool,
) -> Result<(), String> {
    let prev = world.resource::<Dictation>().inserted.clone();
    let state = world
        .get_resource::<TerminalStore>()
        .and_then(|s| jim_terminal::terminal_write_state(s, pane))
        .ok_or("that terminal pane is gone — dictation stopped")?;

    if !prev.is_empty() {
        // There are bytes out there to take back, so the child must still
        // accept line-editor-style revisions. Cursor movement is deliberately
        // ignored: Codex may redraw while dictation is active.
        if !state.revisable {
            return Err("that terminal started a full-screen program — dictation stopped".into());
        }
    } else if !final_pass && !state.revisable {
        // Nothing written yet and the child can't take a preview. Handled
        // by the caller, but re-checked here because revisability can flip
        // between that check and this write.
        return Ok(());
    }

    let store = world
        .get_resource::<TerminalStore>()
        .ok_or("no terminal store")?;
    let data = store
        .map
        .get(&pane)
        .ok_or("that terminal pane is gone — dictation stopped")?;
    if !prev.is_empty() {
        // 0x7f (DEL) is what the Delete key sends. One per CHARACTER — a
        // line editor deletes a character per press, not a byte.
        data.worker
            .send(WorkerMsg::Input(vec![0x7f; prev.chars().count()]));
    }
    if !text.is_empty() {
        // Paste, not Input: bracketed paste means a shell/TUI treats it as
        // inserted text rather than replaying it as keystrokes, so a
        // transcript can't auto-run a command.
        data.worker.send(WorkerMsg::Paste(text.to_string()));
    }
    Ok(())
}

fn dictation_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".jim/dictation"))
}

// ============================================================
// Status pill (top-center, MENU_OVERLAY_LAYER)
// ============================================================

/// Rebuild the pill only when its visible content changes — mirrors
/// `screenshot_consent::render_consent`.
fn render_pill(world: &mut World) {
    let (sig, visible) = {
        let d = world.resource::<Dictation>();
        let now = world.resource::<Time>().elapsed_secs_f64();
        (pill_signature(d, now), pill_visible(d))
    };
    let prev_root = world.resource::<Dictation>().root;

    if !visible {
        if let Some(root) = prev_root {
            let _ = world.despawn(root);
            world.resource_mut::<Dictation>().root = None;
        }
        return;
    }
    if prev_root.is_some() && sig == world.resource::<Dictation>().last_sig {
        return;
    }
    if let Some(root) = prev_root {
        let _ = world.despawn(root);
    }

    let win_h = {
        let mut q = world.query::<&Window>();
        match q.iter(world).next() {
            Some(w) => w.height(),
            None => return,
        }
    };

    let el = build_pill(world);

    let theme = world.resource::<jim_style::Theme>().clone();
    let fonts = world.resource::<jim_style::FontRegistry>().clone();
    let font = world.resource::<PaneFont>().0.clone();
    let metrics = *world.resource::<PaneFontMetrics>();
    let colors = WidgetPalette::from_theme(&theme);

    let top_left = Vec2::new(-PILL_W * 0.5, win_h * 0.5 - PILL_TOP);
    let root = world
        .spawn((
            Transform::from_xyz(top_left.x, top_left.y, PILL_Z),
            Visibility::Visible,
            RenderLayers::layer(MENU_OVERLAY_LAYER),
        ))
        .id();

    let ctx = LayoutCtx {
        font,
        metrics,
        owner_pane: root,
        content_root: root,
        content_size: Vec2::new(PILL_W, win_h),
        palette: colors,
        theme,
        fonts,
        focused_input: None,
        caret_visible: true,
        hovered_click_id: None,
        anim: Default::default(),
    };
    let mut targets = WidgetTargets::default();
    {
        let mut commands = world.commands();
        render::render(
            &mut commands,
            &ctx,
            &mut targets,
            &el,
            Vec2::ZERO,
            PILL_W,
            0.0,
        );
    }
    world.flush();
    stamp_layer(world, root, MENU_OVERLAY_LAYER);

    let mut d = world.resource_mut::<Dictation>();
    d.root = Some(root);
    d.last_sig = sig;
}

fn pill_visible(d: &Dictation) -> bool {
    d.phase != Phase::Idle || d.error.is_some()
}

fn pill_signature(d: &Dictation, now: f64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (d.phase as u8).hash(&mut h);
    d.error.as_ref().map(|(m, _)| m).hash(&mut h);
    d.detached.hash(&mut h);
    d.in_place.hash(&mut h);
    // Only when it's on screen — otherwise every pass would rebuild the
    // overlay for text nobody is looking at.
    if !d.in_place {
        d.preview.hash(&mut h);
    }
    // Tenths of a second, and the meter in ~20 steps: live enough to read,
    // coarse enough not to rebuild the overlay every frame.
    if d.phase == Phase::Recording {
        (((now - d.started) * 10.0) as i64).hash(&mut h);
        ((d.level * 20.0) as i64).hash(&mut h);
    }
    h.finish()
}

fn build_pill(world: &World) -> Element {
    let d = world.resource::<Dictation>();
    let now = world.resource::<Time>().elapsed_secs_f64();

    let (icon, label, hint, accent) = if let Some((msg, _)) = &d.error {
        ("⚠", msg.clone(), String::new(), "fg_muted")
    } else {
        match d.phase {
            Phase::Recording => {
                let secs = (now - d.started).max(0.0);
                (
                    "●",
                    format!("Listening… {secs:.1}s"),
                    format!(
                        "{}{}",
                        if d.hands_free {
                            "Esc to finish"
                        } else {
                            "release ⌘⇧M to finish"
                        },
                        target_hint(d)
                    ),
                    "accent",
                )
            }
            Phase::Finishing => ("◌", "Transcribing…".to_string(), String::new(), "accent"),
            Phase::Idle => ("", String::new(), String::new(), "fg_muted"),
        }
    };

    let mut rows = vec![Element::Hstack {
        gap: 8.0,
        pad: 0.0,
        align: Align::Center,
        children: vec![
            text(icon, accent, 15.0, Weight::Bold),
            frame_grow(vec![text(&label, "fg", 14.0, Weight::Bold)]),
        ],
        style: Some(Style {
            width: Some("100%".into()),
            ..Default::default()
        }),
    }];
    if d.phase == Phase::Recording {
        rows.push(meter(d.level));
    }
    // When the transcript can't be shown where it's going — an alt-screen
    // TUI that would take unretractable bytes — show it here instead, so
    // "is it hearing me" is still answerable without waiting for release.
    if !d.in_place && !d.preview.is_empty() {
        rows.push(text(&tail_of(&d.preview, 180), "fg", 12.0, Weight::Normal));
    }
    if !hint.is_empty() {
        rows.push(text(&hint, "fg_muted", 11.0, Weight::Normal));
    }

    Element::Frame {
        gap: 8.0,
        pad: 0.0,
        children: rows,
        style: Some(Style {
            background: Some("surface_2".into()),
            radius: Some("radius_lg".into()),
            border: Some(Border {
                color: accent.into(),
                width: 1.0,
            }),
            padding: Some(Edges::all(12.0)),
            width: Some(format!("{}", PILL_W as i32)),
            shadow: Some(Shadow {
                token: Some("shadow_lg".into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
    }
}

/// A level bar, so you can tell the mic is hearing you before you've said
/// the whole sentence.
fn meter(level: f32) -> Element {
    let fill = (level.clamp(0.0, 1.0) * (PILL_W - 24.0)).max(2.0);
    Element::Frame {
        gap: 0.0,
        pad: 0.0,
        children: vec![Element::Frame {
            gap: 0.0,
            pad: 0.0,
            children: vec![],
            style: Some(Style {
                background: Some("accent".into()),
                radius: Some("radius_sm".into()),
                width: Some(format!("{}", fill as i32)),
                height: Some("4".into()),
                ..Default::default()
            }),
        }],
        style: Some(Style {
            background: Some("surface_1".into()),
            radius: Some("radius_sm".into()),
            width: Some(format!("{}", (PILL_W - 24.0) as i32)),
            height: Some("4".into()),
            ..Default::default()
        }),
    }
}

fn target_hint(d: &Dictation) -> String {
    match d.target {
        Some(Target::Palette) => " · palette".into(),
        Some(Target::Widget(_)) => " · that input".into(),
        Some(Target::Editor(_)) => " · at the caret".into(),
        Some(Target::Terminal(_)) if d.in_place => " · terminal".into(),
        // A full-screen program can't take a preview, so say what will
        // happen rather than leave the empty terminal looking broken.
        Some(Target::Terminal(_)) => " · terminal (pastes on release)".into(),
        None => String::new(),
    }
}

/// The last `max` characters of `s`, with a leading ellipsis when clipped.
/// Character-wise, so it can't split a UTF-8 sequence.
fn tail_of(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    format!("…{}", s.chars().skip(n - max).collect::<String>())
}

fn text(s: &str, color: &str, size: f32, weight: Weight) -> Element {
    Element::Text {
        wrap: true,
        value: s.to_string(),
        color: Some(color.into()),
        size: Some(size),
        weight: Some(weight),
        family: None,
        selectable: false,
    }
}

fn frame_grow(children: Vec<Element>) -> Element {
    Element::Frame {
        gap: 0.0,
        pad: 0.0,
        children,
        style: Some(Style {
            flex_grow: Some(1.0),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn speech(secs: f32) -> Vec<f32> {
        let n = (RATE as f32 * secs) as usize;
        (0..n)
            .map(|i| (i as f32 * 200.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.3)
            .collect()
    }

    /// Not digital silence — a real room floor, which has to read as quiet.
    fn quiet(secs: f32) -> Vec<f32> {
        let n = (RATE as f32 * secs) as usize;
        (0..n)
            .map(|i| (i as f32 * 60.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.001)
            .collect()
    }

    fn at(v: &[f32], secs: f32) -> usize {
        (RATE as f32 * secs) as usize
    }

    #[test]
    fn cuts_inside_the_pause() {
        let mut s = speech(2.0);
        s.extend(quiet(1.0));
        s.extend(speech(2.0));
        let cut = commit_point(&s, RATE, s.len(), false).expect("a 1s pause is a commit point");
        assert!(
            cut > at(&s, 2.0) && cut < at(&s, 3.0),
            "cut at {:.2}s landed outside the 2.0–3.0s pause",
            cut as f32 / RATE as f32
        );
    }

    /// The point of `latest`: the last few seconds are still being revised,
    /// so a pause inside them must not be committed yet.
    #[test]
    fn never_cuts_past_latest() {
        let mut s = speech(2.0);
        s.extend(quiet(1.0));
        s.extend(speech(1.0));
        s.extend(quiet(1.0));
        let latest = at(&s, 3.5);
        let cut = commit_point(&s, RATE, latest, false).expect("the first pause is still eligible");
        assert!(
            cut <= latest,
            "cut at {cut} exceeded latest {latest} — committed audio whisper hadn't settled"
        );
        assert!(cut > at(&s, 2.0), "should have used the 2.0–3.0s pause");
    }

    /// A pause shorter than SILENCE_MIN_SECS is a gap between words, not
    /// between phrases; cutting there could split one.
    #[test]
    fn ignores_a_gap_too_short_to_be_a_pause() {
        let mut s = speech(2.0);
        s.extend(quiet(0.1));
        s.extend(speech(2.0));
        assert!(commit_point(&s, RATE, s.len(), false).is_none());
    }

    #[test]
    fn unbroken_speech_has_no_commit_point() {
        let s = speech(30.0);
        assert!(commit_point(&s, RATE, s.len(), false).is_none());
    }

    /// ...but past COMMIT_FORCE_SECS we cut anyway, because a pass cost that
    /// climbs forever is worse than one clipped word.
    #[test]
    fn force_cuts_unbroken_speech() {
        let s = speech(30.0);
        let cut = commit_point(&s, RATE, s.len(), true).expect("force must always produce a cut");
        assert!(cut > 0 && cut <= s.len());
    }

    #[test]
    fn too_short_to_scan_is_none() {
        let s = speech(0.01);
        assert!(commit_point(&s, RATE, s.len(), false).is_none());
    }

    #[test]
    fn join_keeps_exactly_one_space() {
        assert_eq!(join("hello", "world"), "hello world");
        assert_eq!(join("hello ", " world"), "hello world");
        assert_eq!(join("", "world"), "world");
        assert_eq!(join("hello", ""), "hello");
        assert_eq!(join("", ""), "");
    }

    /// One distinctive noun per sentence, spread across a clip long enough
    /// to force several commits. `[[slnc]]` is a macOS `say` directive that
    /// inserts a real pause, which is what [`commit_point`] cuts at.
    const SPOKEN: &[(&str, &str)] = &[
        (
            "elephant",
            "The first thing I want to mention is the elephant in the garden.",
        ),
        (
            "bicycle",
            "Second, we should really talk about the bicycle in the hallway.",
        ),
        (
            "kitchen",
            "Third, somebody left the window open in the kitchen last night.",
        ),
        (
            "mountain",
            "Fourth, the photograph on the wall shows a mountain at sunrise.",
        ),
        (
            "umbrella",
            "Fifth, I could not find my umbrella anywhere this morning.",
        ),
        (
            "computer",
            "Sixth, the computer on the desk has been running all week.",
        ),
        (
            "hospital",
            "Seventh, the road that goes past the hospital is closed today.",
        ),
        (
            "guitar",
            "Eighth, there is an old guitar leaning against the bookshelf.",
        ),
        (
            "garden",
            "Ninth, the tomatoes in the garden are finally starting to ripen.",
        ),
        (
            "letter",
            "Tenth, I still have to write that letter before the weekend.",
        ),
        (
            "morning",
            "Eleventh, the train leaves early in the morning from platform two.",
        ),
        (
            "coffee",
            "Twelfth, and last, there is no coffee left in the entire house.",
        ),
    ];

    /// Drives the real commit-and-rejoin path over a clip long enough to
    /// force several commits, against a real whisper-server.
    ///
    /// This is the test that catches a commit point eating a word: the
    /// spoken text is known, so a cut that swallowed one shows up as a
    /// missing noun in the joined transcript. Asserting on the JOIN is the
    /// whole point — transcribing the clip in one piece would prove
    /// nothing about the windowing.
    ///
    /// Ignored by default: needs `say`, and spawns a server that loads a
    /// ~1GB model. Run with:
    ///   cargo test -p jim_app --lib dictation -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn commits_across_a_long_clip_without_losing_words() {
        let rate = 16_000u32;
        let script = SPOKEN
            .iter()
            .map(|(_, s)| *s)
            .collect::<Vec<_>>()
            .join(" [[slnc 900]] ");
        let path = std::env::temp_dir().join("jim-dictation-commit-test.wav");
        let out = std::process::Command::new("say")
            .args([
                "--data-format=LEI16@16000",
                "-o",
                &path.to_string_lossy(),
                &script,
            ])
            .output()
            .expect("`say` should be available on macOS");
        assert!(
            out.status.success(),
            "say failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let mut r = hound::WavReader::open(&path).expect("say should have written a readable wav");
        let samples: Vec<f32> = r
            .samples::<i16>()
            .map(|s| s.expect("sample") as f32 / 32768.0)
            .collect();
        let total = samples.len() as f32 / rate as f32;
        println!("clip is {total:.1}s");
        assert!(
            total > COMMIT_TARGET_SECS + COMMIT_KEEP_SECS,
            "clip is {total:.1}s — too short to commit at all, so this test would prove nothing"
        );

        // Feed it the way the worker does: a chunk at a time, committing
        // whenever the tail has grown enough and there's a pause to use.
        let mut roll = Rolling::default();
        let mut commits = 0;
        for chunk in samples.chunks(rate as usize) {
            roll.push(chunk.iter().copied());
            while roll.try_commit(rate) {
                commits += 1;
                println!(
                    "commit {commits}: tail now {:.1}s, committed {:?}",
                    roll.secs(rate),
                    roll.committed
                );
            }
            assert!(
                roll.secs(rate) <= COMMIT_FORCE_SECS + 2.0,
                "tail reached {:.1}s — windowing isn't bounding pass cost",
                roll.secs(rate)
            );
        }
        let text = roll.text(rate).expect("final pass").to_lowercase();
        println!("\nfinal transcript:\n{text}\n");

        assert!(
            commits >= 2,
            "only {commits} commit(s) over {total:.1}s — the windowing path barely ran"
        );
        let missing: Vec<&str> = SPOKEN
            .iter()
            .map(|(w, _)| *w)
            .filter(|w| !text.contains(w))
            .collect();
        assert!(
            missing.is_empty(),
            "commit boundaries lost {missing:?} from the transcript"
        );
    }

    #[test]
    fn tail_of_clips_from_the_front() {
        assert_eq!(tail_of("abcdef", 10), "abcdef");
        assert_eq!(tail_of("abcdef", 3), "…def");
        // Multi-byte: must clip on a character, not a byte.
        assert_eq!(tail_of("héllo wörld", 5), "…wörld");
    }
}

fn stamp_layer(world: &mut World, root: Entity, layer: usize) {
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        let kids: Vec<Entity> = world
            .get::<Children>(e)
            .map(|c| c.iter().collect::<Vec<Entity>>())
            .unwrap_or_default();
        if let Ok(mut em) = world.get_entity_mut(e) {
            em.insert(RenderLayers::layer(layer));
        }
        stack.extend(kids);
    }
}
