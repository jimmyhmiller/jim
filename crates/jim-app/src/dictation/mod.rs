//! Push-to-talk dictation into whatever owns the keyboard, transcribed live.
//!
//! Hold **⌘⇧M** and talk. Words appear at the caret while you're still
//! speaking and settle as whisper gets more context; release and the final
//! text lands. It goes into whatever was focused when you started talking —
//! a widget `Input`/`TextArea`, an editor pane, the command palette, or a
//! terminal.
//!
//! ## How "live" works
//!
//! Every pass re-transcribes the WHOLE clip so far and rewrites everything
//! written since you started talking. That sounds wasteful and isn't:
//! against a warm [`whisper`] server a 5s clip costs ~0.35s and a 10s clip
//! ~0.43s, because whisper is a 30s-window model and short clips are nearly
//! free. Re-running the whole thing also means every pass has full context,
//! so the text converges on the best transcript instead of accumulating
//! chunk-boundary mistakes it could never go back and fix. Cost does grow
//! with length (30s → ~1.2s), so past [`MAX_LIVE_SECS`] the preview stops
//! updating and only the final pass runs.
//!
//! The consequence you can see: text under the caret churns. "config fill"
//! becomes "config file" a pass later. That's inherent to previewing a
//! non-streaming model, and it's the tradeoff this mode chooses.
//!
//! ## The two things that make it safe
//!
//! **The target is snapshotted when recording STARTS**, not when text
//! arrives — focus may have moved by then.
//!
//! **Every rewrite verifies what it's replacing.** We remember the exact
//! text last written at the anchor; if what's there now differs, the user
//! typed under us, so we detach rather than clobber their edit. A terminal
//! can't be rewritten at all (a bracketed paste is not un-sendable), so it
//! skips the preview and takes one paste on release.

mod whisper;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::camera::visibility::RenderLayers;
use bevy::input::ButtonInput;
use bevy::prelude::*;

use editor_core::selection::Selection;
use editor_core::transaction::{Change, Transaction};
use jim_editor::EditorStateComp;
use jim_pane::{FocusedPane, PaneFont, PaneFontMetrics, PaneKindMarker};
use jim_terminal::worker::WorkerMsg;
use jim_terminal::TerminalStore;
use jim_widget::protocol::{Align, Border, Edges, Element, HostEvent, Shadow, Style, Weight};
use jim_widget::render::{self, LayoutCtx, WidgetPalette};
use jim_widget::script_widget::ScriptWidget;
use jim_widget::{audio, WidgetIO, WidgetInputFocus, WidgetTargets};

use crate::actions::{ActionRegistry, Keymap};
use crate::command_palette::{self, CommandPalette, PaletteUsage};
use crate::MENU_OVERLAY_LAYER;

/// Push-to-talk key. Held with ⌘ and ⇧. (⌘⇧D is the style dev panel; M is
/// for mic.)
const HOTKEY: KeyCode = KeyCode::KeyM;
/// Safety net for a missed key-up (macOS can swallow one while ⌘ is held):
/// a recording never runs longer than this.
const MAX_RECORD_SECS: f64 = 120.0;
/// How long a failure message stays on screen.
const ERROR_SECS: f64 = 5.0;
/// Don't transcribe a fragment shorter than this — there's nothing in it
/// yet, and whisper tends to hallucinate on near-silence.
const MIN_LIVE_SECS: f32 = 0.8;
/// Stop previewing past this much audio. A pass on a clip this long costs
/// >1.5s (measured: 30s → ~1.2s, 60s → ~2.1s), so it stops being a preview
/// and just burns a core. The final pass still transcribes everything.
const MAX_LIVE_SECS: f32 = 45.0;
/// Floor on the gap between live passes, so a short clip (~0.35s a pass)
/// can't pin a core at 100% duty cycle.
const MIN_PASS_GAP: Duration = Duration::from_millis(250);

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
    /// Whether this target can show the live preview. A terminal can't: a
    /// bracketed paste can't be taken back, so it gets one paste on release.
    fn supports_live(&self) -> bool {
        !matches!(self, Target::Terminal(_))
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
    /// `Time::elapsed` when capture began — drives the readout and the
    /// [`MAX_RECORD_SECS`] cap.
    started: f64,
    /// Most recent capture level, 0..1, for the pill's meter.
    level: f32,
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
    /// Recording isn't transient — it's user-driven and already bounded by
    /// [`MAX_RECORD_SECS`].
    pub fn is_transcribing(&self) -> bool {
        self.phase == Phase::Finishing
    }
}

pub struct DictationPlugin;

impl Plugin for DictationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Dictation>()
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
    dictation_hotkey(world);
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

/// Start on ⌘⇧M down, stop on M up (or on either modifier being released —
/// letting go of the whole chord is the natural way to stop talking).
fn dictation_hotkey(world: &mut World) {
    let (start, stop) = {
        let keys = world.resource::<ButtonInput<KeyCode>>();
        let cmd = keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight);
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
        let recording = world.resource::<Dictation>().phase == Phase::Recording;
        (
            keys.just_pressed(HOTKEY) && cmd && shift && !recording,
            recording && (keys.just_released(HOTKEY) || !cmd || !shift),
        )
    };
    if start {
        start_recording(world);
    } else if stop {
        begin_finish(world);
    }
}

fn start_recording(world: &mut World) {
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
    let live = target.supports_live();
    let stop_w = stop.clone();
    if std::thread::Builder::new()
        .name("dictate-worker".into())
        .spawn(move || worker(stop_w, tx, live))
        .is_err()
    {
        audio::record_stop();
        audio::set_pcm_tap(false);
        fail(world, "could not start the transcription worker".into());
        return;
    }

    let mut d = world.resource_mut::<Dictation>();
    d.phase = Phase::Recording;
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
}

/// Key released: stop the mic and let the worker do its final pass. The
/// session stays alive until that lands.
fn begin_finish(world: &mut World) {
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
    d.session = None;
    d.target = None;
    d.anchor = None;
    d.inserted.clear();
    d.detached = false;
    d.level = 0.0;
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
/// the clip allows and automatically slow down as it grows, with no queue
/// building up behind a slow pass.
fn worker(stop: Arc<AtomicBool>, tx: Sender<Msg>, live: bool) {
    let mut samples: Vec<f32> = Vec::new();
    loop {
        samples.extend(audio::take_pcm());

        if stop.load(Ordering::Acquire) {
            // `record_stop` only *asks* the controller to stop; wait for it
            // to actually finish, so the last callbacks' audio is in the tap
            // before the final pass reads it.
            audio::wait_until_finalized(Duration::from_secs(5));
            samples.extend(audio::take_pcm());
            let rate = audio::pcm_rate().max(1);
            let msg = if (samples.len() as f32 / rate as f32) < MIN_LIVE_SECS {
                Msg::Error("heard nothing".into())
            } else {
                match whisper::transcribe(&samples, rate) {
                    Ok(t) if t.trim().is_empty() => Msg::Error("heard nothing".into()),
                    Ok(t) => Msg::Final(t),
                    Err(e) => Msg::Error(e),
                }
            };
            let _ = tx.send(msg);
            return;
        }

        let rate = audio::pcm_rate().max(1);
        let secs = samples.len() as f32 / rate as f32;
        if !live || secs < MIN_LIVE_SECS || secs > MAX_LIVE_SECS {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let began = Instant::now();
        match whisper::transcribe(&samples, rate) {
            Ok(t) if !t.trim().is_empty() => {
                // A closed channel means the session ended (app quit, error
                // path) — stop working for nobody.
                if tx.send(Msg::Update(t)).is_err() {
                    return;
                }
            }
            // A failed preview isn't worth reporting: the next pass may
            // succeed, and the final pass reports for real if it can't.
            _ => {}
        }
        if let Some(rest) = MIN_PASS_GAP.checked_sub(began.elapsed()) {
            std::thread::sleep(rest);
        }
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
        let elapsed = now - d.started;
        drop(d);
        // The device can also stop itself (unplugged); the clip so far is
        // still worth transcribing.
        if elapsed > MAX_RECORD_SECS || !audio::is_recording() {
            begin_finish(world);
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
    let text = text.trim();

    let result = match (target, &anchor) {
        (Target::Palette, Anchor::Palette { base }) => write_palette(world, base, text),
        (Target::Widget(e), Anchor::Widget { id, at }) => write_widget(world, e, id, *at, text),
        (Target::Editor(e), Anchor::Editor { at }) => write_editor(world, e, *at, text, final_pass),
        // Nothing to preview into; the release lands one paste.
        (Target::Terminal(e), Anchor::Terminal) => {
            if final_pass {
                write_terminal(world, e, text)
            } else {
                Ok(())
            }
        }
        _ => Err("dictation target and anchor disagree".into()),
    };

    match result {
        Ok(()) => world.resource_mut::<Dictation>().inserted = text.to_string(),
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

fn write_terminal(world: &mut World, pane: Entity, text: &str) -> Result<(), String> {
    let store = world
        .get_resource::<TerminalStore>()
        .ok_or("no terminal store")?;
    let data = store
        .map
        .get(&pane)
        .ok_or("that terminal pane is gone — dictation stopped")?;
    // Paste, not Input: bracketed paste means a shell/TUI treats it as
    // inserted text rather than replaying it as keystrokes, so a transcript
    // with a newline in it can't auto-run a command.
    data.worker.send(WorkerMsg::Paste(text.to_string()));
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
                    if secs as f32 > MAX_LIVE_SECS {
                        "long clip — preview paused, text lands on release".to_string()
                    } else {
                        format!("release ⌘⇧M to finish{}", target_hint(d))
                    },
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
        Some(Target::Terminal(_)) => " · terminal (pastes on release)".into(),
        None => String::new(),
    }
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
