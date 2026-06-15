//! Command palette — a VSCode/Sublime-style centered overlay that fuzzy-
//! searches the [`ActionRegistry`](crate::actions::ActionRegistry) and
//! runs the chosen action. Opened with **Cmd+Shift+P**; Esc / Enter close it.
//!
//! It also hosts the **"Ask DeepSeek"** entry, which launches the in-app
//! [`agent`](crate::agent): the query becomes the goal for an autonomous
//! ReAct loop that runs shell commands (driving the app through `jimctl`)
//! and observes their output, step by step. The loop runs on a worker
//! thread; its transcript (thoughts / commands / observations) streams into
//! the palette live, and Esc cancels it.
//!
//! ## Why native (not a widget pane)
//!
//! The palette is a *modal overlay*: it owns the keyboard, sits above
//! everything, stays centered. It reuses the widget **Element** vocabulary
//! for its look (inheriting theme tokens) but renders natively onto
//! [`MENU_OVERLAY_LAYER`].
//!
//! ## Render-layer correctness
//!
//! `jim_widget::render::render` spawns primitives with no `RenderLayers`,
//! and pane-bevy's propagation only stamps subtrees under a `PaneLayer`
//! ancestor — which a native overlay isn't. So [`render_palette`] is an
//! **exclusive system**: it spawns the content root, renders into the
//! world command buffer, flushes, then stamps
//! `RenderLayers::layer(MENU_OVERLAY_LAYER)` over the whole subtree in the
//! same run. No frame shows palette glyphs on the main camera.
//!
//! ## Input isolation
//!
//! While the palette is open, `compute_keyboard_owner` (terminal-bevy)
//! sets `jim_pane::KeyboardOwner::Modal`, which every keyboard consumer
//! respects — pane typing and global chords are suppressed centrally, so
//! the palette needs no per-handler focus juggling of its own.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use bevy::camera::visibility::RenderLayers;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

use jim_pane::{PaneFont, PaneFontMetrics, PaneRegistry};
use jim_widget::protocol::{Align, Border, Edges, Element, Shadow, Style, Weight};
use jim_widget::render::{self, LayoutCtx, WidgetPalette};
use jim_widget::WidgetTargets;

use crate::actions::{ActionInvocations, ActionRegistry, Keymap};
use crate::agent::{self, AgentMsg};
use crate::projects::Projects;
use crate::MENU_OVERLAY_LAYER;

/// Z within the overlay layer — above the drawer (550) and radial (600).
const PALETTE_Z: f32 = 700.0;
const PALETTE_W: f32 = 600.0;
const TOP_MARGIN: f32 = 96.0;
const MAX_ROWS: usize = 10;
/// Synthetic result-row id for the "Ask DeepSeek" entry.
const ASK_ID: &str = "__ask_deepseek__";

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    /// Fuzzy action search (default).
    #[default]
    Actions,
    /// The agent is running; the transcript streams in live.
    Busy,
    /// The agent finished; final answer + transcript shown until dismissed.
    Plan,
}

/// One line of the agent transcript shown in the palette.
struct TLine {
    icon: &'static str,
    text: String,
    color: &'static str,
}

/// One row in the filtered action list.
#[derive(Clone)]
pub struct PaletteRow {
    pub id: &'static str,
    pub title: String,
    pub category: &'static str,
    pub keybind: Option<String>,
}

/// External request (e.g. from IPC) to open the palette.
#[derive(Resource, Default)]
pub struct PaletteOpenRequest {
    pub requested: bool,
    pub seed: Option<String>,
    /// Immediately fire the DeepSeek "Ask" flow with the seeded query.
    pub ask: bool,
}

/// Channel back from the agent worker thread, plus a cancel flag the UI
/// flips when the palette closes mid-run. `Mutex` makes the `Receiver`
/// `Sync` so this stays an ordinary `Resource`.
#[derive(Resource, Default)]
pub struct AgentChannel {
    rx: Mutex<Option<Receiver<AgentMsg>>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
}

/// Per-action pick counts, persisted to disk. Used to bias ranking so a
/// frequently-chosen action floats above equal/near-equal fuzzy matches
/// (and an empty query lists your most-used first).
#[derive(Resource, Default)]
pub struct PaletteUsage {
    counts: HashMap<String, u32>,
}

/// Each pick is worth this many score points, capped so usage biases
/// ties / small gaps without overriding a clearly-better fuzzy match.
const USAGE_WEIGHT: i32 = 2;
const USAGE_CAP: u32 = 8;

impl PaletteUsage {
    fn load() -> Self {
        let Some(path) = usage_path() else {
            return Self::default();
        };
        match std::fs::read(&path) {
            Ok(bytes) => Self {
                counts: serde_json::from_slice(&bytes).unwrap_or_default(),
            },
            Err(_) => Self::default(),
        }
    }

    fn count(&self, id: &str) -> u32 {
        self.counts.get(id).copied().unwrap_or(0)
    }

    /// Score contribution for `id` — capped so habits win ties but not
    /// strong matches.
    fn bonus(&self, id: &str) -> i32 {
        self.count(id).min(USAGE_CAP) as i32 * USAGE_WEIGHT
    }

    /// Record a pick and persist immediately (the file is tiny).
    fn bump(&mut self, id: &str) {
        *self.counts.entry(id.to_string()).or_insert(0) += 1;
        if let Some(path) = usage_path() {
            if let Ok(bytes) = serde_json::to_vec_pretty(&self.counts) {
                let _ = std::fs::write(path, bytes);
            }
        }
    }
}

fn usage_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".jim");
    p.push("palette_usage.json");
    Some(p)
}

#[derive(Resource, Default)]
pub struct CommandPalette {
    pub open: bool,
    pub mode: PaletteMode,
    pub query: String,
    pub results: Vec<PaletteRow>,
    pub selected: usize,
    /// Header line for Busy / Plan modes.
    message: String,
    /// Live agent transcript (Busy + Plan modes).
    transcript: Vec<TLine>,
    root: Option<Entity>,
    last_sig: u64,
}

impl CommandPalette {
    /// Hash of the visible state — [`render_palette`] only rebuilds when
    /// this changes.
    fn signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.open.hash(&mut h);
        (self.mode as u8).hash(&mut h);
        self.query.hash(&mut h);
        self.selected.hash(&mut h);
        self.message.hash(&mut h);
        self.results.len().hash(&mut h);
        self.transcript.len().hash(&mut h);
        if let Some(last) = self.transcript.last() {
            last.text.hash(&mut h);
        }
        h.finish()
    }
}

pub struct CommandPalettePlugin;

impl Plugin for CommandPalettePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CommandPalette>()
            .init_resource::<PaletteOpenRequest>()
            .init_resource::<AgentChannel>()
            .insert_resource(PaletteUsage::load())
            .add_systems(Update, (palette_input, poll_agent).chain())
            .add_systems(Update, render_palette.after(poll_agent));
    }
}

// ---------- Fuzzy matching ----------

/// Subsequence fuzzy scorer; rewards consecutive runs and word-start hits.
fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    let n: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();
    if n.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = haystack.chars().collect();
    let mut hi = 0usize;
    let mut score = 0i32;
    let mut consec = 0i32;
    for &nc in &n {
        let mut found = false;
        while hi < h.len() {
            let hc = h[hi].to_ascii_lowercase();
            let at = hi;
            hi += 1;
            if hc == nc {
                score += 1 + consec;
                if at == 0 || matches!(h[at - 1], ' ' | '_' | '.' | '/' | '-') {
                    score += 4;
                }
                consec += 1;
                found = true;
                break;
            } else {
                consec = 0;
            }
        }
        if !found {
            return None;
        }
    }
    Some(score)
}

/// Filter + rank the registry, then append the synthetic "Ask DeepSeek"
/// row when the query is non-empty. Ranking blends the fuzzy score with a
/// capped usage bonus so frequently-picked actions float up (and break
/// ties against alphabetical/registration order).
fn refresh_results(
    palette: &mut CommandPalette,
    registry: &ActionRegistry,
    usage: &PaletteUsage,
    keymap: &Keymap,
) {
    let q = palette.query.trim();
    let mut scored: Vec<(i32, PaletteRow)> = registry
        .iter()
        .filter_map(|a| {
            let hay = if a.keywords.is_empty() {
                a.title.to_string()
            } else {
                format!("{} {}", a.title, a.keywords.join(" "))
            };
            let s = fuzzy_score(q, &hay)? + usage.bonus(a.id);
            Some((
                s,
                PaletteRow {
                    id: a.id,
                    title: a.title.to_string(),
                    category: a.category,
                    keybind: keymap.label(a.id),
                },
            ))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut rows: Vec<PaletteRow> = scored.into_iter().map(|(_, r)| r).collect();
    if !q.is_empty() {
        rows.push(PaletteRow {
            id: ASK_ID,
            title: format!("Ask DeepSeek: {}", q),
            category: "AI",
            keybind: None,
        });
    }
    palette.results = rows;
    if palette.selected >= palette.results.len() {
        palette.selected = 0;
    }
}

// ---------- Input ----------

#[allow(clippy::too_many_arguments)]
fn palette_input(
    mut keys: MessageReader<KeyboardInput>,
    mods: Res<ButtonInput<KeyCode>>,
    registry: Res<ActionRegistry>,
    pane_registry: Res<PaneRegistry>,
    projects: Res<Projects>,
    mut palette: ResMut<CommandPalette>,
    mut invocations: ResMut<ActionInvocations>,
    mut agent_ch: ResMut<AgentChannel>,
    mut usage: ResMut<PaletteUsage>,
    mut open_req: ResMut<PaletteOpenRequest>,
    keymap: Res<Keymap>,
) {
    // External (IPC) open request.
    if open_req.requested {
        open_req.requested = false;
        if !palette.open {
            open(&mut palette, &registry, &usage, &keymap);
        }
        if let Some(seed) = open_req.seed.take() {
            palette.query = seed;
            refresh_results(&mut palette, &registry, &usage, &keymap);
        }
        if std::mem::take(&mut open_req.ask) && !palette.query.trim().is_empty() {
            start_agent(&mut palette, &projects, &pane_registry, &mut agent_ch);
        }
    }

    let cmd = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);
    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);

    for ev in keys.read() {
        if !ev.state.is_pressed() {
            continue;
        }

        // Cmd+Shift+P toggles, open or closed (VSCode/Sublime palette key).
        if cmd && shift && ev.key_code == KeyCode::KeyP {
            if palette.open {
                close(&mut palette);
            } else {
                open(&mut palette, &registry, &usage, &keymap);
            }
            continue;
        }

        if !palette.open {
            continue;
        }

        match &ev.logical_key {
            Key::Escape => close_and_cancel(&mut palette, &mut agent_ch),
            Key::Enter => match palette.mode {
                PaletteMode::Actions => {
                    if let Some(row) = palette.results.get(palette.selected).cloned() {
                        if row.id == ASK_ID {
                            start_agent(&mut palette, &projects, &pane_registry, &mut agent_ch);
                        } else {
                            usage.bump(row.id);
                            invocations.request(row.id, None);
                            close(&mut palette);
                        }
                    }
                }
                // Agent finished — Enter just dismisses.
                PaletteMode::Plan => close(&mut palette),
                // Running — Enter is inert; Esc cancels.
                PaletteMode::Busy => {}
            },
            Key::ArrowDown if palette.mode == PaletteMode::Actions => {
                if !palette.results.is_empty() {
                    palette.selected = (palette.selected + 1).min(palette.results.len() - 1);
                }
            }
            Key::ArrowUp if palette.mode == PaletteMode::Actions => {
                palette.selected = palette.selected.saturating_sub(1);
            }
            Key::Backspace if palette.mode == PaletteMode::Actions => {
                palette.query.pop();
                refresh_results(&mut palette, &registry, &usage, &keymap);
            }
            Key::Space if palette.mode == PaletteMode::Actions => {
                palette.query.push(' ');
                refresh_results(&mut palette, &registry, &usage, &keymap);
            }
            Key::Character(s) if palette.mode == PaletteMode::Actions && !cmd && !ctrl => {
                palette.query.push_str(s.as_str());
                refresh_results(&mut palette, &registry, &usage, &keymap);
            }
            _ => {}
        }
    }
}

fn open(palette: &mut CommandPalette, registry: &ActionRegistry, usage: &PaletteUsage, keymap: &Keymap) {
    palette.open = true;
    palette.mode = PaletteMode::Actions;
    palette.query.clear();
    palette.message.clear();
    palette.transcript.clear();
    palette.selected = 0;
    refresh_results(palette, registry, usage, keymap);
    // No focus juggling: while `palette.open`, `compute_keyboard_owner`
    // sets `KeyboardOwner::Modal`, which centrally suppresses pane typing
    // and global chords. Focus returns to its pane automatically on close.
}

fn close(palette: &mut CommandPalette) {
    palette.open = false;
    palette.mode = PaletteMode::Actions;
    palette.transcript.clear();
}

/// Close the palette and signal any in-flight agent run to stop. The worker
/// checks the flag between steps; we also drop the receiver so its final
/// events are ignored.
fn close_and_cancel(palette: &mut CommandPalette, agent_ch: &mut AgentChannel) {
    if let Some(flag) = agent_ch.cancel.lock().unwrap().take() {
        flag.store(true, Ordering::Relaxed);
    }
    *agent_ch.rx.lock().unwrap() = None;
    close(palette);
}

// ---------- Agent ----------

/// Kick off an agent run: resolve config, assemble context, spawn the
/// blocking loop on a worker thread, and switch to Busy. Transcript events
/// stream back over [`AgentChannel`] and are drained by [`poll_agent`].
fn start_agent(
    palette: &mut CommandPalette,
    projects: &Projects,
    pane_registry: &PaneRegistry,
    agent_ch: &mut AgentChannel,
) {
    let cfg = match jim_inference::llm::LlmConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            palette.mode = PaletteMode::Plan;
            palette.message = format!("Agent unavailable: {e}");
            palette.transcript.clear();
            return;
        }
    };
    let context = assemble_context(projects, pane_registry);
    let goal = palette.query.trim().to_string();
    let system = agent::build_system_prompt(&context);

    let (tx, rx) = std::sync::mpsc::channel::<AgentMsg>();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();
    let spawned = std::thread::Builder::new()
        .name("jim-agent".into())
        .spawn(move || agent::run(cfg, system, goal, tx, cancel_worker));
    if spawned.is_err() {
        palette.mode = PaletteMode::Plan;
        palette.message = "Could not spawn agent worker".into();
        return;
    }
    *agent_ch.rx.lock().unwrap() = Some(rx);
    *agent_ch.cancel.lock().unwrap() = Some(cancel);
    palette.mode = PaletteMode::Busy;
    palette.message = "Agent working…".into();
    palette.transcript.clear();
}

/// Concise context handed to the model.
fn assemble_context(projects: &Projects, pane_registry: &PaneRegistry) -> String {
    let mut s = String::new();
    if let Some(active) = projects.active {
        s.push_str(&format!(
            "Active project: {}\n",
            projects.name_of(active).unwrap_or("?")
        ));
        if let Some(cwd) = projects.default_cwd_of(active) {
            s.push_str(&format!("Active project cwd: {cwd}\n"));
        }
    }
    let names: Vec<&str> = projects.list.iter().map(|p| p.name.as_str()).collect();
    if !names.is_empty() {
        s.push_str(&format!("Known projects: {}\n", names.join(", ")));
    }
    let kinds: Vec<&str> = pane_registry.iter().map(|k| k.kind).collect();
    s.push_str(&format!("Pane kinds (for 'kind' args): {}\n", kinds.join(", ")));
    s
}

/// Drain transcript events from the agent worker each frame, appending them
/// as transcript lines. On Done/Error the palette flips to Plan (the run is
/// over, transcript stays visible until dismissed).
fn poll_agent(mut palette: ResMut<CommandPalette>, agent_ch: Res<AgentChannel>) {
    if palette.mode != PaletteMode::Busy {
        return;
    }
    // Collect everything available this frame without holding the lock while
    // mutating the palette.
    let mut events: Vec<AgentMsg> = Vec::new();
    let mut disconnected = false;
    {
        let mut guard = agent_ch.rx.lock().unwrap();
        if let Some(rx) = guard.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(msg) => events.push(msg),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            *guard = None;
        }
    }

    let mut finished = false;
    for ev in events {
        match ev {
            AgentMsg::Thought(t) => palette.transcript.push(TLine {
                icon: "·",
                text: t,
                color: "fg_muted",
            }),
            AgentMsg::Action(c) => palette.transcript.push(TLine {
                icon: "$",
                text: c,
                color: "accent",
            }),
            AgentMsg::Observation(o) => palette.transcript.push(TLine {
                icon: "→",
                text: o,
                color: "fg_muted",
            }),
            AgentMsg::Done(m) => {
                palette.message = m;
                finished = true;
            }
            AgentMsg::Error(e) => {
                palette.message = format!("Agent error: {e}");
                finished = true;
            }
        }
    }

    if finished {
        palette.mode = PaletteMode::Plan;
        *agent_ch.rx.lock().unwrap() = None;
        *agent_ch.cancel.lock().unwrap() = None;
    } else if disconnected {
        // Worker died without a Done/Error (shouldn't happen) — don't hang.
        palette.message = "Agent worker exited".into();
        palette.mode = PaletteMode::Plan;
        *agent_ch.cancel.lock().unwrap() = None;
    }
}

// ---------- Render (exclusive) ----------

fn render_palette(world: &mut World) {
    let open = world.resource::<CommandPalette>().open;
    let sig = world.resource::<CommandPalette>().signature();
    let prev_root = world.resource::<CommandPalette>().root;
    let last_sig = world.resource::<CommandPalette>().last_sig;

    if !open {
        if let Some(root) = prev_root {
            despawn_tree(world, root);
            world.resource_mut::<CommandPalette>().root = None;
        }
        return;
    }
    if prev_root.is_some() && sig == last_sig {
        return;
    }
    if let Some(root) = prev_root {
        despawn_tree(world, root);
    }

    let win_h = {
        let mut q = world.query::<&Window>();
        match q.iter(world).next() {
            Some(w) => w.height(),
            None => return,
        }
    };

    let theme = world.resource::<jim_style::Theme>().clone();
    let fonts = world.resource::<jim_style::FontRegistry>().clone();
    let font = world.resource::<PaneFont>().0.clone();
    let metrics = *world.resource::<PaneFontMetrics>();
    let colors = WidgetPalette::from_theme(&theme);

    let el = build_palette_element(world.resource::<CommandPalette>());

    let top_left = Vec2::new(-PALETTE_W * 0.5, win_h * 0.5 - TOP_MARGIN);
    let root = world
        .spawn((
            Transform::from_xyz(top_left.x, top_left.y, PALETTE_Z),
            Visibility::Visible,
            RenderLayers::layer(MENU_OVERLAY_LAYER),
        ))
        .id();

    let ctx = LayoutCtx {
        font,
        metrics,
        owner_pane: root,
        content_root: root,
        content_size: Vec2::new(PALETTE_W, win_h),
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
            PALETTE_W,
            0.0,
        );
    }
    world.flush();
    stamp_layer(world, root, MENU_OVERLAY_LAYER);

    let mut p = world.resource_mut::<CommandPalette>();
    p.root = Some(root);
    p.last_sig = sig;
}

fn build_palette_element(palette: &CommandPalette) -> Element {
    let children = match palette.mode {
        PaletteMode::Actions => actions_children(palette),
        PaletteMode::Busy => transcript_children(palette, true),
        PaletteMode::Plan => transcript_children(palette, false),
    };

    Element::Frame {
        gap: 4.0,
        pad: 0.0,
        children,
        style: Some(Style {
            background: Some("surface_2".into()),
            radius: Some("radius_lg".into()),
            border: Some(Border {
                color: "surface_3".into(),
                width: 1.0,
            }),
            padding: Some(Edges::all(14.0)),
            width: Some(format!("{}", PALETTE_W as i32)),
            shadow: Some(Shadow {
                token: Some("shadow_lg".into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
    }
}

fn actions_children(palette: &CommandPalette) -> Vec<Element> {
    let mut children = vec![query_text(&palette.query)];
    if palette.results.is_empty() {
        children.push(text_muted("no matching actions", 14.0));
    }
    for (i, row) in palette.results.iter().take(MAX_ROWS).enumerate() {
        let hint = row.keybind.clone().unwrap_or_else(|| row.category.to_string());
        children.push(list_row(
            row.id,
            &row.title,
            &hint,
            i == palette.selected,
            "fg",
        ));
    }
    children
}

/// Render the agent transcript. `running` true → Busy (footer: Esc cancels);
/// false → Plan (run finished; the header holds the final answer).
fn transcript_children(palette: &CommandPalette, running: bool) -> Vec<Element> {
    let mut children = Vec::new();
    children.push(text_colored(&palette.message, "accent", 16.0));

    // Keep the visible transcript bounded; show the most recent lines.
    let max_lines = 12usize;
    let skip = palette.transcript.len().saturating_sub(max_lines);
    if skip > 0 {
        children.push(text_muted(&format!("… {skip} earlier steps"), 12.0));
    }
    for line in palette.transcript.iter().skip(skip) {
        let label = format!("{}  {}", line.icon, line.text);
        children.push(text_colored(&label, line.color, 13.0));
    }

    children.push(text_muted(
        if running {
            "running · Esc to stop"
        } else {
            "Esc to dismiss"
        },
        12.0,
    ));
    children
}

// ---------- Element helpers ----------

fn query_text(query: &str) -> Element {
    Element::Text {
        value: format!("› {}▏", query),
        color: Some("fg".into()),
        size: Some(20.0),
        weight: Some(Weight::Normal),
        family: None,
        selectable: false,
    }
}

fn list_row(id: &str, title: &str, hint: &str, selected: bool, title_color: &str) -> Element {
    let title_el = Element::Frame {
        gap: 0.0,
        pad: 0.0,
        children: vec![Element::Text {
            value: title.to_string(),
            color: Some(title_color.into()),
            size: Some(15.0),
            weight: Some(Weight::Normal),
            family: None,
            selectable: false,
        }],
        style: Some(Style {
            flex_grow: Some(1.0),
            ..Default::default()
        }),
    };
    let hint_el = Element::Text {
        value: hint.to_string(),
        color: Some("fg_muted".into()),
        size: Some(13.0),
        weight: Some(Weight::Normal),
        family: None,
        selectable: false,
    };
    Element::ListItem {
        id: id.to_string(),
        selected,
        gap: 8.0,
        pad: 8.0,
        children: vec![Element::Hstack {
            gap: 8.0,
            pad: 0.0,
            align: Align::Center,
            children: vec![title_el, hint_el],
            style: Some(Style {
                width: Some("100%".into()),
                ..Default::default()
            }),
        }],
        style: None,
    }
}

fn text_muted(s: &str, size: f32) -> Element {
    text_colored(s, "fg_muted", size)
}

fn text_colored(s: &str, color: &str, size: f32) -> Element {
    Element::Text {
        value: s.to_string(),
        color: Some(color.into()),
        size: Some(size),
        weight: Some(Weight::Normal),
        family: None,
        selectable: false,
    }
}

// ---------- Subtree helpers ----------

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

fn despawn_tree(world: &mut World, root: Entity) {
    // `despawn` cascades to descendants, so despawning the root is enough
    // — walking and despawning each child would double-despawn and log
    // "entity is invalid" warnings.
    let _ = world.despawn(root);
}
