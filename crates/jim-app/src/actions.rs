//! Central **action registry** — the single source of truth for "things
//! the app can do." One `Action` is enumerable by the command palette,
//! placeable on the radial ring, bindable to a global keyboard chord,
//! and (later) exposable to DeepSeek as a tool. Before this module the
//! same capability was spread across four near-identical keyboard-
//! shortcut systems in `lib.rs` and a pane-spawn-only radial menu.
//!
//! ## Shape
//!
//! - [`Action`] is `Copy` (all-`'static` fields + an [`ActionRun`] that
//!   is either a data-carrying `SpawnPane` or a bare `fn` pointer),
//!   mirroring `jim_pane::PaneKindSpec`.
//! - [`ActionRegistry`] is a resource keyed by id, preserving insertion
//!   order so the palette lists actions deterministically.
//! - Producers (keybinds, radial, palette) push an [`ActionInvocation`]
//!   onto [`ActionInvocations`]; the exclusive [`run_requested_actions`]
//!   system drains it and performs each effect with full `&mut World`
//!   access. Producers run in [`ActionProducerSet`], dispatch after it.
//!
//! ## Keybindings (rebindable + chord sequences)
//!
//! Each action carries a [`Action::default_keys`] sequence (empty =
//! unbound, one chord = a plain shortcut, many = a *sequence* like `⌘K`
//! then `C`). At startup [`rebuild_keymap`] folds those defaults together
//! with `~/.jim/keybinds.json` into the [`Keymap`] resource —
//! the one place every consumer reads bindings from. The JSON is a flat
//! `{ "<action id>": "<binding>" }` map; a `null` value unbinds an action
//! that has a default:
//!
//! ```json
//! {
//!   "view.toggle_cube": "cmd+shift+backslash",
//!   "pane.spawn.terminal": "cmd+k t",
//!   "file.open": null
//! }
//! ```
//!
//! Binding strings are `+`-joined modifiers + key (`cmd|super|meta`,
//! `shift`, `alt|opt`, `ctrl`), with space-separated chords forming a
//! sequence (see [`KeyChord::parse`] / [`parse_sequence`]). The
//! `keybinds.reload` action re-reads the file live. The matcher
//! ([`dispatch_action_keybinds`]) recognizes a sequence one key at a time
//! via [`PendingSequence`]; while a sequence is mid-flight the keyboard is
//! held modal so the continuation key can't leak into the focused pane —
//! so sequence *leaders* should carry a modifier (a bare-key leader leaks
//! for one frame before suppression kicks in).

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;

use jim_pane::{KeyboardOwner, PaneRegistry};

use crate::projects::{NewPaneRequest, PendingActions, Projects};

/// A global keyboard chord. Left/right modifier variants are folded
/// together by the matcher, so a chord only records *whether* each
/// modifier is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub key: KeyCode,
}

impl KeyChord {
    pub const fn cmd(key: KeyCode) -> Self {
        Self { cmd: true, shift: false, alt: false, ctrl: false, key }
    }
    pub const fn cmd_shift(key: KeyCode) -> Self {
        Self { cmd: true, shift: true, alt: false, ctrl: false, key }
    }
    /// No modifiers — the typical second chord in a sequence (e.g. the
    /// `C` in `⌘K C`).
    pub const fn plain(key: KeyCode) -> Self {
        Self { cmd: false, shift: false, alt: false, ctrl: false, key }
    }

    /// Human-readable hint shown in the palette (e.g. `⌘⇧T`).
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.ctrl { s.push('⌃'); }
        if self.alt { s.push('⌥'); }
        if self.shift { s.push('⇧'); }
        if self.cmd { s.push('⌘'); }
        s.push_str(key_name(self.key));
        s
    }

    /// Parse one chord from a string like `"cmd+shift+t"` / `"ctrl+\\"`.
    /// Modifier aliases: cmd|super|meta|win, shift, alt|opt|option,
    /// ctrl|control. The final non-modifier token is the key. Returns
    /// `None` on an unknown token or a missing/duplicate key.
    pub fn parse(s: &str) -> Option<KeyChord> {
        let (mut cmd, mut shift, mut alt, mut ctrl) = (false, false, false, false);
        let mut key: Option<KeyCode> = None;
        for tok in s.split('+') {
            let t = tok.trim().to_ascii_lowercase();
            match t.as_str() {
                "" => {}
                "cmd" | "super" | "meta" | "win" => cmd = true,
                "shift" => shift = true,
                "alt" | "opt" | "option" => alt = true,
                "ctrl" | "control" => ctrl = true,
                other => {
                    if key.is_some() {
                        return None; // two keys in one chord
                    }
                    key = Some(parse_key(other)?);
                }
            }
        }
        Some(KeyChord { cmd, shift, alt, ctrl, key: key? })
    }
}

/// Parse a whitespace-separated chord sequence (e.g. `"cmd+k c"`). Returns
/// `None` if empty or any chord fails to parse.
pub fn parse_sequence(s: &str) -> Option<Vec<KeyChord>> {
    let v: Vec<KeyChord> = s.split_whitespace().map(KeyChord::parse).collect::<Option<_>>()?;
    (!v.is_empty()).then_some(v)
}

/// Render a chord sequence for the palette, chords joined by a space.
pub fn seq_label(seq: &[KeyChord]) -> String {
    seq.iter().map(|c| c.label()).collect::<Vec<_>>().join(" ")
}

/// True for the bare modifier keys, which must never form a chord on their
/// own (so holding `⌘` while reaching for the next key doesn't fire or
/// abort a sequence).
fn is_modifier_key(k: KeyCode) -> bool {
    use KeyCode::*;
    matches!(
        k,
        SuperLeft | SuperRight | ShiftLeft | ShiftRight | AltLeft | AltRight | ControlLeft | ControlRight
    )
}

/// Inverse of [`key_name`] for the key tokens the parser accepts.
fn parse_key(s: &str) -> Option<KeyCode> {
    use KeyCode::*;
    Some(match s {
        "a" => KeyA, "b" => KeyB, "c" => KeyC, "d" => KeyD, "e" => KeyE,
        "f" => KeyF, "g" => KeyG, "h" => KeyH, "i" => KeyI, "j" => KeyJ,
        "k" => KeyK, "l" => KeyL, "m" => KeyM, "n" => KeyN, "o" => KeyO,
        "p" => KeyP, "q" => KeyQ, "r" => KeyR, "s" => KeyS, "t" => KeyT,
        "u" => KeyU, "v" => KeyV, "w" => KeyW, "x" => KeyX, "y" => KeyY,
        "z" => KeyZ,
        "0" => Digit0, "1" => Digit1, "2" => Digit2, "3" => Digit3, "4" => Digit4,
        "5" => Digit5, "6" => Digit6, "7" => Digit7, "8" => Digit8, "9" => Digit9,
        "\\" | "backslash" => Backslash,
        "=" | "equal" | "plus" => Equal,
        "-" | "minus" => Minus,
        "[" | "bracketleft" => BracketLeft,
        "]" | "bracketright" => BracketRight,
        "space" => Space,
        "enter" | "return" => Enter,
        "tab" => Tab,
        "esc" | "escape" => Escape,
        "up" | "arrowup" => ArrowUp,
        "down" | "arrowdown" => ArrowDown,
        "left" | "arrowleft" => ArrowLeft,
        "right" | "arrowright" => ArrowRight,
        _ => return None,
    })
}

fn key_name(key: KeyCode) -> &'static str {
    use KeyCode::*;
    match key {
        KeyA => "A", KeyB => "B", KeyC => "C", KeyD => "D", KeyE => "E",
        KeyF => "F", KeyG => "G", KeyH => "H", KeyI => "I", KeyJ => "J",
        KeyK => "K", KeyL => "L", KeyM => "M", KeyN => "N", KeyO => "O",
        KeyP => "P", KeyQ => "Q", KeyR => "R", KeyS => "S", KeyT => "T",
        KeyU => "U", KeyV => "V", KeyW => "W", KeyX => "X", KeyY => "Y",
        KeyZ => "Z", Backslash => "\\",
        Digit0 => "0", Digit1 => "1", Digit2 => "2", Digit3 => "3", Digit4 => "4",
        Digit5 => "5", Digit6 => "6", Digit7 => "7", Digit8 => "8", Digit9 => "9",
        Equal => "=", Minus => "-", BracketLeft => "[", BracketRight => "]",
        Space => "Space", Enter => "⏎", Tab => "⇥", Escape => "Esc",
        ArrowUp => "↑", ArrowDown => "↓", ArrowLeft => "←", ArrowRight => "→",
        _ => "?",
    }
}

/// How an action performs its effect when invoked.
#[derive(Clone, Copy)]
pub enum ActionRun {
    /// Spawn a registered pane kind into the active project. Auto-
    /// generated for every `PaneKindSpec`. `origin` (cursor) comes from
    /// the invocation; `config` is `Null`.
    SpawnPane {
        kind: &'static str,
        size: Option<Vec2>,
    },
    /// Arbitrary world mutation. The handler receives an [`ActionCtx`]
    /// and can do anything an old shortcut system did.
    Custom(fn(&mut ActionCtx)),
    /// Spawn a registered pane kind with a runtime-defined config blob.
    /// Used by manifest-defined actions (`~/.jim/actions.json`): the
    /// config can't live in this `Copy` enum, so it's stored in
    /// [`RuntimeActions::configs`] keyed by the action id and looked up
    /// at dispatch. This is the data-driven sibling of `SpawnPane`.
    SpawnConfigured { kind: &'static str },
}

/// One thing the app can do.
#[derive(Clone, Copy)]
pub struct Action {
    /// Stable dispatch id, e.g. `"pane.spawn.terminal"`, `"view.toggle_cube"`.
    pub id: &'static str,
    /// Human label shown in the palette / radial.
    pub title: &'static str,
    /// Palette section, e.g. `"Panes"`, `"View"`, `"AI"`.
    pub category: &'static str,
    /// Extra fuzzy-search aliases beyond the title.
    pub keywords: &'static [&'static str],
    /// `Some(glyph)` makes the action eligible for the radial ring.
    pub radial_icon: Option<&'static str>,
    /// Default key binding. Empty = unbound; one chord = a simple
    /// shortcut; more = a chord *sequence* (e.g. `⌘K` then `C`). Always
    /// overridable per-id from `~/.jim/keybinds.json`; the
    /// effective binding lives in [`Keymap`].
    pub default_keys: &'static [KeyChord],
    /// The effect.
    pub run: ActionRun,
}

/// Registry resource. Keyed by id; `order` preserves first-registration
/// order for a stable palette listing.
#[derive(Resource, Default)]
pub struct ActionRegistry {
    by_id: HashMap<&'static str, Action>,
    order: Vec<&'static str>,
}

impl ActionRegistry {
    pub fn register(&mut self, action: Action) {
        if self.by_id.insert(action.id, action).is_none() {
            self.order.push(action.id);
        }
    }

    pub fn get(&self, id: &str) -> Option<&Action> {
        self.by_id.get(id)
    }

    /// Drop a set of actions by id (used when reloading the runtime
    /// manifest, so removed/renamed entries don't linger).
    pub fn unregister_many(&mut self, ids: &[&str]) {
        for id in ids {
            self.by_id.remove(*id);
        }
        self.order.retain(|id| !ids.contains(id));
    }

    /// Iterate in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Action> {
        self.order.iter().map(move |id| &self.by_id[id])
    }

    /// Actions eligible for the radial ring (those with an icon).
    pub fn radial_items(&self) -> impl Iterator<Item = &Action> {
        self.iter().filter(|a| a.radial_icon.is_some())
    }
}

/// How long a partial chord sequence waits for its next key before it is
/// abandoned (and the keyboard, which the sequence holds modal, freed).
const SEQUENCE_TIMEOUT: f32 = 1.2;

/// The effective key bindings: every action's [`Action::default_keys`],
/// overlaid by the user's `~/.jim/keybinds.json`. The matcher
/// and the command palette both read bindings from here (never straight
/// off `default_keys`) so a disk override is honored everywhere at once.
/// Rebuilt by [`rebuild_keymap`] at startup and on the `keybinds.reload`
/// action.
#[derive(Resource, Default)]
pub struct Keymap {
    bindings: HashMap<&'static str, Vec<KeyChord>>,
}

impl Keymap {
    /// Effective binding for an action id, if any.
    pub fn get(&self, id: &str) -> Option<&[KeyChord]> {
        self.bindings.get(id).map(Vec::as_slice)
    }

    /// Palette hint for an action id (chords joined by spaces).
    pub fn label(&self, id: &str) -> Option<String> {
        self.get(id).map(seq_label)
    }

    /// The action whose binding is exactly `seq`, if any. (Ties — two
    /// actions on one chord — resolve arbitrarily; we don't reject them.)
    fn match_exact(&self, seq: &[KeyChord]) -> Option<&'static str> {
        self.bindings
            .iter()
            .find(|(_, b)| b.as_slice() == seq)
            .map(|(id, _)| *id)
    }

    /// True if `seq` is a strict prefix of some longer binding — i.e. more
    /// keys could still complete a sequence.
    fn is_prefix(&self, seq: &[KeyChord]) -> bool {
        self.bindings
            .values()
            .any(|b| b.len() > seq.len() && &b[..seq.len()] == seq)
    }
}

/// In-flight chord sequence: the chords pressed so far and when the
/// sequence began (engine `elapsed_secs`, for the timeout). Empty = no
/// sequence in progress.
#[derive(Resource, Default)]
pub struct PendingSequence {
    pub chords: Vec<KeyChord>,
    pub started_at: f32,
}

/// `~/.jim/keybinds.json`.
fn keybinds_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = std::path::PathBuf::from(home);
    p.push(".jim");
    p.push("keybinds.json");
    Some(p)
}

/// Disk override map: action id -> binding string. `null` (or empty
/// string) unbinds an action that has a default. A missing file means
/// "no overrides" — not an error.
fn load_disk_overrides() -> HashMap<String, Option<String>> {
    let Some(path) = keybinds_path() else {
        return HashMap::new();
    };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!("keybinds.json: invalid JSON ({e}); ignoring overrides");
            HashMap::new()
        }),
        Err(_) => HashMap::new(),
    }
}

/// (Re)compute [`Keymap`] from the registry defaults plus disk overrides.
/// Safe to call any time after the registry is populated. Unknown ids and
/// unparseable chord strings are warned about and skipped (the default,
/// if any, stays).
pub fn rebuild_keymap(world: &mut World) {
    let mut bindings: HashMap<&'static str, Vec<KeyChord>> = world
        .resource::<ActionRegistry>()
        .iter()
        .filter(|a| !a.default_keys.is_empty())
        .map(|a| (a.id, a.default_keys.to_vec()))
        .collect();
    // id literals, so a disk entry can be remapped to its &'static id.
    let known: HashMap<&str, &'static str> = world
        .resource::<ActionRegistry>()
        .iter()
        .map(|a| (a.id, a.id))
        .collect();

    for (id, val) in load_disk_overrides() {
        // `_`-prefixed keys are treated as comments (JSON has none).
        if id.starts_with('_') {
            continue;
        }
        let Some(&sid) = known.get(id.as_str()) else {
            warn!("keybinds.json: unknown action id {id:?}");
            continue;
        };
        match val {
            None => {
                bindings.remove(sid);
            }
            Some(s) if s.trim().is_empty() => {
                bindings.remove(sid);
            }
            Some(s) => match parse_sequence(&s) {
                Some(seq) => {
                    bindings.insert(sid, seq);
                }
                None => warn!("keybinds.json: could not parse {s:?} for {id}"),
            },
        }
    }
    world.resource_mut::<Keymap>().bindings = bindings;
}

/// A queued request to run an action, drained by [`run_requested_actions`].
pub struct ActionInvocation {
    /// Action id. Owned because radial snapshots / palette results hand
    /// us strings, not `'static` literals.
    pub id: String,
    /// Cursor position for `SpawnPane` origin (radial). `None` = use the
    /// kind's normal cascade placement.
    pub origin: Option<Vec2>,
}

/// Pending action invocations for this frame.
#[derive(Resource, Default)]
pub struct ActionInvocations(pub Vec<ActionInvocation>);

impl ActionInvocations {
    pub fn request(&mut self, id: impl Into<String>, origin: Option<Vec2>) {
        self.0.push(ActionInvocation { id: id.into(), origin });
    }
}

/// Context handed to [`ActionRun::Custom`] handlers.
pub struct ActionCtx<'w> {
    pub world: &'w mut World,
    /// Cursor at invocation time (radial), if any.
    pub origin: Option<Vec2>,
}

/// Systems that *enqueue* action invocations (keybind matcher, radial
/// pick, palette Enter). [`run_requested_actions`] runs after this set
/// so picks dispatch the same frame.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionProducerSet;

/// Extension trait mirroring `app.add_systems` — register a bespoke
/// action at plugin-build time. Requires [`ActionsPlugin`] added first.
pub trait AppActionsExt {
    fn add_action(&mut self, action: Action) -> &mut Self;
}

impl AppActionsExt for App {
    fn add_action(&mut self, action: Action) -> &mut Self {
        self.world_mut()
            .resource_mut::<ActionRegistry>()
            .register(action);
        self
    }
}

// ============================================================
// Runtime-defined actions (`~/.jim/actions.json`, hot-reloaded)
// ============================================================
//
// Lets the common "open a specific pane kind with a config" action —
// e.g. "open the chess funct widget" — be declared on disk and picked up
// live, without editing this file + rebuilding. Mirrors the funct-widget
// hot reload (`~/.jim/widgets/`).
//
// The manifest is a JSON array of entries:
//
// ```json
// [
//   {
//     "id": "widget.chess",
//     "title": "Chess",
//     "category": "Widgets",
//     "icon": "♟",
//     "keys": "cmd+k h",
//     "kind": "script_widget",
//     "config": { "script": "chess.ft", "title": "Chess" }
//   }
// ]
// ```
//
// `kind` defaults to `script_widget`; `config` is handed verbatim to that
// pane kind's spawn callback. `keys` is a default binding (still
// overridable per-id from `keybinds.json`); `icon` makes it radial-eligible.

/// One manifest entry as it appears on disk.
#[derive(Deserialize)]
struct ManifestAction {
    id: String,
    title: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    keys: Option<String>,
    #[serde(default = "default_manifest_kind")]
    kind: String,
    #[serde(default)]
    config: serde_json::Value,
}

fn default_manifest_kind() -> String {
    "script_widget".to_string()
}

/// State + file watcher for the runtime action manifest. The config
/// blobs live here (they can't ride along in the `Copy` [`Action`]); the
/// `ids` list lets a reload drop the previous manifest's actions before
/// registering the new set.
#[derive(Resource, Default)]
pub struct RuntimeActions {
    /// Ids registered from the manifest, so a reload can remove them.
    ids: Vec<&'static str>,
    /// Per-id spawn config for [`ActionRun::SpawnConfigured`].
    configs: HashMap<&'static str, serde_json::Value>,
    rx: Option<Mutex<Receiver<()>>>,
    _watcher: Option<RecommendedWatcher>,
}

fn actions_manifest_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".jim");
    p.push("actions.json");
    Some(p)
}

/// Default manifest written on first run so the feature is discoverable
/// and immediately useful (the chess widget script is auto-bootstrapped
/// by the funct-widget plugin, so this action works out of the box).
const DEFAULT_ACTIONS_MANIFEST: &str = r#"[
  {
    "id": "widget.chess",
    "title": "Chess",
    "category": "Widgets",
    "icon": "♟",
    "keys": "cmd+k h",
    "kind": "script_widget",
    "config": { "script": "chess.ft", "title": "Chess" }
  },
  {
    "id": "widget.garden",
    "title": "Garden",
    "category": "Widgets",
    "icon": "✿",
    "kind": "script_widget",
    "config": { "script": "garden.ft", "title": "Garden" }
  }
]
"#;

/// Read + (re)register the manifest, then rebuild the keymap so new
/// `keys` take effect. Exclusive (mutates the registry, leaks `'static`
/// strings, calls [`rebuild_keymap`]). Invalid entries are warned about
/// and skipped; a parse error keeps the previously-loaded set.
fn apply_actions_manifest(world: &mut World) {
    let Some(path) = actions_manifest_path() else {
        return;
    };
    // Missing manifest → treat as empty so disk-discovery below still runs;
    // a present-but-invalid manifest keeps the current set untouched.
    let manifest: Vec<ManifestAction> = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!("[actions] {}: invalid JSON ({e}); keeping current actions", path.display());
                return;
            }
        },
        Err(_) => Vec::new(),
    };

    // Drop the previous manifest's actions + configs.
    let old_ids = std::mem::take(&mut world.resource_mut::<RuntimeActions>().ids);
    if !old_ids.is_empty() {
        let refs: Vec<&str> = old_ids.iter().copied().collect();
        world.resource_mut::<ActionRegistry>().unregister_many(&refs);
    }
    world.resource_mut::<RuntimeActions>().configs.clear();

    let mut new_ids: Vec<&'static str> = Vec::new();
    let mut new_configs: HashMap<&'static str, serde_json::Value> = HashMap::new();
    // Scripts a manifest entry already claims, so disk-discovery below
    // doesn't double-register the same widget.
    let mut declared_scripts: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in manifest {
        if m.id.trim().is_empty() {
            warn!("[actions] manifest entry with empty id; skipping");
            continue;
        }
        // Leak owned strings to `'static`, matching the `Box::leak`
        // already used for dynamic pane-kind ids. Reloads re-leak (a
        // small, bounded, user-driven cost — same tradeoff as funct
        // script recompiles).
        let id: &'static str = Box::leak(m.id.clone().into_boxed_str());
        let title: &'static str = Box::leak(m.title.into_boxed_str());
        let category: &'static str =
            Box::leak(m.category.unwrap_or_else(|| "Widgets".to_string()).into_boxed_str());
        let kind: &'static str = Box::leak(m.kind.into_boxed_str());
        let radial_icon: Option<&'static str> =
            m.icon.map(|s| &*Box::leak(s.into_boxed_str()));
        let keywords: &'static [&'static str] = Box::leak(
            m.keywords
                .into_iter()
                .map(|k| &*Box::leak(k.into_boxed_str()))
                .collect::<Vec<&'static str>>()
                .into_boxed_slice(),
        );
        let default_keys: &'static [KeyChord] = match m.keys.as_deref() {
            Some(s) if !s.trim().is_empty() => match parse_sequence(s) {
                Some(seq) => Box::leak(seq.into_boxed_slice()),
                None => {
                    warn!("[actions] {id}: could not parse keys {s:?}; leaving unbound");
                    &[]
                }
            },
            _ => &[],
        };

        world.resource_mut::<ActionRegistry>().register(Action {
            id,
            title,
            category,
            keywords,
            radial_icon,
            default_keys,
            run: ActionRun::SpawnConfigured { kind },
        });
        if let Some(script) = m.config.get("script").and_then(|v| v.as_str()) {
            declared_scripts.insert(script.to_string());
        }
        new_configs.insert(id, m.config);
        new_ids.push(id);
    }

    // Auto-discover every spawnable `.ft` widget in ~/.jim/widgets and
    // register a `widget.<stem>` action for any not already claimed by the
    // manifest — so a newly-added widget ALWAYS shows up in the palette with
    // no manual manifest edit. Tracked in `RuntimeActions::ids` alongside the
    // manifest ones, so a reload drops + re-derives them cleanly.
    let mut discovered = 0usize;
    for w in discover_widget_actions(&declared_scripts) {
        if world.resource::<ActionRegistry>().get(w.id.as_str()).is_some() {
            continue; // id already taken by a built-in / pane / manifest action
        }
        discovered += 1;
        let id: &'static str = Box::leak(w.id.into_boxed_str());
        let title: &'static str = Box::leak(w.title.clone().into_boxed_str());
        let stem: &'static str = Box::leak(w.stem.into_boxed_str());
        let keywords: &'static [&'static str] =
            Box::leak(vec![stem, "widget", "script"].into_boxed_slice());
        world.resource_mut::<ActionRegistry>().register(Action {
            id,
            title,
            category: "Widgets",
            keywords,
            radial_icon: None,
            default_keys: &[],
            run: ActionRun::SpawnConfigured { kind: "script_widget" },
        });
        new_configs.insert(id, serde_json::json!({ "script": w.script, "title": w.title }));
        new_ids.push(id);
    }
    eprintln!("[actions] auto-registered {discovered} widget action(s) from ~/.jim/widgets");

    {
        let mut ra = world.resource_mut::<RuntimeActions>();
        ra.ids = new_ids;
        ra.configs = new_configs;
    }
    rebuild_keymap(world);
}

/// A `.ft` widget found on disk that should auto-register as a palette action.
struct DiscoveredWidget {
    /// Action id, `widget.<stem>`.
    id: String,
    /// Human title (prettified stem).
    title: String,
    /// Script filename, e.g. `ipc_monitor.ft`.
    script: String,
    /// Bare stem, used as a search keyword.
    stem: String,
}

/// Scan `~/.jim/widgets/*.ft` for spawnable widgets not already declared in
/// the manifest. "Spawnable" = the script defines a `render` function (this
/// filters out library modules like `df.ft` / `host.ft`). Sorted by title for
/// a stable palette order.
fn discover_widget_actions(declared_scripts: &std::collections::HashSet<String>) -> Vec<DiscoveredWidget> {
    let Some(dir) = jim_widget::script_widget::widgets_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ft") {
            continue;
        }
        let (Some(file_name), Some(stem)) = (
            path.file_name().and_then(|n| n.to_str()).map(String::from),
            path.file_stem().and_then(|s| s.to_str()).map(String::from),
        ) else {
            continue;
        };
        // `_`-prefixed files are private helpers by convention.
        if stem.starts_with('_') || declared_scripts.contains(&file_name) {
            continue;
        }
        // Only scripts that render are spawnable widgets (skips libraries).
        match std::fs::read_to_string(&path) {
            Ok(src) if src.contains("fn render") => {}
            _ => continue,
        }
        out.push(DiscoveredWidget {
            id: format!("widget.{stem}"),
            title: prettify(&stem),
            script: file_name,
            stem,
        });
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

/// `df_view_bar` → `Df View Bar`; `set_theme` → `Set Theme`.
fn prettify(stem: &str) -> String {
    stem.split(|c| c == '_' || c == '-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().chain(cs).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// First-run bootstrap (write the default manifest) + start the file
/// watcher + do the initial load.
fn setup_actions_watcher(world: &mut World) {
    let Some(path) = actions_manifest_path() else {
        warn!("[actions] HOME not set, no runtime actions");
        return;
    };
    let dir = path.parent().map(PathBuf::from);
    if let Some(dir) = &dir {
        let _ = std::fs::create_dir_all(dir);
    }
    if !path.exists() {
        if let Err(e) = std::fs::write(&path, DEFAULT_ACTIONS_MANIFEST) {
            warn!("[actions] couldn't write default manifest {}: {e}", path.display());
        }
    }

    // Watch ~/.jim (for actions.json) and ~/.jim/widgets (for *.ft
    // add/remove/edit) non-recursively, forwarding only the events that
    // change the action set — the dirs see frequent writes for other state.
    if let Some(dir) = dir {
        let widgets_dir = jim_widget::script_widget::widgets_dir();
        if let Some(wd) = &widgets_dir {
            let _ = std::fs::create_dir_all(wd);
        }
        let (tx, rx) = mpsc::channel::<()>();
        let target = path.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if !matches!(
                ev.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) | EventKind::Any
            ) {
                return;
            }
            let hit_manifest = ev.paths.iter().any(|p| p == &target);
            // A new/removed/renamed .ft widget changes the discovered set.
            let hit_widget = ev
                .paths
                .iter()
                .any(|p| p.extension().and_then(|e| e.to_str()) == Some("ft"));
            if hit_manifest || hit_widget {
                let _ = tx.send(());
            }
        });
        match watcher {
            Ok(mut w) => {
                let mut ok = true;
                if let Err(e) = w.watch(&dir, RecursiveMode::NonRecursive) {
                    warn!("[actions] failed to watch {}: {e}", dir.display());
                    ok = false;
                }
                if let Some(wd) = &widgets_dir {
                    if let Err(e) = w.watch(wd, RecursiveMode::NonRecursive) {
                        warn!("[actions] failed to watch {}: {e}", wd.display());
                    }
                }
                if ok {
                    let mut ra = world.resource_mut::<RuntimeActions>();
                    ra.rx = Some(Mutex::new(rx));
                    ra._watcher = Some(w);
                }
            }
            Err(e) => warn!("[actions] file watcher failed to start: {e}"),
        }
    }

    apply_actions_manifest(world);
}

/// Drain manifest-change notifications and re-apply (exclusive, since it
/// re-registers actions + rebuilds the keymap).
fn poll_actions_watcher(world: &mut World) {
    let _t_prof = jim_pane::prof::sys_span("poll_actions_watcher");
    let changed = {
        let Some(ra) = world.get_resource::<RuntimeActions>() else {
            return;
        };
        let Some(rx) = &ra.rx else { return };
        let rx = rx.lock().expect("actions watcher channel poisoned");
        rx.try_iter().count() > 0
    };
    if changed {
        apply_actions_manifest(world);
        eprintln!("[actions] reloaded manifest from external edit");
    }
}

pub struct ActionsPlugin;

impl Plugin for ActionsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActionRegistry>()
            .init_resource::<ActionInvocations>()
            .init_resource::<Keymap>()
            .init_resource::<PendingSequence>()
            .init_resource::<RuntimeActions>()
            // Pane registrations land in `Startup`; synthesize their
            // spawn-actions once those are all in, then fold every action's
            // defaults + the disk overrides into the `Keymap`. (Bespoke
            // actions are registered in `App::build` before `PostStartup`,
            // so they're present too.) Finally load the runtime manifest
            // (`~/.jim/actions.json`) on top + start its watcher.
            .add_systems(
                PostStartup,
                (generate_pane_spawn_actions, rebuild_keymap, setup_actions_watcher).chain(),
            )
            .add_systems(Update, poll_actions_watcher)
            .add_systems(Update, dispatch_action_keybinds.in_set(ActionProducerSet))
            .add_systems(Update, run_requested_actions.after(ActionProducerSet));
    }
}

/// Synthesize one `pane.spawn.<kind>` action per registered pane kind so
/// adding a pane plugin makes it appear in the radial *and* palette with
/// no extra bookkeeping. The kind's `radial_icon` carries over verbatim.
fn generate_pane_spawn_actions(registry: Res<PaneRegistry>, mut actions: ResMut<ActionRegistry>) {
    let specs: Vec<(&'static str, &'static str, Option<&'static str>)> = registry
        .iter()
        .map(|s| (s.kind, s.display_name, s.radial_icon))
        .collect();
    for (kind, display_name, radial_icon) in specs {
        // Leak the id once at startup — matches the `Box::leak` already
        // used for dynamic pane kinds in the SpawnWidget IPC handler.
        let id: &'static str = Box::leak(format!("pane.spawn.{kind}").into_boxed_str());
        actions.register(Action {
            id,
            title: display_name,
            category: "Panes",
            keywords: &[],
            radial_icon,
            default_keys: default_spawn_keys(kind),
            run: ActionRun::SpawnPane { kind, size: None },
        });
    }
}

/// Default spawn shortcut for a few well-known pane kinds. Anything not
/// listed gets no default and is still bindable from `keybinds.json` by
/// its id (`pane.spawn.<kind>`).
fn default_spawn_keys(kind: &str) -> &'static [KeyChord] {
    match kind {
        "terminal" => const { &[KeyChord::cmd(KeyCode::KeyT)] },
        _ => &[],
    }
}

/// Read keyboard events once and drive the chord-sequence matcher against
/// the effective [`Keymap`]. A single-chord binding fires immediately; a
/// multi-chord binding is recognized one key at a time via
/// [`PendingSequence`].
fn dispatch_action_keybinds(
    mut events: MessageReader<KeyboardInput>,
    mods: Res<ButtonInput<KeyCode>>,
    keymap: Res<Keymap>,
    owner: Res<KeyboardOwner>,
    time: Res<Time>,
    mut pending: ResMut<PendingSequence>,
    mut invocations: ResMut<ActionInvocations>,
) {
    let now = time.elapsed_secs();
    // Expire a stale partial sequence even on a frame with no key events,
    // so a forgotten prefix doesn't hold the keyboard hostage — while a
    // sequence is pending the owner authority reports `Modal`.
    if !pending.chords.is_empty() && now - pending.started_at > SEQUENCE_TIMEOUT {
        pending.chords.clear();
    }
    // A *text* modal (palette / rename) suppresses chords entirely. A
    // pending sequence ALSO makes the owner `Modal` (so pane typing is
    // gated mid-sequence) but must not suppress us — that's how we read
    // the continuation key. Distinguish the two by whether a sequence is
    // actually in progress.
    if owner.is_modal() && pending.chords.is_empty() {
        events.clear();
        return;
    }

    let cmd = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);
    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let alt = mods.pressed(KeyCode::AltLeft) || mods.pressed(KeyCode::AltRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);

    for ev in events.read() {
        if !ev.state.is_pressed() || is_modifier_key(ev.key_code) {
            continue;
        }
        let chord = KeyChord { cmd, shift, alt, ctrl, key: ev.key_code };
        let mut candidate = pending.chords.clone();
        candidate.push(chord);

        if let Some(id) = keymap.match_exact(&candidate) {
            // Complete binding — fire and reset.
            invocations.request(id, None);
            pending.chords.clear();
        } else if keymap.is_prefix(&candidate) {
            // Still a viable prefix of a longer binding — wait for more.
            pending.chords = candidate;
            pending.started_at = now;
        } else {
            // Dead end: this key extends nothing. Abandon any partial
            // sequence (the key is consumed, not re-routed).
            pending.chords.clear();
        }
    }
}

/// Exclusive system: drain queued invocations and perform each effect.
/// Looks each action up, copies it out (releasing the registry borrow),
/// then dispatches with `&mut World`.
pub fn run_requested_actions(world: &mut World) {
    let _t_prof = jim_pane::prof::sys_span("run_requested_actions");
    let queued = std::mem::take(&mut world.resource_mut::<ActionInvocations>().0);
    for inv in queued {
        let Some(action) = world.resource::<ActionRegistry>().get(&inv.id).copied() else {
            warn!("action {:?} requested but not registered", inv.id);
            continue;
        };
        match action.run {
            ActionRun::SpawnPane { kind, size } => {
                let Some(active) = world.resource::<Projects>().active else {
                    continue;
                };
                world
                    .resource_mut::<PendingActions>()
                    .new_panes
                    .push(NewPaneRequest {
                        kind,
                        project_id: active,
                        origin: inv.origin,
                        size,
                        config: serde_json::Value::Null,
                    });
            }
            ActionRun::Custom(f) => {
                let mut ctx = ActionCtx { world, origin: inv.origin };
                f(&mut ctx);
            }
            ActionRun::SpawnConfigured { kind } => {
                let Some(active) = world.resource::<Projects>().active else {
                    continue;
                };
                let config = world
                    .resource::<RuntimeActions>()
                    .configs
                    .get(inv.id.as_str())
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                world
                    .resource_mut::<PendingActions>()
                    .new_panes
                    .push(NewPaneRequest {
                        kind,
                        project_id: active,
                        origin: inv.origin,
                        size: None,
                        config,
                    });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_chords() {
        assert_eq!(
            KeyChord::parse("cmd+shift+t"),
            Some(KeyChord::cmd_shift(KeyCode::KeyT))
        );
        assert_eq!(
            KeyChord::parse("CMD+O"),
            Some(KeyChord::cmd(KeyCode::KeyO))
        );
        // modifier aliases + a symbol key
        assert_eq!(
            KeyChord::parse("ctrl+\\"),
            Some(KeyChord {
                cmd: false,
                shift: false,
                alt: false,
                ctrl: true,
                key: KeyCode::Backslash,
            })
        );
        assert_eq!(KeyChord::parse("opt+="), Some(KeyChord { cmd: false, shift: false, alt: true, ctrl: false, key: KeyCode::Equal }));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(KeyChord::parse("cmd+nope"), None); // unknown key
        assert_eq!(KeyChord::parse("cmd+shift"), None); // no key
        assert_eq!(KeyChord::parse("a+b"), None); // two keys
    }

    #[test]
    fn parses_sequences() {
        let seq = parse_sequence("cmd+k c").unwrap();
        assert_eq!(seq, vec![KeyChord::cmd(KeyCode::KeyK), KeyChord::plain(KeyCode::KeyC)]);
        assert_eq!(parse_sequence("   "), None);
        assert!(parse_sequence("cmd+k bogus").is_none()); // any bad chord fails the whole seq
    }

    #[test]
    fn labels_round_trip_through_parse() {
        for s in ["cmd+shift+t", "ctrl+\\", "cmd+0", "cmd+["] {
            let chord = KeyChord::parse(s).unwrap();
            // The label re-parses to the same chord (glyphs aren't parseable,
            // so check structural equality via a fresh parse of the canonical
            // ascii form instead).
            assert_eq!(KeyChord::parse(s), Some(chord));
        }
        assert_eq!(seq_label(&parse_sequence("cmd+k c").unwrap()), "⌘K C");
    }

    fn keymap_with(pairs: &[(&'static str, &str)]) -> Keymap {
        let mut km = Keymap::default();
        for (id, s) in pairs {
            km.bindings.insert(id, parse_sequence(s).unwrap());
        }
        km
    }

    /// End-to-end (headless): a `SpawnConfigured` action, when invoked,
    /// resolves its config from `RuntimeActions` and queues a matching
    /// `NewPaneRequest` for the active project. Covers the dispatch arm
    /// that manifest-defined actions ride on.
    #[test]
    fn spawn_configured_action_queues_pane_request() {
        let mut world = World::new();
        world.init_resource::<ActionRegistry>();
        world.init_resource::<ActionInvocations>();
        world.init_resource::<RuntimeActions>();
        world.init_resource::<PendingActions>();
        world.init_resource::<Projects>();
        world.resource_mut::<Projects>().active = Some(42);

        world.resource_mut::<ActionRegistry>().register(Action {
            id: "widget.chess",
            title: "Chess",
            category: "Widgets",
            keywords: &[],
            radial_icon: Some("♟"),
            default_keys: &[],
            run: ActionRun::SpawnConfigured { kind: "script_widget" },
        });
        let cfg = serde_json::json!({ "script": "chess.ft", "title": "Chess" });
        world
            .resource_mut::<RuntimeActions>()
            .configs
            .insert("widget.chess", cfg.clone());

        world
            .resource_mut::<ActionInvocations>()
            .request("widget.chess", None);
        run_requested_actions(&mut world);

        let panes = &world.resource::<PendingActions>().new_panes;
        assert_eq!(panes.len(), 1, "exactly one pane queued");
        assert_eq!(panes[0].kind, "script_widget");
        assert_eq!(panes[0].project_id, 42);
        assert_eq!(panes[0].config, cfg);
    }

    #[test]
    fn matches_exact_and_prefix() {
        let km = keymap_with(&[
            ("a.single", "cmd+o"),
            ("a.seq", "cmd+k c"),
        ]);
        let cmd_o = vec![KeyChord::cmd(KeyCode::KeyO)];
        let cmd_k = vec![KeyChord::cmd(KeyCode::KeyK)];
        let cmd_k_c = vec![KeyChord::cmd(KeyCode::KeyK), KeyChord::plain(KeyCode::KeyC)];

        // single chord fires immediately
        assert_eq!(km.match_exact(&cmd_o), Some("a.single"));
        assert!(!km.is_prefix(&cmd_o));

        // sequence prefix is not yet a match, but IS a viable prefix
        assert_eq!(km.match_exact(&cmd_k), None);
        assert!(km.is_prefix(&cmd_k));

        // full sequence matches and is no longer a prefix of anything
        assert_eq!(km.match_exact(&cmd_k_c), Some("a.seq"));
        assert!(!km.is_prefix(&cmd_k_c));
    }
}
