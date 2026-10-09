//! Dictation into whatever owns the keyboard, transcribed live.
//!
//! Hold **⌘⇧M** and talk. Words appear at the caret while you're still
//! speaking and settle as the engine gets more context; release and the
//! final text lands. It goes into whatever was focused when you started
//! talking — a widget `Input`/`TextArea`, an editor pane, the command
//! palette, or a terminal.
//!
//! For hands-free dictation, press **⌘⇧T** once and release the keys.
//! Press **Escape** (or **⌘⇧T** again) to stop. Escape is consumed by
//! dictation, so it does not also reach the focused target or another overlay.
//!
//! There is no time limit. Talk for as long as you like.
//!
//! ## Engines
//!
//! Two, switchable at runtime from the palette ("Dictation: Use Phonon" /
//! "Use Whisper") or `~/.jim/dictation.json` — see [`engine`]:
//!
//! - [`whisper`] — whisper.cpp large-v3-turbo. Multilingual. Not a
//!   streaming model, so it is driven with LocalAgreement: short re-decoded
//!   windows, words frozen once two passes agree on them.
//! - [`phonon`] — Phonon-2 on its CPU engine. English-only, ~10× faster on
//!   dictation-length audio, half the memory, and a real streaming endpoint:
//!   audio goes out as it's captured and phrases come back as they close.
//!
//! Both sit behind [`Transcriber`]: the worker feeds 16 kHz audio in (via
//! [`resample`]) and gets back the *whole transcript so far* whenever it
//! changes. Text near the caret may still churn — "config fill" becomes
//! "config file" a moment later — but text an engine has frozen never moves
//! again, so the rewrite below only ever touches a short tail in practice.
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

mod engine;
mod phonon;
mod resample;
mod server;
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
use crate::actions::{ActionRegistry, AppActionsExt, Keymap};
use crate::command_palette::{self, CommandPalette, PaletteUsage};

/// Push-to-talk key, held with ⌘ and ⇧.
const HOLD_HOTKEY: KeyCode = KeyCode::KeyM;
/// Hands-free toggle, pressed with ⌘ and ⇧. This intentionally replaces
/// the old theme-editor shortcut.
const TOGGLE_HOTKEY: KeyCode = KeyCode::KeyT;
/// How long a failure message stays on screen.
const ERROR_SECS: f64 = 5.0;
/// How long a confirmation (an engine switch) stays on screen.
const NOTICE_SECS: f64 = 2.5;
/// The rate both engines consume. Capture runs at the device's native rate
/// and is resampled on the way in.
const RATE: u32 = 16_000;
/// How often the worker moves captured audio to the engine and checks for
/// new text. Whisper's passes take far longer than this and pace
/// themselves; for Phonon this is the streaming granularity.
const POLL: Duration = Duration::from_millis(30);

/// A live transcription session. Implementations own their engine-specific
/// policy — when to run a pass, what to freeze — and all report the same
/// thing: the whole transcript so far.
trait Transcriber: Send {
    /// Newly captured mono audio at [`RATE`].
    fn push(&mut self, samples: &[f32]) -> Result<(), String>;
    /// Do whatever is due. Returns the whole transcript when it changed.
    /// May block for a decode.
    fn step(&mut self) -> Result<Option<String>, String>;
    /// All audio has been pushed: the final transcript.
    fn finish(&mut self) -> Result<String, String>;
}

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
    /// Whisper is loading; the microphone remains closed.
    Starting,
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
    Ready,
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
    capture_started: Arc<AtomicBool>,
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
    /// A confirmation (not a failure), same shape as `error`.
    notice: Option<(String, f64)>,
    /// The engine this session runs on, snapshotted at start for the pill.
    engine: engine::Engine,
    /// Spawned overlay root, and a signature so it only re-renders when the
    /// visible content changes.
    root: Option<Entity>,
    last_sig: u64,
}

impl Dictation {
    /// True while the winit loop must keep waking us at its dictation cadence.
    ///
    /// Not decoration: the idle baseline is `reactive(5s)`, and the capture's
    /// idle watchdog auto-stops a stream nobody polls within ~2s. A 30Hz
    /// reactive wake is sufficient; recording would die mid-sentence at the
    /// normal five-second idle cadence if the user did not move.
    pub fn needs_frames(&self) -> bool {
        self.phase != Phase::Idle || self.error.is_some() || self.notice.is_some()
    }

    fn is_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    fn notice(&mut self, msg: String, now: f64) {
        self.notice = Some((msg, now + NOTICE_SECS));
    }

    fn set_error(&mut self, msg: String, now: f64) {
        self.error = Some((msg, now + ERROR_SECS));
    }
}

pub struct DictationPlugin;

impl Plugin for DictationPlugin {
    fn build(&self, app: &mut App) {
        // Free any per-GUI whisper server an older jim left behind, then get
        // the selected engine warm (adopting its server if it outlived the
        // last jim) so the first dictation doesn't wait on a model load.
        whisper::reap_orphans();
        match engine::selected() {
            Ok(e) => e.prewarm(),
            // Reported again, on screen, when a dictation starts.
            Err(e) => eprintln!("[dictation] no engine to prewarm: {e}"),
        }
        app.add_action(engine::USE_PHONON)
            .add_action(engine::USE_WHISPER)
            .init_resource::<Dictation>()
            // Run immediately after Bevy gathers input. A hands-free Escape
            // is removed here before any Update keyboard consumer can see it.
            .add_systems(
                PreUpdate,
                dictation_hotkey
                    .after(bevy::input::InputSystems)
                    .after(crate::reconcile_macos_modifiers),
            )
            .add_systems(Update, dictation_tick);
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
        let recording = matches!(d.phase, Phase::Starting | Phase::Recording);
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
    if world.resource::<Dictation>().phase != Phase::Idle {
        return;
    }
    let Some((target, anchor)) = resolve_target(world) else {
        fail(world, "nothing focused to dictate into".into());
        return;
    };
    let engine = match engine::selected() {
        Ok(e) => e,
        Err(e) => {
            fail(world, e);
            return;
        }
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

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let stop_w = stop.clone();
    let capture_started = Arc::new(AtomicBool::new(false));
    let capture_w = capture_started.clone();
    if std::thread::Builder::new()
        .name("dictate-worker".into())
        .spawn(move || worker(engine, stop_w, capture_w, tx))
        .is_err()
    {
        audio::record_stop();
        audio::set_pcm_tap(false);
        fail(world, "could not start the transcription worker".into());
        return;
    }

    eprintln!(
        "[dictation] startup requested: engine={} mode={} target={target:?} wav={}",
        engine.label(),
        if hands_free {
            "hands-free"
        } else {
            "push-to-talk"
        },
        wav.display()
    );
    let mut d = world.resource_mut::<Dictation>();
    d.phase = Phase::Starting;
    d.engine = engine;
    d.notice = None;
    d.hands_free = hands_free;
    d.target = Some(target);
    d.anchor = Some(anchor);
    d.inserted.clear();
    d.detached = false;
    d.session = Some(Session {
        rx: Mutex::new(rx),
        stop,
        capture_started,
    });
    d.wav = Some(wav);
    d.started = now;
    d.level = 0.0;
    d.error = None;
    d.preview.clear();
    d.in_place = true;
    d.submit_after_finish = false;
}

fn begin_capture(world: &mut World) {
    if world.resource::<Dictation>().phase != Phase::Starting {
        return;
    }
    let wav = world.resource::<Dictation>().wav.clone().unwrap();
    // The tap is what live passes read; enabling clears any stale audio.
    audio::set_pcm_tap(true);
    // "" = system default input. Mono is what both engines want, so there's no
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

    let now = world.resource::<Time>().elapsed_secs_f64();
    let mut d = world.resource_mut::<Dictation>();
    d.phase = Phase::Recording;
    d.started = now;
    d.session
        .as_ref()
        .unwrap()
        .capture_started
        .store(true, Ordering::Release);
}

/// Stop capture and request the final pass, or cancel a pending startup.
fn begin_finish(world: &mut World, reason: FinishReason) {
    if world.resource::<Dictation>().phase == Phase::Starting {
        end_session(world);
        return;
    }
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
    audio::record_stop();
    audio::set_pcm_tap(false);
    let mut d = world.resource_mut::<Dictation>();
    if let Some(session) = &d.session {
        session.stop.store(true, Ordering::Release);
    }
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
    world.resource_mut::<Dictation>().set_error(msg, now);
}

// ============================================================
// Worker thread
// ============================================================

/// Start the engine and report readiness, then wait until the main thread
/// opens the microphone. `None` when startup failed (already reported) or
/// the session was cancelled first.
fn await_capture<T>(
    stop: &AtomicBool,
    capture_started: &AtomicBool,
    tx: &Sender<Msg>,
    prepare: impl FnOnce() -> Result<T, String>,
) -> Option<T> {
    let ready = match prepare() {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Msg::Error(e));
            return None;
        }
    };
    if tx.send(Msg::Ready).is_err() {
        return None;
    }
    while !capture_started.load(Ordering::Acquire) {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    (!stop.load(Ordering::Acquire)).then_some(ready)
}

/// Move tapped audio into the engine and report its transcript until told
/// to stop, then report the final one.
fn worker(
    engine: engine::Engine,
    stop: Arc<AtomicBool>,
    capture_started: Arc<AtomicBool>,
    tx: Sender<Msg>,
) {
    let Some(mut t) = await_capture(&stop, &capture_started, &tx, || engine.start()) else {
        return;
    };
    let mut feed = Feed::default();
    // What we last sent, so an unchanged transcript doesn't wake the main
    // thread into re-rendering and re-writing identical text.
    let mut sent = String::new();

    loop {
        if stop.load(Ordering::Acquire) {
            // `record_stop` only *asks* the controller to stop; wait for it
            // to actually finish, so the last callbacks' audio is in the tap
            // before the final pass reads it.
            if !audio::wait_until_finalized(Duration::from_secs(5)) {
                eprintln!("[dictation] timed out waiting 5s for audio finalization");
            }
            let began = Instant::now();
            let result = feed
                .drain(t.as_mut(), true)
                .and_then(|()| t.finish());
            eprintln!(
                "[dictation] {} final: took={:.2}s ok={}",
                engine.label(),
                began.elapsed().as_secs_f32(),
                result.is_ok()
            );
            let _ = tx.send(match result {
                Ok(full) if full.trim().is_empty() => Msg::Error("heard nothing".into()),
                Ok(full) => Msg::Final(full),
                Err(e) => Msg::Error(e),
            });
            return;
        }

        let update = feed.drain(t.as_mut(), false).and_then(|()| t.step());
        match update {
            Ok(Some(full)) if !full.is_empty() && full != sent => {
                // A closed channel means the session ended (app quit, error
                // path) — stop working for nobody.
                if tx.send(Msg::Update(full.clone())).is_err() {
                    return;
                }
                sent = full;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Msg::Error(e));
                return;
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Captured audio on its way to the engine: drained from the tap and
/// resampled from the device rate to [`RATE`].
#[derive(Default)]
struct Feed {
    resampler: Option<resample::Resampler>,
}

impl Feed {
    /// Push everything captured since the last call. `last` also flushes the
    /// resampler's look-ahead, so the final words aren't left in the filter.
    fn drain(&mut self, t: &mut dyn Transcriber, last: bool) -> Result<(), String> {
        let pcm = audio::take_pcm();
        // 0 until the device has delivered its first buffer.
        let rate = audio::pcm_rate();
        if rate == 0 {
            return Ok(());
        }
        if self.resampler.as_ref().is_some_and(|r| r.in_rate() != rate) {
            // The device changed rate mid-session: finish the old stream of
            // samples cleanly before starting a new filter.
            let tail = self.resampler.take().map(|mut r| r.flush()).unwrap_or_default();
            t.push(&tail)?;
        }
        let r = self
            .resampler
            .get_or_insert_with(|| resample::Resampler::new(rate, RATE));
        let mut out = r.process(&pcm);
        if last {
            out.extend(r.flush());
        }
        t.push(&out)
    }
}

/// Drop a space the engine left before closing punctuation — both emit it
/// at segment joins ("the weekend .", "Platform2 ."). Only where the mark
/// ends a word, so "a .5 inch" and "... and" are left alone.
fn tidy_punctuation(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let closes = |j: usize| {
            chars.get(j).is_some_and(|c| matches!(c, '.' | ',' | '!' | '?' | ';' | ':'))
                && chars.get(j + 1).is_none_or(|c| c.is_whitespace())
        };
        if c == ' ' && i > 0 && closes(i + 1) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Glue two transcript fragments, keeping exactly one space between them.
fn join(a: &str, b: &str) -> String {
    match (a.trim(), b.trim()) {
        ("", b) => b.to_string(),
        (a, "") => a.to_string(),
        (a, b) => format!("{a} {b}"),
    }
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
        if d.notice.as_ref().map(|(_, at)| now >= *at).unwrap_or(false) {
            d.notice = None;
        }
        match phonon::take_install_report() {
            Some(Ok(())) => d.notice("Phonon is installed and ready".into(), now),
            Some(Err(e)) => d.set_error(e, now),
            None => {}
        }
    }

    let phase = world.resource::<Dictation>().phase;
    if phase == Phase::Idle {
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
            Some(Msg::Ready) => begin_capture(world),
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

/// Perform the Enter action that was held back while the transcript finalized.
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
    // Engines break their output into segments and can put newlines between
    // them. Speech has no line breaks in it, so those are an artifact — and
    // an actively harmful one at a shell prompt. Flatten to single spaces,
    // which also makes the character count we backspace over unambiguous.
    let text = tidy_punctuation(&text.split_whitespace().collect::<Vec<_>>().join(" "));
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
            d.set_error(e, now);
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
        ground: std::cell::Cell::new(Color::LinearRgba(theme.color(jim_style::tokens::PANE_BG))),
        theme,
        fonts,
        focused_input: None,
        caret_visible: true,
        hovered_click_id: None,
        anim: Default::default(),
    };
    let mut targets = WidgetTargets::default();
    render::render_in_world(world, &ctx, &mut targets, &el, Vec2::ZERO, PILL_W, 0.0);
    stamp_layer(world, root, MENU_OVERLAY_LAYER);

    let mut d = world.resource_mut::<Dictation>();
    d.root = Some(root);
    // A font not loaded yet left text unmeasured: don't record the
    // signature, so the next frame (which the wakeup guarantees) re-renders.
    if targets.layout_incomplete {
        jim_widget::request_main_loop_wakeup();
    } else {
        d.last_sig = sig;
    }
}

fn pill_visible(d: &Dictation) -> bool {
    d.phase != Phase::Idle || d.error.is_some() || d.notice.is_some()
}

fn pill_signature(d: &Dictation, now: f64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (d.phase as u8).hash(&mut h);
    d.error.as_ref().map(|(m, _)| m).hash(&mut h);
    d.notice.as_ref().map(|(m, _)| m).hash(&mut h);
    (d.engine as u8).hash(&mut h);
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
    } else if let (Phase::Idle, Some((msg, _))) = (d.phase, &d.notice) {
        ("✓", msg.clone(), String::new(), "accent")
    } else {
        match d.phase {
            Phase::Starting => (
                "◌",
                format!("Starting {}…", d.engine.label()),
                "Microphone is off".into(),
                "accent",
            ),
            Phase::Recording => {
                let secs = (now - d.started).max(0.0);
                (
                    "●",
                    format!("Listening… {secs:.1}s"),
                    format!(
                        "{} · {}{}",
                        if d.hands_free {
                            "Esc to finish"
                        } else {
                            "release ⌘⇧M to finish"
                        },
                        d.engine.label(),
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

    #[test]
    fn startup_failure_is_reported_without_starting_capture() {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = AtomicBool::new(false);
        let capture = AtomicBool::new(false);
        assert!(
            await_capture(&stop, &capture, &tx, || Err::<(), _>("cannot spawn".into())).is_none()
        );
        assert!(matches!(rx.recv().unwrap(), Msg::Error(e) if e == "cannot spawn"));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(!capture.load(Ordering::Acquire));
    }

    #[test]
    fn ready_worker_waits_for_microphone_start() {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture = Arc::new(AtomicBool::new(false));
        let capture_w = capture.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            done_tx
                .send(await_capture(&stop, &capture_w, &tx, || Ok(7)))
                .unwrap();
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Msg::Ready
        ));
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        capture.store(true, Ordering::Release);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap(), Some(7));
        join.join().unwrap();
    }

    #[test]
    fn cancelling_startup_never_enters_capture_loop() {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = AtomicBool::new(true);
        let capture = AtomicBool::new(false);
        assert!(await_capture(&stop, &capture, &tx, || Ok(())).is_none());
        assert!(matches!(rx.recv().unwrap(), Msg::Ready));
    }

    #[test]
    fn join_keeps_exactly_one_space() {
        assert_eq!(join("hello", "world"), "hello world");
        assert_eq!(join("hello ", " world"), "hello world");
        assert_eq!(join("", "world"), "world");
        assert_eq!(join("hello", ""), "hello");
        assert_eq!(join("", ""), "");
    }

    #[test]
    fn tidies_space_before_punctuation() {
        assert_eq!(tidy_punctuation("from Platform2 . Twelfth"), "from Platform2. Twelfth");
        assert_eq!(tidy_punctuation("the weekend ."), "the weekend.");
        assert_eq!(tidy_punctuation("well , then ?"), "well, then?");
        assert_eq!(tidy_punctuation("a .5 inch gap"), "a .5 inch gap");
        assert_eq!(tidy_punctuation(". leading"), ". leading");
        assert_eq!(tidy_punctuation("wait ... and"), "wait ... and");
    }

    #[test]
    fn tail_of_clips_from_the_front() {
        assert_eq!(tail_of("abcdef", 10), "abcdef");
        assert_eq!(tail_of("abcdef", 3), "…def");
        // Multi-byte: must clip on a character, not a byte.
        assert_eq!(tail_of("héllo wörld", 5), "…wörld");
    }

    // ------------------------------------------------------------
    // Shared harness for the engines' (ignored) end-to-end tests.
    // ------------------------------------------------------------

    /// One distinctive noun per sentence, spread across a clip long enough
    /// to make an engine freeze and drop audio many times over. `[[slnc]]`
    /// is a macOS `say` directive that inserts a real pause.
    const SPOKEN: &[(&str, &str)] = &[
        ("elephant", "The first thing I want to mention is the elephant in the garden."),
        ("bicycle", "Second, we should really talk about the bicycle in the hallway."),
        ("kitchen", "Third, somebody left the window open in the kitchen last night."),
        ("mountain", "Fourth, the photograph on the wall shows a mountain at sunrise."),
        ("umbrella", "Fifth, I could not find my umbrella anywhere this morning."),
        ("computer", "Sixth, the computer on the desk has been running all week."),
        ("hospital", "Seventh, the road that goes past the hospital is closed today."),
        ("guitar", "Eighth, there is an old guitar leaning against the bookshelf."),
        ("garden", "Ninth, the tomatoes in the garden are finally starting to ripen."),
        ("letter", "Tenth, I still have to write that letter before the weekend."),
        ("morning", "Eleventh, the train leaves early in the morning from platform two."),
        ("coffee", "Twelfth, and last, there is no coffee left in the entire house."),
    ];

    pub(super) struct Report {
        pub text: String,
        pub missing: Vec<&'static str>,
        pub updates: usize,
    }

    impl Report {
        pub fn assert_complete(&self) {
            println!("\n{} live updates; final transcript:\n{}\n", self.updates, self.text);
            assert!(self.updates >= 5, "only {} live updates — nothing was live", self.updates);
            assert!(
                self.missing.is_empty(),
                "the stream lost {:?} from the transcript",
                self.missing
            );
        }
    }

    pub(super) fn stream_spoken_clip(t: &mut dyn Transcriber) -> Report {
        stream_spoken_clip_with(t, &mut |_| {})
    }

    /// Speak [`SPOKEN`] with `say` at 48 kHz (a real microphone's rate, so
    /// the resampler is in the loop), then feed it to `t` paced like a live
    /// microphone: each step is followed by exactly as much audio as the
    /// step took in wall time. `at` is called with the clip position before
    /// every step, for tests that break something mid-stream.
    pub(super) fn stream_spoken_clip_with(
        t: &mut dyn Transcriber,
        at: &mut dyn FnMut(f32),
    ) -> Report {
        let mic_rate = 48_000u32;
        let script = SPOKEN.iter().map(|(_, s)| *s).collect::<Vec<_>>().join(" [[slnc 900]] ");
        let path = std::env::temp_dir().join(format!("jim-dictation-{}.wav", std::process::id()));
        let out = std::process::Command::new("say")
            .args([&format!("--data-format=LEI16@{mic_rate}"), "-o", &path.to_string_lossy(), &script])
            .output()
            .expect("`say` should be available on macOS");
        assert!(out.status.success(), "say failed: {}", String::from_utf8_lossy(&out.stderr));
        let mut r = hound::WavReader::open(&path).expect("say should have written a wav");
        let samples: Vec<f32> = r
            .samples::<i16>()
            .map(|s| s.expect("sample") as f32 / 32768.0)
            .collect();
        let _ = std::fs::remove_file(&path);
        let total = samples.len() as f32 / mic_rate as f32;
        println!("clip is {total:.1}s");

        let mut rs = resample::Resampler::new(mic_rate, RATE);
        let mut fed = 0usize;
        let mut chunk = (mic_rate as f32 * POLL.as_secs_f32()) as usize;
        let mut updates = 0;
        let began = Instant::now();
        while fed < samples.len() {
            let n = chunk.min(samples.len() - fed);
            t.push(&rs.process(&samples[fed..fed + n])).expect("push");
            fed += n;
            at(fed as f32 / mic_rate as f32);
            let step = Instant::now();
            if let Some(text) = t.step().expect("step") {
                updates += 1;
                println!("[{:5.1}s] {}", fed as f32 / mic_rate as f32, tail_of(&text, 100));
            }
            std::thread::sleep(POLL);
            // The microphone kept capturing through the step and the sleep.
            chunk = (mic_rate as f32 * step.elapsed().as_secs_f32()) as usize;
        }
        t.push(&rs.flush()).expect("push tail");
        let fin = Instant::now();
        let text = t.finish().expect("finish");
        println!(
            "streamed {total:.1}s in {:.1}s; finish took {:.2}s",
            began.elapsed().as_secs_f32(),
            fin.elapsed().as_secs_f32()
        );
        let lower = text.to_lowercase();
        let missing = SPOKEN.iter().map(|(w, _)| *w).filter(|w| !lower.contains(w)).collect();
        Report { text, missing, updates }
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
