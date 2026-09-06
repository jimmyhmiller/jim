//! Projects + sidebar.
//!
//! A project is a named bucket that owns one or more terminal entities.
//! At any moment exactly one project is "active" — terminals belonging
//! to other projects keep running (their worker threads are unaffected),
//! they're just hidden via `Visibility::Hidden` and skipped by the
//! mouse picker. Switching projects flips which set is visible.
//!
//! ## Persistence
//!
//! Project list + active id + the next id to hand out are serialized to
//! `~/.jim/projects.json`. Terminal *layouts* (position, size,
//! z, project membership, session id) are also stored there; their raw
//! pty byte streams live alongside in `~/.jim/scrollback/<id>.bytes`
//! so the visible scrollback survives restarts. The shell process itself
//! can't survive — restored terminals always get a fresh shell, the
//! prior session's bytes just paint the screen behind it.
//!
//! ## Sidebar UI
//!
//! Drawn as flat sprites + Text2d on the LEFT edge of the window. No
//! `bevy_ui`, matching the rest of this crate. The whole tree is
//! rebuilt whenever `Projects::layout_dirty` is set or the window
//! dimensions change — cheap because it's a few dozen entities, and
//! avoids a separate "follow the window" system that would have to
//! shadow every layout decision.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use bevy::camera::ClearColorConfig;
use bevy::camera::visibility::RenderLayers;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::text::LineHeight;
use serde::{Deserialize, Serialize};

use jim_pane::{
    FocusedPane, MIN_PANE_SIZE, PaneKindMarker, PanePinned, PaneProject, PaneRect, PaneRegistry,
    PaneSnapshot, PaneTag, PaneTitle, spawn_pane_from_registry,
};

use jim_terminal::TerminalSession;

use jim_editor::EditorFilePath;

use jim_terminal::{FONT_SIZE, MonoFont, MonoMetrics};

pub const SIDEBAR_DEFAULT_WIDTH: f32 = 220.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 160.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 480.0;
/// Hit area for the resize handle is `2 * SIDEBAR_RESIZE_HALF` pixels
/// wide, centered on the sidebar's right edge — wider than the visible
/// divider so the user doesn't have to be pixel-perfect.
const SIDEBAR_RESIZE_HALF: f32 = 4.0;
/// Far enough above any terminal `rect.z` (terminals start at 1.0 and
/// only step up by 1.0 per focus-bring-to-front) but well inside the
/// default Bevy 2D camera's far plane (1000) — z values past that get
/// silently clipped, which manifests as a black screen on first run.
const SIDEBAR_Z: f32 = 500.0;

// Muted, modern palette — no saturated reds/greens/blues. Active state
// uses a thin accent stripe instead of a full-row colour wash.
/// Theme-derived sidebar colors, resolved once per `sidebar_layout`
/// call. Preset switches retone the whole sidebar in the same frame
/// because layout rebuilds on `theme.is_changed()`.
struct SidebarPalette {
    bg: Color,
    divider: Color,
    row_active_bg: Color,
    terminal_row_bg: Color,
    row_renaming_bg: Color,
    active_stripe: Color,
    edit_underline: Color,
    text: Color,
    text_dim: Color,
    text_faint: Color,
}

fn sidebar_palette(theme: &jim_style::Theme) -> SidebarPalette {
    use jim_style::tokens as t;
    let c = |id| Color::LinearRgba(theme.color(id));
    let sidebar_bg = theme.color(t::SIDEBAR_BG);
    SidebarPalette {
        bg: Color::LinearRgba(sidebar_bg),
        divider: c(t::CHROME_DIVIDER),
        row_active_bg: c(t::SIDEBAR_ROW_ACTIVE_BG),
        terminal_row_bg: Color::LinearRgba(LinearRgba::new(
            sidebar_bg.red * 0.88,
            sidebar_bg.green * 0.88,
            sidebar_bg.blue * 0.88,
            sidebar_bg.alpha,
        )),
        row_renaming_bg: c(t::SIDEBAR_ROW_RENAMING_BG),
        active_stripe: c(t::ACCENT),
        edit_underline: c(t::ACCENT),
        text: c(t::FG),
        text_dim: c(t::FG_MUTED),
        text_faint: c(t::SIDEBAR_TEXT_FAINT),
    }
}

const HEADER_H: f32 = 36.0;
const ROW_H: f32 = 28.0;
const ROW_PAD_X: f32 = 14.0;
const STRIPE_W: f32 = 3.0;
const DELETE_W: f32 = 22.0;
/// Width of the per-row hide/show eye column, sitting just left of the
/// delete glyph. Only painted while the row (or the project) is hovered,
/// but the hit-rect is always live.
const EYE_W: f32 = 22.0;
/// Side of the square bottom-left hot-zone that reveals + toggles the
/// global "show hidden projects" eyeball.
const EYE_ZONE: f32 = 40.0;
/// Pixels the cursor must travel from the press point before a project
/// row press is treated as a reorder drag rather than a click.
const DRAG_THRESHOLD: f32 = 4.0;
const DIVIDER_H: f32 = 1.0;
const TEXT_FONT_SIZE: f32 = 13.0;
const HEADER_FONT_SIZE: f32 = 12.0;

// ----- Workspace switcher strip (sidebar header, right side) -----
//
// A page indicator: one short bar per workspace, the current one lit.
// Bars rather than dots because the sidebar draws with plain `Sprite`s
// and a rectangle is the only shape that costs nothing.

/// Width of one workspace bar. Uniform across workspaces — the active
/// one is distinguished by colour, not size, which keeps the strip's
/// width a plain multiplication and so keeps the overflow window below
/// honest.
const WS_BAR_W: f32 = 10.0;
const WS_BAR_H: f32 = 3.0;
/// Gap between bars, and between the last bar and the `+`.
const WS_BAR_GAP: f32 = 5.0;
/// Hit width of the `+` that adds a workspace.
const WS_ADD_W: f32 = 16.0;
/// Inset of the whole strip from the sidebar's inner (right) edge.
const WS_STRIP_PAD_R: f32 = 8.0;
/// Horizontal space always left for the workspace name, however many
/// workspaces there are. Past this the strip drops bars from whichever
/// end is furthest from the current workspace, rather than shrinking the
/// name to nothing.
const WS_NAME_MIN_W: f32 = 70.0;

const NEW_TERMINAL_OFFSET: f32 = 28.0;

// ---------- Persistence ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectData {
    pub id: u64,
    pub name: String,
    /// Remembered working directory for terminals spawned in this
    /// project. Populated by the inference layer the first time the
    /// user `cd`s into a plausible project root (the classifier in
    /// [`crate::inferences_pane`]'s feed decides); used at terminal
    /// spawn time as the initial cwd. `None` means "no remembered
    /// default" → fall back to `$HOME` like every other terminal.
    /// `serde(default)` keeps old projects.json files loadable.
    #[serde(default)]
    pub default_cwd: Option<String>,
    /// Legacy pre-workspace park flag. Hiding is per-WORKSPACE now — see
    /// [`WorkspaceData::hidden`] — so this is read once, to seed the
    /// first workspace out of an old save, and never written back.
    /// Nothing outside [`Projects::from_persisted`] may read it; ask
    /// [`Projects::is_hidden`] instead, which knows which workspace you
    /// are on.
    ///
    /// `rename` is load-bearing: the on-disk key is still `hidden`, and
    /// without it every existing save silently migrates to "nothing
    /// parked" — a 44-project sidebar where 35 were tucked away.
    #[serde(default, rename = "hidden", skip_serializing)]
    legacy_hidden: bool,
}

/// A workspace: one saved sidebar configuration.
///
/// A workspace does NOT own projects. Every project exists in every
/// workspace; what a workspace remembers is which of them are *parked*
/// (see the hidden/switchable rules on [`Projects`]) and which one you
/// were last working in. So switching workspaces never moves a project,
/// touches a pane, or kills a shell — it parks a different subset and
/// restores the active project you left behind.
///
/// There is always at least one. Deleting the last one is refused, since
/// every hide decision in the app is stored inside one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceData {
    pub id: u64,
    pub name: String,
    /// Projects parked in THIS workspace. Deleting a project sweeps its
    /// id out of every workspace, so this never accumulates ghosts that
    /// would silently re-park a recycled id.
    #[serde(default)]
    pub hidden: Vec<u64>,
    /// Project that was active when this workspace was last left.
    /// Re-validated on the way back in — it may have been deleted, or
    /// parked in this workspace, while we were away.
    #[serde(default)]
    pub active: Option<u64>,
}

/// Name given to the workspace an old (pre-workspace) save is migrated
/// into, and to the first workspace of a fresh install.
const DEFAULT_WORKSPACE_NAME: &str = "Main";

/// Legacy terminal-only snapshot from before the pane unification.
/// Kept for `serde(default)` deserialization so old projects.json files
/// still load; on save we always write the new `panes` field instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub session_id: u64,
    pub project_id: u64,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub z: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    projects: Vec<ProjectData>,
    #[serde(default)]
    active: Option<u64>,
    #[serde(default)]
    next_id: u64,
    /// Saved sidebar configurations. Empty in any save written before
    /// workspaces existed; [`Projects::from_persisted`] migrates those
    /// into a single workspace built from the old per-project flags.
    #[serde(default)]
    workspaces: Vec<WorkspaceData>,
    #[serde(default)]
    active_workspace: Option<u64>,
    #[serde(default)]
    next_workspace_id: u64,
    #[serde(default)]
    sidebar_width: Option<f32>,
    /// Legacy field — populated when reading old saves; never written.
    #[serde(default, skip_serializing)]
    terminals: Vec<TerminalSnapshot>,
    /// All panes (any kind) with their kind-specific config blob.
    #[serde(default)]
    panes: Vec<PaneSnapshot>,
    #[serde(default)]
    next_terminal_id: u64,
    /// Next nested-canvas id to hand out (see [`Projects::next_canvas_id`]).
    #[serde(default)]
    next_canvas_id: u64,
    /// Next per-pane thumbnail id (see [`Projects::next_snap_id`]).
    #[serde(default)]
    next_snap_id: u64,
    /// Per-level canvas view (pan + zoom). Keyed by `"project:canvas"` as
    /// a string for JSON friendliness (`canvas == 0` is the project
    /// root). `serde(default)` keeps old saves loadable; missing levels
    /// use the default view.
    #[serde(default)]
    canvas_views: std::collections::HashMap<String, crate::canvas::CanvasViewState>,
}

fn save_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".jim");
    Some(p)
}

fn load_persisted() -> PersistedState {
    let Some(dir) = save_path() else {
        return PersistedState::default();
    };
    let file = dir.join("projects.json");
    let Ok(bytes) = fs::read(&file) else {
        return PersistedState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        eprintln!(
            "[projects] failed to parse {}: {} — starting empty",
            file.display(),
            e
        );
        PersistedState::default()
    })
}

fn save_persisted(state: &PersistedState) {
    let Some(dir) = save_path() else {
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[projects] mkdir {}: {}", dir.display(), e);
        return;
    }
    let file = dir.join("projects.json");
    let tmp = dir.join("projects.json.tmp");
    let bytes = match serde_json::to_vec_pretty(state) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[projects] serialize failed: {}", e);
            return;
        }
    };
    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &file)
    })();
    if let Err(e) = write_result {
        eprintln!("[projects] write {}: {}", file.display(), e);
    }
}

// ---------- Resource state ----------

#[derive(Resource, Default)]
pub struct Projects {
    pub list: Vec<ProjectData>,
    pub active: Option<u64>,
    pub next_id: u64,
    /// Saved sidebar configurations, in swipe order. Never empty once
    /// `load_or_seed_projects` has run; the `Default` impl leaves it
    /// empty only for the placeholder resource inserted before Startup,
    /// so every accessor tolerates that.
    pub workspaces: Vec<WorkspaceData>,
    /// Which workspace the sidebar is showing. See
    /// [`Projects::switch_workspace`].
    pub active_workspace: u64,
    pub next_workspace_id: u64,
    /// A workspace change waiting to be animated: `(left behind, travel
    /// direction)`. Written by [`Projects::switch_workspace_toward`] and
    /// taken by `sidebar_slide`. Session-only — a switch that happens
    /// while nothing is drawing simply isn't animated.
    pub pending_switch: Option<(u64, f32)>,
    /// Counter for `TerminalSession` ids. Bumped on every spawn (new or
    /// restored) so we never collide with an existing scrollback file.
    pub next_terminal_id: u64,
    /// Counter for nested-canvas ids (see [`jim_pane::PaneCanvas`]).
    /// Globally unique across projects; `0` is reserved for "root
    /// canvas", so allocation starts at 1. Bumped when a `canvas` tile is
    /// created and persisted so restored tiles never collide.
    pub next_canvas_id: u64,
    /// Counter for per-pane thumbnail ids (see [`jim_pane::PaneSnapId`]).
    /// `0` = none; allocation starts at 1.
    pub next_snap_id: u64,
    /// Set when the on-disk file is out of date.
    pub dirty: bool,
    /// Set when the sidebar entity tree needs rebuilding (rows added /
    /// removed / renamed / active-project changed).
    pub layout_dirty: bool,
    /// Set when terminal layouts (positions, sizes, membership) have
    /// changed and need flushing to disk. Saved separately from `dirty`
    /// because we want to debounce drag/resize bursts to mouse-up; the
    /// project-state save is fine to fire immediately.
    pub terminals_dirty: bool,
    /// Per-project unread BEL counter. Bumped when a terminal in a
    /// hidden context (window unfocused, or project not active) rings
    /// the bell; cleared when the user is actually looking at that
    /// project. Session-only — not persisted to projects.json.
    pub unread_bells: std::collections::HashMap<u64, u64>,
    /// When true, the sidebar also lists projects whose `hidden` flag is
    /// set (drawn dimmed). Toggled by the bottom-left eyeball. View
    /// state only — session-local, not persisted.
    pub show_hidden: bool,
}

impl Projects {
    fn from_persisted(p: PersistedState) -> Self {
        let next_id = p
            .next_id
            .max(p.projects.iter().map(|p| p.id + 1).max().unwrap_or(1));
        let legacy_max_session = p
            .terminals
            .iter()
            .map(|t| t.session_id + 1)
            .max()
            .unwrap_or(0);
        let panes_max_session = p
            .panes
            .iter()
            .filter(|p| p.kind == "terminal")
            .filter_map(|p| p.config.get("session_id").and_then(|v| v.as_u64()))
            .map(|id| id + 1)
            .max()
            .unwrap_or(0);
        let next_terminal_id = p
            .next_terminal_id
            .max(legacy_max_session.max(panes_max_session))
            .max(1);
        // Guard the canvas counter against hand-edited / legacy saves the
        // same way as terminals: never hand out an id a restored tile or
        // gathered pane already uses.
        let panes_max_canvas = p
            .panes
            .iter()
            .map(|pane| pane.canvas + 1)
            .max()
            .unwrap_or(0);
        let tiles_max_canvas = p
            .panes
            .iter()
            .filter(|pane| pane.kind == crate::canvas_pane::PANE_KIND)
            .filter_map(|pane| pane.config.get("canvas_id").and_then(|v| v.as_u64()))
            .map(|id| id + 1)
            .max()
            .unwrap_or(0);
        let next_canvas_id = p
            .next_canvas_id
            .max(panes_max_canvas.max(tiles_max_canvas))
            .max(1);
        let panes_max_snap = p
            .panes
            .iter()
            .map(|pane| pane.snap_id + 1)
            .max()
            .unwrap_or(0);
        let next_snap_id = p.next_snap_id.max(panes_max_snap).max(1);
        // Workspaces. A save written before they existed has none, and
        // its per-project `hidden` flags ARE a sidebar configuration —
        // just the only one there was. Migrating them into one workspace
        // means an upgrade changes nothing the user can see.
        let mut workspaces = p.workspaces;
        if workspaces.is_empty() {
            workspaces.push(WorkspaceData {
                id: 1,
                name: DEFAULT_WORKSPACE_NAME.to_string(),
                hidden: p
                    .projects
                    .iter()
                    .filter(|pr| pr.legacy_hidden)
                    .map(|pr| pr.id)
                    .collect(),
                active: p.active,
            });
        }
        // Same guard as every other counter here: never hand out an id a
        // hand-edited or older save already uses.
        let next_workspace_id = p
            .next_workspace_id
            .max(workspaces.iter().map(|w| w.id + 1).max().unwrap_or(1))
            .max(1);
        let active_workspace = p
            .active_workspace
            .filter(|id| workspaces.iter().any(|w| w.id == *id))
            .unwrap_or(workspaces[0].id);
        Self {
            list: p.projects,
            active: p.active,
            next_id,
            workspaces,
            active_workspace,
            next_workspace_id,
            next_terminal_id,
            next_canvas_id,
            next_snap_id,
            dirty: false,
            layout_dirty: true,
            terminals_dirty: false,
            unread_bells: std::collections::HashMap::new(),
            show_hidden: false,
            pending_switch: None,
        }
    }
    /// Hand out a fresh nested-canvas id and mark state dirty so the
    /// bumped counter is persisted.
    pub fn allocate_canvas_id(&mut self) -> u64 {
        let id = self.next_canvas_id.max(1);
        self.next_canvas_id = id + 1;
        self.dirty = true;
        id
    }
    /// Hand out a fresh per-pane thumbnail id.
    pub fn allocate_snap_id(&mut self) -> u64 {
        let id = self.next_snap_id.max(1);
        self.next_snap_id = id + 1;
        self.dirty = true;
        id
    }
    pub fn allocate_terminal_id(&mut self) -> u64 {
        let id = self.next_terminal_id.max(1);
        self.next_terminal_id = id + 1;
        self.dirty = true;
        id
    }
    /// Add a project, listed in the CURRENT workspace and parked in
    /// every other one.
    ///
    /// Projects are global — they exist in every workspace — so a new
    /// one would otherwise appear in all of them at once, which defeats
    /// the point of having separated them. Parking it elsewhere means
    /// "new project shows up where I made it", and un-parking it
    /// somewhere else is one eye-click away.
    pub fn create(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        self.list.push(ProjectData {
            id,
            name: format!("Project {}", id),
            default_cwd: None,
            legacy_hidden: false,
        });
        let current = self.active_workspace;
        for w in &mut self.workspaces {
            if w.id != current {
                w.hidden.push(id);
            }
        }
        if self.active.is_none() {
            self.active = Some(id);
        }
        self.dirty = true;
        self.layout_dirty = true;
        id
    }
    pub fn delete(&mut self, id: u64) {
        let before = self.list.len();
        self.list.retain(|p| p.id != id);
        if self.list.len() == before {
            return;
        }
        // Sweep the id out of every workspace. Left behind, a stale park
        // entry would silently re-hide whatever project inherits the id,
        // and a stale `active` would restore a project that no longer
        // exists on the next swipe back.
        for w in &mut self.workspaces {
            w.hidden.retain(|&h| h != id);
            if w.active == Some(id) {
                w.active = None;
            }
        }
        if self.active == Some(id) {
            self.active = self.first_switchable();
        }
        self.dirty = true;
        self.layout_dirty = true;
    }
    pub fn set_active(&mut self, id: u64) {
        // Hidden projects are parked — not part of the switch rotation.
        // Guarding here means no switcher (sidebar, cube/prism, future
        // UIs) can ever land on a hidden project, so the "active is always
        // switchable" invariant holds no matter who calls us.
        if self.is_hidden(id) {
            return;
        }
        if self.active != Some(id) {
            self.active = Some(id);
            self.dirty = true;
            self.layout_dirty = true;
        }
    }
    pub fn rename(&mut self, id: u64, new_name: String) {
        for p in &mut self.list {
            if p.id == id {
                if p.name != new_name {
                    p.name = new_name;
                    self.dirty = true;
                    self.layout_dirty = true;
                }
                return;
            }
        }
    }
    // ----- Hidden / switchable projects -----
    //
    // "Hidden" is one semantic concept, not a sidebar detail: a hidden
    // project is *parked*. It keeps all its data and panes, but it drops
    // out of every place a user picks or cycles projects — the sidebar
    // nav, the cube/prism overview, and any switcher added later. The
    // single rule every consumer relies on:
    //
    //     the active project is ALWAYS switchable (never hidden).
    //
    // Enforced in two spots: `set_active` refuses hidden targets, and
    // `set_hidden` re-homes `active` if you park the current project.
    // Everything else just reads `switchable*()` and gets it for free.
    //
    // Parking is per-WORKSPACE (see [`WorkspaceData`]): the same project
    // can be listed in one workspace and tucked away in the next. That
    // is the whole of what a workspace is, which is why these accessors
    // are the only place the distinction shows up — every caller already
    // asked "is this hidden?" rather than reading a flag, so they all
    // became workspace-aware for free.

    /// Projects parked in the current workspace. Empty before Startup
    /// has built any workspace, which is the same as "nothing parked".
    fn parked(&self) -> &[u64] {
        self.workspace().map(|w| w.hidden.as_slice()).unwrap_or(&[])
    }

    /// Is this project parked in the current workspace? (Unknown ids are
    /// treated as not hidden.)
    pub fn is_hidden(&self, id: u64) -> bool {
        self.parked().contains(&id)
    }

    /// The projects a user can switch between, in list order. THIS is the
    /// set every switcher must enumerate — never `list` directly — so
    /// parked projects stay out of all of them.
    pub fn switchable(&self) -> impl Iterator<Item = &ProjectData> {
        let parked = self.parked();
        self.list.iter().filter(move |p| !parked.contains(&p.id))
    }
    pub fn switchable_ids(&self) -> Vec<u64> {
        self.switchable().map(|p| p.id).collect()
    }
    /// First switchable project (the canonical fallback target whenever
    /// `active` needs re-homing). `None` only when every project is parked.
    pub fn first_switchable(&self) -> Option<u64> {
        self.switchable().next().map(|p| p.id)
    }

    /// Projects to draw in the sidebar: the switchable ones, plus parked
    /// ones while `show_hidden` is on (the management view that lets you
    /// un-park them). Distinct from `switchable_ids` on purpose — revealing
    /// hidden rows in the sidebar must NOT make them switchable elsewhere.
    pub fn sidebar_ids(&self) -> Vec<u64> {
        let parked = self.parked();
        self.list
            .iter()
            .filter(|p| self.show_hidden || !parked.contains(&p.id))
            .map(|p| p.id)
            .collect()
    }

    /// Park / un-park a project **in the current workspace only**.
    ///
    /// Maintains the active-is-switchable invariant: parking the active
    /// project re-homes `active` to the first remaining switchable one
    /// (or `None` if none are left); un-parking when nothing is active
    /// adopts it as active.
    pub fn set_hidden(&mut self, id: u64, hidden: bool) {
        if !self.list.iter().any(|p| p.id == id) {
            return;
        }
        let Some(ws) = self.workspace_mut() else {
            return;
        };
        let changed = if hidden {
            let missing = !ws.hidden.contains(&id);
            if missing {
                ws.hidden.push(id);
            }
            missing
        } else {
            let before = ws.hidden.len();
            ws.hidden.retain(|&h| h != id);
            ws.hidden.len() != before
        };
        if !changed {
            return;
        }
        self.dirty = true;
        self.layout_dirty = true;
        if hidden {
            if self.active == Some(id) {
                self.active = self.first_switchable();
            }
        } else if self.active.is_none() {
            self.active = Some(id);
        }
    }
    /// Convenience toggle used by the sidebar eye affordance.
    pub fn toggle_hidden(&mut self, id: u64) {
        self.set_hidden(id, !self.is_hidden(id));
    }

    // ----- Workspaces -----
    //
    // A workspace is a saved sidebar configuration: which projects are
    // parked, and which one you were working in. Swiping two fingers
    // horizontally over the sidebar cycles them (see `sidebar_swipe`).
    //
    // Every method here keeps two invariants, because breaking either
    // strands the user with an unusable sidebar:
    //   * there is always at least one workspace, and
    //   * `active_workspace` always names one that exists.

    /// The workspace the sidebar is currently showing. `None` only for
    /// the placeholder resource that exists before Startup builds the
    /// real one.
    pub fn workspace(&self) -> Option<&WorkspaceData> {
        self.workspaces
            .iter()
            .find(|w| w.id == self.active_workspace)
            .or_else(|| self.workspaces.first())
    }

    fn workspace_mut(&mut self) -> Option<&mut WorkspaceData> {
        let id = self.active_workspace;
        if self.workspaces.iter().any(|w| w.id == id) {
            self.workspaces.iter_mut().find(|w| w.id == id)
        } else {
            self.workspaces.first_mut()
        }
    }

    /// Display name of the current workspace (the sidebar header).
    pub fn workspace_name(&self) -> &str {
        self.workspace().map_or("", |w| w.name.as_str())
    }

    /// Index of the current workspace in swipe order, for the header
    /// dot strip.
    pub fn workspace_index(&self) -> usize {
        self.workspaces
            .iter()
            .position(|w| w.id == self.active_workspace)
            .unwrap_or(0)
    }

    pub fn workspace_id_by_name(&self, name: &str) -> Option<u64> {
        self.workspaces
            .iter()
            .find(|w| w.name.eq_ignore_ascii_case(name))
            .map(|w| w.id)
    }

    /// Add a workspace, and switch to it.
    ///
    /// It starts as a FORK of the one you were on — same parked set,
    /// same active project. Starting from "everything parked" would open
    /// onto an empty sidebar that looks broken, and starting from
    /// "nothing parked" would make the first two workspaces identical
    /// for anyone who had already tucked projects away. A fork is the
    /// only option you can adjust with one eye-click in either
    /// direction.
    pub fn create_workspace(&mut self, name: Option<String>) -> u64 {
        let id = self.next_workspace_id.max(1);
        self.next_workspace_id = id + 1;
        let (hidden, active) = self
            .workspace()
            .map(|w| (w.hidden.clone(), w.active.or(self.active)))
            .unwrap_or_default();
        // Remember where we were before leaving, same as a switch.
        let leaving = self.active;
        if let Some(ws) = self.workspace_mut() {
            ws.active = leaving;
        }
        self.workspaces.push(WorkspaceData {
            id,
            name: name.unwrap_or_else(|| format!("Workspace {id}")),
            hidden,
            active,
        });
        self.active_workspace = id;
        self.dirty = true;
        self.layout_dirty = true;
        id
    }

    /// Remove a workspace. Refused (returning false) for the last one:
    /// every hide decision in the app lives inside a workspace, so there
    /// has to be somewhere to stand.
    pub fn delete_workspace(&mut self, id: u64) -> bool {
        if self.workspaces.len() <= 1 || !self.workspaces.iter().any(|w| w.id == id) {
            return false;
        }
        let idx = self.workspaces.iter().position(|w| w.id == id).unwrap_or(0);
        self.workspaces.retain(|w| w.id != id);
        if self.active_workspace == id {
            // Land on the neighbour that took its place, or the new last
            // one if we deleted off the end.
            let next = self.workspaces[idx.min(self.workspaces.len() - 1)].id;
            self.active_workspace = next;
            self.adopt_workspace_active();
        }
        self.dirty = true;
        self.layout_dirty = true;
        true
    }

    pub fn rename_workspace(&mut self, id: u64, name: String) {
        for w in &mut self.workspaces {
            if w.id == id {
                if w.name != name {
                    w.name = name;
                    self.dirty = true;
                    self.layout_dirty = true;
                }
                return;
            }
        }
    }

    /// Show a different workspace.
    ///
    /// The workspace being left records the project you were in, so
    /// swiping back lands exactly where you were. The one being entered
    /// restores its own — re-validated, because that project may have
    /// been deleted, or parked in this workspace, since you last left.
    pub fn switch_workspace(&mut self, id: u64) {
        // Which way the list should travel. Comparing positions is right
        // for a click on the strip or a name from the CLI; a wrapping
        // swipe knows better and says so via `switch_workspace_toward`.
        let dir = self
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .map(|to| {
                if to as i64 > self.workspace_index() as i64 {
                    1
                } else {
                    -1
                }
            })
            .unwrap_or(1);
        self.switch_workspace_toward(id, dir);
    }

    /// `switch_workspace`, with the travel direction stated rather than
    /// inferred. Cycling off one end of the strip and onto the other is
    /// still forward motion even though the index goes backwards.
    pub fn switch_workspace_toward(&mut self, id: u64, dir: i32) {
        if self.active_workspace == id || !self.workspaces.iter().any(|w| w.id == id) {
            return;
        }
        let leaving = self.active;
        if let Some(ws) = self.workspace_mut() {
            ws.active = leaving;
        }
        let from = self.active_workspace;
        self.active_workspace = id;
        self.adopt_workspace_active();
        // Every entry point — swipe, strip click, `jimctl`, palette —
        // funnels through here, so recording the move is enough to make
        // all of them animate. `sidebar_slide` picks it up.
        self.pending_switch = Some((from, if dir >= 0 { 1.0 } else { -1.0 }));
        self.dirty = true;
        self.layout_dirty = true;
    }

    /// Point `active` at whatever the current workspace remembers,
    /// falling back to its first switchable project. Upholds the
    /// active-is-switchable invariant across a workspace change, where
    /// the *set* of switchable projects moves under `active`.
    fn adopt_workspace_active(&mut self) {
        let remembered = self.workspace().and_then(|w| w.active);
        let usable = remembered
            .filter(|a| self.list.iter().any(|p| p.id == *a))
            .filter(|a| !self.is_hidden(*a));
        self.active = usable.or_else(|| self.first_switchable());
    }

    /// Step `delta` workspaces along the swipe order, wrapping. Returns
    /// the workspace landed on, or `None` if there is nowhere to go.
    pub fn cycle_workspace(&mut self, delta: i32) -> Option<u64> {
        let n = self.workspaces.len();
        if n < 2 {
            return None;
        }
        let cur = self.workspace_index() as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        let id = self.workspaces[next].id;
        // Say the direction rather than let it be inferred: wrapping from
        // the last workspace to the first is still forward motion.
        self.switch_workspace_toward(id, delta);
        Some(id)
    }

    /// Step along the workspace strip without wrapping. Sidebar swipes use
    /// this so pushing past either physical end leaves the current workspace
    /// in place.
    fn swipe_workspace(&mut self, delta: i32) -> Option<u64> {
        let next = self.workspace_index() as i32 + delta;
        if next < 0 || next >= self.workspaces.len() as i32 {
            return None;
        }
        let id = self.workspaces[next as usize].id;
        self.switch_workspace_toward(id, delta);
        Some(id)
    }
    pub fn name_of(&self, id: u64) -> Option<&str> {
        self.list
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.as_str())
    }
    pub fn default_cwd_of(&self, id: u64) -> Option<&str> {
        self.list
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.default_cwd.as_deref())
    }
    /// Set or clear a project's remembered default cwd. Marks the
    /// projects.json dirty so it flushes to disk on the next save tick.
    /// Returns true if the value actually changed.
    pub fn set_default_cwd(&mut self, id: u64, cwd: Option<String>) -> bool {
        for p in &mut self.list {
            if p.id == id {
                if p.default_cwd != cwd {
                    p.default_cwd = cwd;
                    self.dirty = true;
                    return true;
                }
                return false;
            }
        }
        false
    }
    /// Bump the unread bell counter for one project. Marks layout
    /// dirty so the sidebar redraws with the new badge.
    pub fn bump_unread(&mut self, project_id: u64) {
        *self.unread_bells.entry(project_id).or_insert(0) += 1;
        self.layout_dirty = true;
    }
    /// Clear a project's unread count. No-op if it's already zero.
    /// Returns true if anything actually changed (so the caller can
    /// decide whether to mark layout dirty).
    pub fn clear_unread(&mut self, project_id: u64) -> bool {
        match self.unread_bells.remove(&project_id) {
            Some(n) if n > 0 => {
                self.layout_dirty = true;
                true
            }
            _ => false,
        }
    }
    pub fn unread_total(&self) -> u64 {
        self.unread_bells.values().copied().sum()
    }
}

#[derive(Resource, Default)]
pub struct Renaming {
    pub id: Option<u64>,
    pub buffer: String,
    /// What `id` names. Project rows and the workspace header both edit
    /// inline in the sidebar and share one keyboard handler; only the
    /// commit target differs.
    pub target: RenameTarget,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenameTarget {
    #[default]
    Project,
    Workspace,
}

/// Side-channel for spawn / restore / open-file actions that need
/// exclusive World access (worker setup is `!Send`, spawn registration
/// goes through `PaneRegistry`). Pane closes are owned by pane-bevy's
/// own `PendingPaneActions`.
#[derive(Resource, Default)]
pub struct PendingActions {
    /// Spawn a new pane of any registered kind.
    pub new_panes: Vec<NewPaneRequest>,
    /// Restore persisted panes at startup. Each is dispatched to the
    /// registered kind's `spawn` callback with its saved config blob.
    pub restore_panes: Vec<PaneSnapshot>,
    /// Files to open into a new editor pane (Cmd+O picker, `tbopen`
    /// CLI, etc.).
    pub open_files: Vec<OpenFileRequest>,
    /// Close requests from `tbclose`: `(project_id, kind_filter)`. A
    /// `None` kind closes every pane in the project. Resolved to pane
    /// entities in `apply_pending_actions` (needs a world query).
    pub close_panes: Vec<(u64, Option<String>, Option<Vec<String>>)>,
    /// "Open Emacs here": `(project_id, root_dir)`. Spawns a file tree
    /// docked beside an Emacs pane — see `crate::open_emacs_workspace`.
    /// `None` root means the project's own directory.
    pub emacs_workspaces: Vec<(u64, Option<String>)>,
    /// `jimctl group assign|clear`: `(project_id, titles, group)`. `None`
    /// as the group clears membership. Applied by `apply_pane_group_sets`.
    pub set_pane_groups: Vec<(u64, Vec<String>, Option<String>)>,
    /// `jimctl move`: `(src_project_id, dest_project_id, kind, titles)`.
    /// Resolved to entities in `apply_pending_actions` (needs a world
    /// query), same as `close_panes`.
    pub move_panes: Vec<(u64, u64, Option<String>, Option<Vec<String>>)>,
    /// Dock requests from `jimctl dock`:
    /// `(project_id, titles, template, empty, slots)`. With `empty`, spawn
    /// a template skeleton of `slots` empty cells; otherwise dock the
    /// matched `titles` (empty titles = all free top-level panes).
    pub dock_panes: Vec<(u64, Vec<String>, Option<String>, bool, Option<usize>)>,
}

/// Request to spawn one new pane of a given kind.
#[derive(Debug, Clone)]
pub struct NewPaneRequest {
    pub kind: &'static str,
    pub project_id: u64,
    /// Optional window-space top-left for the new pane. `None` cascades
    /// from a default position based on how many panes the project has.
    pub origin: Option<Vec2>,
    /// Optional pixel size. `None` uses the kind's `default_size` from
    /// `PaneRegistry` (clamped to `MIN_PANE_SIZE`).
    pub size: Option<Vec2>,
    pub config: serde_json::Value,
}

/// Request to load a file into a new editor pane. Project is resolved
/// when the request is consumed — by then `Projects` is up to date.
#[derive(Debug, Clone)]
pub struct OpenFileRequest {
    pub path: PathBuf,
    pub project: OpenProjectTarget,
    /// Optional window-space top-left for the new pane. `None` means
    /// cascade from the default canvas position.
    pub origin: Option<Vec2>,
    /// Optional 1-based line to place the cursor on (and scroll to).
    pub line: Option<u32>,
    /// Optional 0-based column within `line`.
    pub column: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum OpenProjectTarget {
    /// Whichever project is currently active.
    Active,
    /// Project with the given id (no-op if it's been deleted).
    ById(u64),
    /// First project whose name matches case-insensitively (no-op if
    /// nothing matches). Used by the `tbopen --project NAME` flag.
    ByName(String),
}

/// Re-exported from `jim_pane` so call sites in this crate keep their
/// existing import paths.
pub use jim_pane::InputConsumed;

/// Sidebar geometry. Only the width is mutable; height + position are
/// driven by the window. Persisted as part of the projects file.
#[derive(Resource)]
pub struct Sidebar {
    pub width: f32,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self {
            width: SIDEBAR_DEFAULT_WIDTH,
        }
    }
}

/// Live drag state for the sidebar resize handle. `active` flips on
/// when the user mouse-downs in the handle hit area; `grab_offset_x`
/// stores `pt.x - sidebar.width` so the handle stays glued under the
/// cursor across drags. `dirty_pending` lets us batch the disk-save to
/// mouse-up instead of every drag tick.
#[derive(Resource, Default)]
struct SidebarResize {
    active: bool,
    grab_offset_x: f32,
    dirty_pending: bool,
}

// ---------- Components ----------

/// Project membership component. Aliased to `jim_pane::PaneProject` so
/// pane-bevy's visibility/persistence systems can read it directly.
pub type ProjectMembership = PaneProject;

#[derive(Component)]
pub struct SidebarEntity;

#[derive(Component, Copy, Clone, Debug)]
pub enum SidebarHit {
    Project(u64),
    DeleteProject(u64),
    /// Per-row eye column: parks / un-parks this project in the CURRENT
    /// workspace (see [`Projects::set_hidden`]).
    ToggleHidden(u64),
    NewProject,
    /// A bar in the header's workspace strip: switch to that workspace.
    Workspace(u64),
    /// The `+` at the end of the strip: add a workspace.
    NewWorkspace,
    /// The workspace name in the header: double-click renames it.
    WorkspaceName,
}

/// Bounds in window coords (top-left origin). Recomputed each frame so
/// resizing the window doesn't desync hit-tests from the visuals.
#[derive(Component, Copy, Clone, Debug)]
pub struct SidebarBounds {
    pub min: Vec2,
    pub max: Vec2,
}

/// Hover state that drives the reveal-on-hover eye affordances. Mouse
/// motion updates this; when it changes we mark the sidebar layout dirty
/// so the eye glyphs appear/disappear. Window coords, top-left origin.
#[derive(Resource, Default)]
struct SidebarHover {
    /// Project row currently under the cursor (per-row eye reveal).
    row: Option<u64>,
    /// Cursor is inside the bottom-left eyeball hot-zone.
    eyeball: bool,
}

/// Live state for dragging a project row to reorder it. `candidate` is
/// armed on press (before we know if it's a click or a drag); once the
/// cursor moves past `DRAG_THRESHOLD` the press becomes a real drag and
/// the row reorders live. `dirty_pending` batches the disk save to
/// mouse-up like the resize handle does.
#[derive(Resource, Default)]
struct ProjectDrag {
    candidate: Option<u64>,
    dragging: bool,
    press: Vec2,
    dirty_pending: bool,
}

// ----- The workspace slide -----
//
// Switching workspace pushes the whole list sideways: the one you left
// travels off one edge while the one you arrived at comes in from the
// other, the way a page turns. Anything less and a swipe just teleports
// the sidebar's contents, which reads as a glitch rather than a move.
//
// This only works because the sidebar owns a camera whose viewport is
// the sidebar rect (see [`SIDEBAR_LAYER`]); without that, content
// halfway through the slide would be painting across the canvas.

/// How long one workspace slide takes. Short — this is navigation
/// feedback, not a transition you are meant to watch.
const SLIDE_SECONDS: f32 = 0.22;

/// Dedicated RenderLayer for the sidebar.
///
/// The sidebar used to draw on layer 0, which meant nothing clipped it:
/// a long project name already spilled onto the canvas, and a sliding
/// list would have smeared right across it. Its own camera, viewport'd
/// to the sidebar rect, makes clipping the renderer's problem — the same
/// trick the panes use.
///
/// MUST be listed in `PanePlugin.reserved_layers` (see `lib.rs`), or the
/// pane allocator can hand the same id to a pane and that pane's content
/// would render inside the sidebar.
pub const SIDEBAR_LAYER: usize = 28;

/// Height of the footer band the list can never scroll into. Matches
/// [`EYE_ZONE`], because the footer exists to give that hot-zone a
/// surface of its own.
const FOOTER_H: f32 = EYE_ZONE;

// Depth ladder above `SIDEBAR_Z`, in draw order. The rows occupy
// 0.05–0.25 and scroll; everything below is chrome pinned over them.
/// Opaque band that hides rows scrolled down past the footer.
const Z_FOOTER_MASK: f32 = 0.26;
/// The hairline above the footer.
const Z_FOOTER_RULE: f32 = 0.27;
/// The show-hidden eyeball.
const Z_FOOTER_ITEM: f32 = 0.28;
/// Opaque band that hides rows scrolled up past the header.
const Z_HEADER_MASK: f32 = 0.30;
/// The hairline under the header.
const Z_HEADER_RULE: f32 = 0.32;
/// Workspace name (and its rename caret, just above).
const Z_HEADER_TEXT: f32 = 0.34;
/// Second mask, so the name sliding out of a workspace disappears behind
/// the strip rather than travelling across it.
const Z_STRIP_MASK: f32 = 0.36;
/// The workspace bars and the `+`.
const Z_STRIP: f32 = 0.38;
/// Scroll indicator, over everything — it reports on the list but is not
/// part of it.
const Z_SCROLLBAR: f32 = 0.40;

/// Width of the scroll indicator, hugging the sidebar's inner edge.
const SCROLLBAR_W: f32 = 3.0;
/// Shortest the thumb gets. Proportional sizing alone would shrink it to
/// a couple of pixels once the list is long enough to need it most.
const SCROLLBAR_MIN_H: f32 = 24.0;

/// Camera order for the sidebar. Above every pane camera (< 75_150) and
/// above the whiteboard overlay (80_000) — ink on the canvas must not
/// paint over the chrome — but below the recursive-slide dive (95_000)
/// and the menu overlay (100_000), both of which are meant to cover the
/// whole window, sidebar included.
pub const SIDEBAR_CAMERA_ORDER: isize = 90_000;

#[derive(Component)]
struct SidebarCamera;

/// Live state of the slide. `t` runs 0→1 over [`SLIDE_SECONDS`].
#[derive(Resource, Default)]
pub struct SidebarSlide {
    /// The workspace being left, and how far along we are.
    from: Option<u64>,
    /// +1 = moving forward through the strip (the new list enters from
    /// the right), -1 = backward.
    dir: f32,
    t: f32,
}

impl SidebarSlide {
    /// Is a slide in flight? The app is reactive, so this has to feed
    /// `want_continuous` or the slide would advance one frame per mouse
    /// twitch instead of playing.
    pub fn animating(&self) -> bool {
        self.from.is_some()
    }

    /// Eased progress. Ease-out cubic: the list arrives decelerating,
    /// which is what makes a short slide read as movement rather than a
    /// jump-cut.
    fn eased(&self) -> f32 {
        let t = self.t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }
}

/// Start a slide when a workspace change is queued, and advance one in
/// flight.
fn sidebar_slide(mut slide: ResMut<SidebarSlide>, mut projects: ResMut<Projects>, time: Res<Time>) {
    if let Some((from, dir)) = projects.pending_switch.take() {
        // Re-switching mid-slide restarts from where the eye is, not
        // from a fresh full-width offset, so a fast double swipe reads
        // as continuous motion.
        slide.from = Some(from);
        slide.dir = dir;
        slide.t = 0.0;
    }
    if slide.from.is_none() {
        return;
    }
    slide.t += time.delta_secs() / SLIDE_SECONDS;
    if slide.t >= 1.0 {
        slide.from = None;
        slide.t = 0.0;
    }
    // Every frame of the slide is a fresh layout: the two lists are
    // spawned at new offsets rather than tweened in place, because the
    // sidebar has always rebuilt wholesale and one moving mechanism is
    // cheaper to reason about than two.
    projects.layout_dirty = true;
}

/// Keep the sidebar camera's viewport glued to the sidebar rect.
///
/// Spawns it on the first run. The viewport maths is the pane cameras'
/// (`pane_camera_setup_for`), which means the world coordinates the
/// layout already computes land in exactly the same place they did on
/// layer 0 — the camera only decides where drawing stops.
fn sync_sidebar_camera(
    mut commands: Commands,
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    presentation: Res<crate::present::Presentation>,
    mut cam: Query<(&mut Camera, &mut Transform), With<SidebarCamera>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let rect = PaneRect {
        pos: Vec2::ZERO,
        size: Vec2::new(sidebar.width, window.height()),
        z: 0.0,
    };
    let setup = jim_pane::camera::pane_camera_setup_for(
        &rect,
        Vec2::new(window.width(), window.height()),
        window.scale_factor(),
        None,
    );
    // While presenting there is no sidebar; the entities are hidden, so
    // the camera has nothing to draw either way, but leaving a stale
    // viewport around is one less thing to wonder about.
    let visible = setup.visible && presentation.sidebar_visible();
    if let Ok((mut camera, mut transform)) = cam.single_mut() {
        let same_viewport = camera.viewport.as_ref().is_some_and(|v| {
            v.physical_position == setup.viewport.physical_position
                && v.physical_size == setup.viewport.physical_size
        });
        if !same_viewport {
            camera.viewport = Some(setup.viewport);
        }
        if camera.is_active != visible {
            camera.is_active = visible;
        }
        let want = Vec3::new(setup.cam_center.x, setup.cam_center.y, 0.0);
        if transform.translation != want {
            transform.translation = want;
        }
        return;
    }
    commands.spawn((
        Camera2d,
        Camera {
            order: SIDEBAR_CAMERA_ORDER,
            viewport: Some(setup.viewport),
            // The sidebar overlays the canvas render; clearing would
            // wipe everything drawn under it.
            clear_color: ClearColorConfig::None,
            is_active: visible,
            ..default()
        },
        // NOT `Msaa::Off`: this camera draws to the window, where every
        // other camera is at Bevy's default Sample4, and one straggler
        // at a different sample count is a fatal validation error.
        Transform::from_xyz(setup.cam_center.x, setup.cam_center.y, 0.0),
        RenderLayers::from_layers(&[SIDEBAR_LAYER]),
        SidebarCamera,
        Name::new("sidebar:camera"),
    ));
}

// ----- Scrolling the list -----

/// How far the list can travel: its height, less the room below the
/// header. Zero when everything already fits, which is the common case
/// and why nothing about the sidebar changes until it doesn't.
///
/// The "+ New Project" row and its divider scroll WITH the list rather
/// than pinning to the bottom. Pinning would move that row halfway down
/// an otherwise short sidebar, changing the layout for everyone to solve
/// a problem only long lists have.
fn max_scroll(rows: usize, win_h: f32) -> f32 {
    let content = rows as f32 * ROW_H + DIVIDER_H + ROW_H;
    (content - rows_room(win_h)).max(0.0)
}

/// Vertical room the list has: below the sticky header, above the
/// footer. Both bands are opaque and pinned, so this is the only part of
/// the sidebar a row can actually be seen or clicked in.
fn rows_room(win_h: f32) -> f32 {
    (win_h - HEADER_H - FOOTER_H).max(0.0)
}

/// Is this cursor y inside the band where rows are visible? Outside it
/// the row is behind the header or footer mask, and a click there must
/// not pick something the user cannot see.
fn in_rows_band(pt_y: f32, win_h: f32) -> bool {
    pt_y >= HEADER_H && pt_y < (win_h - FOOTER_H).max(HEADER_H)
}

/// Which visible row a cursor y is over.
///
/// Hover, press and reorder all map a cursor onto a row, and they have
/// to agree: if one of them forgets the scroll you highlight one project
/// and grab another.
fn row_slot_at(pt_y: f32, scroll: f32) -> i64 {
    ((pt_y - HEADER_H + scroll) / ROW_H).floor() as i64
}

/// How far each workspace's list is scrolled, in px from the top.
///
/// Per workspace, because they hold different numbers of projects and
/// coming back to one should find it where you left it. View state:
/// session-only, like `show_hidden`.
#[derive(Resource, Default)]
pub struct SidebarScroll {
    per_workspace: std::collections::HashMap<u64, f32>,
}

impl SidebarScroll {
    fn get(&self, workspace: u64) -> f32 {
        self.per_workspace.get(&workspace).copied().unwrap_or(0.0)
    }
    /// Read the offset clamped to what the list can actually travel.
    /// Clamping on read as well as on write means a window resize or a
    /// project deletion can't leave the list parked past its end.
    fn clamped(&self, workspace: u64, rows: usize, win_h: f32) -> f32 {
        self.get(workspace).clamp(0.0, max_scroll(rows, win_h))
    }
}

// ----- Two-finger workspace swipe -----

/// Horizontal pixels one gesture must travel before it switches
/// workspace. Low enough to feel like a flick, high enough that the
/// sideways wobble in a vertical scroll never trips it.
const SWIPE_THRESHOLD_PX: f32 = 55.0;
/// A gesture only counts as horizontal if it is this much more sideways
/// than vertical. Trackpads leak a little of each axis into the other,
/// and a vertical scroll that silently swapped the whole sidebar would
/// be the worst possible failure here.
const SWIPE_AXIS_RATIO: f32 = 1.6;
/// Travel before a gesture commits to an axis. Below this the direction
/// is mostly noise; above it, the choice sticks for the rest of the
/// gesture so a long scroll can drift sideways without switching
/// workspace, and a swipe can drift vertically without scrolling.
const AXIS_LOCK_PX: f32 = 6.0;
/// Quiet time that ends a gesture. A trackpad reports no "fingers
/// lifted", so the gap between event bursts is the only signal that one
/// swipe finished and the next began.
///
/// Measured in WALL CLOCK, not accumulated per frame. The app is
/// reactive: with nothing happening it renders every 5s, so a per-frame
/// accumulator counts "frames the app happened to run" rather than time,
/// and the gesture stays latched until something else wakes the loop.
/// That was the bug where you had to jiggle the mouse between swipes —
/// the jiggle was what let the app notice the gap.
const SWIPE_IDLE_SECS: f64 = 0.2;
/// Pixels attributed to one notch of a line-unit wheel. Only a mouse
/// with a horizontal tilt produces these; a couple of notches should
/// switch, same as a flick.
const SWIPE_LINE_PX: f32 = 30.0;

/// What a gesture turned out to be. Decided once, near its start, and
/// held until the gesture ends — see [`AXIS_LOCK_PX`].
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum GestureAxis {
    #[default]
    Undecided,
    /// Sideways: change workspace.
    Swipe,
    /// Up and down: scroll the list.
    Scroll,
}

/// Live state for a wheel gesture over the sidebar. See
/// [`sidebar_wheel`].
#[derive(Resource, Default)]
struct SidebarSwipe {
    /// What this gesture is for, once it has travelled far enough to
    /// say.
    axis: GestureAxis,
    /// Horizontal pixels accumulated within the current gesture.
    accum: f32,
    /// Vertical pixels in the same gesture, for the axis test.
    accum_y: f32,
    /// Set once this gesture has switched. One continuous swipe moves
    /// exactly ONE workspace however far it runs, so a long drag can't
    /// blow through five of them.
    fired: bool,
    /// `Time::elapsed_secs_f64` of the last wheel event, or `None`
    /// before the first one. A wall-clock stamp rather than a per-frame
    /// accumulator — see [`SWIPE_IDLE_SECS`].
    last_event: Option<f64>,
    /// Magnitude of the last horizontal wheel burst. Used to distinguish
    /// a fresh same-direction finger stroke from decaying momentum.
    last_dx_abs: f32,
    /// Consecutive, clearly shrinking bursts after a workspace switch.
    decay_bursts: u8,
    /// Once momentum is visibly decaying, a sharp increase means fingers
    /// touched down for another swipe, even in the same direction.
    tail_armed: bool,
}

impl SidebarSwipe {
    fn begin(&mut self) {
        self.accum = 0.0;
        self.accum_y = 0.0;
        self.fired = false;
        self.axis = GestureAxis::Undecided;
        self.last_dx_abs = 0.0;
        self.decay_bursts = 0;
        self.tail_armed = false;
    }

    /// Commit to an axis once the gesture has moved far enough to mean
    /// something. Returns what it settled on, or `Undecided` while it is
    /// still too small to call.
    fn decide_axis(&mut self) -> GestureAxis {
        if self.axis != GestureAxis::Undecided {
            return self.axis;
        }
        let (dx, dy) = (self.accum.abs(), self.accum_y.abs());
        self.axis = if dx >= AXIS_LOCK_PX && dx >= dy * SWIPE_AXIS_RATIO {
            GestureAxis::Swipe
        } else if dy >= AXIS_LOCK_PX {
            // Anything clearly vertical scrolls. The ratio guard is
            // deliberately one-sided: mistaking a scroll for a swipe
            // swaps the whole sidebar under the user, while mistaking a
            // swipe for a scroll just moves the list a little.
            GestureAxis::Scroll
        } else {
            GestureAxis::Undecided
        };
        self.axis
    }

    /// Does `dx` belong to a NEW gesture rather than the one in
    /// progress?
    ///
    /// This is what makes swiping back and forth work. macOS keeps
    /// delivering momentum events for up to a second after your fingers
    /// leave the trackpad, so the quiet gap that would otherwise end a
    /// gesture never arrives between two swipes made in quick
    /// succession. Momentum only ever decays in the direction of the
    /// flick that caused it, so a sign flip is always a real new gesture
    /// and never the tail of the old one.
    fn is_reversal(&self, dx: f32) -> bool {
        dx != 0.0 && self.accum != 0.0 && dx.signum() != self.accum.signum()
    }

    fn is_same_direction_restart(&self, dx: f32) -> bool {
        const RESTART_MIN_PX: f32 = 3.0;
        const RESTART_RATIO: f32 = 1.8;
        self.fired
            && self.tail_armed
            && dx.abs() >= RESTART_MIN_PX
            && dx.abs() >= self.last_dx_abs * RESTART_RATIO
    }

    fn observe_horizontal_burst(&mut self, dx: f32) {
        let magnitude = dx.abs();
        if self.fired {
            // Two substantial drops distinguish the momentum tail from the
            // acceleration at the beginning of one flick.
            if magnitude < self.last_dx_abs * 0.85 {
                self.decay_bursts = self.decay_bursts.saturating_add(1);
                self.tail_armed |= self.decay_bursts >= 2;
            } else if magnitude > self.last_dx_abs * 1.15 && !self.tail_armed {
                self.decay_bursts = 0;
            }
        }
        self.last_dx_abs = magnitude;
    }
}

/// Route a two-finger gesture over the sidebar: sideways changes
/// workspace, up and down scrolls the list.
///
/// One system rather than two, because the axis is a single decision per
/// gesture and both readings must not fire off the same flick. Trackpads
/// leak each axis into the other, so the gesture commits to one early
/// (see [`SidebarSwipe::decide_axis`]) and stays there.
///
/// Only over the sidebar: the canvas already owns wheel input elsewhere,
/// and a gesture that switched context from anywhere on screen would
/// fire by accident constantly.
///
/// Swipe direction matches the canvas pan and macOS paging — fingers
/// moving LEFT drag the next workspace in from the right.
fn sidebar_wheel(
    mut wheel: MessageReader<MouseWheel>,
    mut swipe: ResMut<SidebarSwipe>,
    mut scroll: ResMut<SidebarScroll>,
    time: Res<Time>,
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    presentation: Res<crate::present::Presentation>,
    keys: Res<ButtonInput<KeyCode>>,
    mut projects: ResMut<Projects>,
) {
    // No sidebar, no gesture — during a talk the strip down the left is
    // the slide, and swiping it must not switch context behind the
    // presenter's back.
    if !presentation.sidebar_visible() {
        wheel.clear();
        return;
    }
    // Cmd+wheel is the canvas pan. Leave it alone.
    if keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight) {
        wheel.clear();
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let over_sidebar = window
        .cursor_position()
        .is_some_and(|pt| pt.x < sidebar.width);

    let mut dx = 0.0;
    let mut dy = 0.0;
    let mut had_event = false;
    for ev in wheel.read() {
        let scale = match ev.unit {
            MouseScrollUnit::Line => SWIPE_LINE_PX,
            MouseScrollUnit::Pixel => 1.0,
        };
        dx += ev.x * scale;
        dy += ev.y * scale;
        had_event = true;
    }
    if !had_event {
        return;
    }
    let now = time.elapsed_secs_f64();
    let quiet = swipe.last_event.is_none_or(|t| now - t > SWIPE_IDLE_SECS);
    swipe.last_event = Some(now);

    // A direction reversal is unambiguous. For repeated swipes in the same
    // direction, a new finger stroke shows up as a sharp rebound after the
    // old stroke's momentum has begun decaying.
    if quiet || swipe.is_reversal(dx) || swipe.is_same_direction_restart(dx) {
        swipe.begin();
    }

    // A gesture that started off the sidebar accumulates nothing, so
    // dragging out of the sidebar mid-gesture abandons it rather than
    // completing it somewhere the user isn't looking.
    if !over_sidebar {
        return;
    }
    swipe.accum += dx;
    swipe.accum_y += dy;
    swipe.observe_horizontal_burst(dx);

    match swipe.decide_axis() {
        GestureAxis::Undecided => {}
        GestureAxis::Scroll => {
            let workspace = projects.active_workspace;
            let rows = sidebar_rows_for(&projects, workspace).len();
            // Positive y is fingers moving down, which under natural
            // scrolling drags the content down — towards the top of the
            // list, so the offset shrinks.
            let want = (scroll.get(workspace) - dy).clamp(0.0, max_scroll(rows, window.height()));
            if scroll.get(workspace) != want {
                scroll.per_workspace.insert(workspace, want);
                projects.layout_dirty = true;
            }
        }
        GestureAxis::Swipe => {
            // One switch per gesture, however far it runs.
            if swipe.fired || swipe.accum.abs() < SWIPE_THRESHOLD_PX {
                return;
            }
            // Positive x is fingers moving right — the same sign the
            // canvas pan reads as "show me what's to the left" — so that
            // goes back a workspace.
            let delta = if swipe.accum > 0.0 { -1 } else { 1 };
            if projects.swipe_workspace(delta).is_some() {
                swipe.fired = true;
            }
        }
    }
}

/// Bottom-left square that reveals + toggles the global "show hidden"
/// eyeball. Clamped to the sidebar width so it never spills onto the
/// canvas. Window coords, top-left origin.
fn eyeball_zone(win_h: f32, sidebar_width: f32) -> SidebarBounds {
    let w = EYE_ZONE.min(sidebar_width);
    SidebarBounds {
        min: Vec2::new(0.0, (win_h - EYE_ZONE).max(0.0)),
        max: Vec2::new(w, win_h),
    }
}

/// Geometry of the workspace switcher in the sidebar header.
///
/// Computed once and used for BOTH the sprites and the hit entities, so
/// the two can't drift apart the way they would if each did its own
/// arithmetic. Window coords, top-left origin.
struct WorkspaceStrip {
    /// `(workspace id, bar rect)` in swipe order, for the bars that fit.
    bars: Vec<(u64, SidebarBounds)>,
    /// The `+` button, dropped only if even it doesn't fit.
    add: Option<SidebarBounds>,
    /// Leftmost x the strip occupies. The name is truncated to end
    /// before this.
    left: f32,
}

fn workspace_strip(projects: &Projects, sidebar_width: f32) -> WorkspaceStrip {
    let inner_right = sidebar_width - DIVIDER_H - WS_STRIP_PAD_R;
    let full = |min_x: f32, w: f32| SidebarBounds {
        // Full header height: a 3px bar is a fine indicator and an
        // impossible click target.
        min: Vec2::new(min_x, 0.0),
        max: Vec2::new(min_x + w, HEADER_H),
    };
    let mut add_left = inner_right - WS_ADD_W;
    let add = if add_left > WS_NAME_MIN_W {
        Some(full(add_left, WS_ADD_W))
    } else {
        add_left = inner_right;
        None
    };

    let n = projects.workspaces.len();
    let avail = (add_left - WS_BAR_GAP - WS_NAME_MIN_W).max(0.0);
    let step = WS_BAR_W + WS_BAR_GAP;
    // How many bars fit; the last one needs no trailing gap.
    let fit = (((avail + WS_BAR_GAP) / step).floor() as usize).min(n);
    if fit == 0 {
        return WorkspaceStrip {
            bars: Vec::new(),
            add,
            left: add_left,
        };
    }
    // With more workspaces than bars, show a window centred on the
    // current one — dropping the bar you are standing on would make the
    // strip lie about where you are.
    let start = projects
        .workspace_index()
        .saturating_sub(fit / 2)
        .min(n - fit);
    let strip_w = fit as f32 * step - WS_BAR_GAP;
    let left = add_left - WS_BAR_GAP - strip_w;
    let bars = (0..fit)
        .map(|i| {
            (
                projects.workspaces[start + i].id,
                full(left + i as f32 * step, WS_BAR_W),
            )
        })
        .collect();
    WorkspaceStrip { bars, add, left }
}

impl SidebarBounds {
    /// Where this hit rect actually sits for `pass`.
    ///
    /// The departing list's rects are pushed out of reach rather than
    /// spawned in place: two live `SidebarHit::Project` entities naming
    /// different workspaces would make the picker's first-match-wins
    /// arbitrary, and the one that won might be the list on its way out.
    fn placed(self, pass: &SidebarPass) -> Self {
        if !pass.interactive {
            return Self {
                min: Vec2::splat(f32::INFINITY),
                max: Vec2::splat(f32::INFINITY),
            };
        }
        let d = Vec2::new(pass.dx, 0.0);
        Self {
            min: self.min + d,
            max: self.max + d,
        }
    }
}

fn in_bounds(pt: Vec2, b: &SidebarBounds) -> bool {
    pt.x >= b.min.x && pt.x <= b.max.x && pt.y >= b.min.y && pt.y <= b.max.y
}

// ---------- Plugin ----------

pub struct ProjectsPlugin;

impl Plugin for ProjectsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Projects::default())
            .insert_resource(Sidebar::default())
            .insert_resource(SidebarResize::default())
            .insert_resource(SidebarHover::default())
            .insert_resource(SidebarSwipe::default())
            .insert_resource(SidebarSlide::default())
            .insert_resource(SidebarScroll::default())
            .insert_resource(ProjectDrag::default())
            .insert_resource(Renaming::default())
            .insert_resource(PendingActions::default())
            .add_systems(
                Startup,
                load_or_seed_projects.after(jim_terminal::setup_terminal_font),
            )
            .add_systems(
                Update,
                (
                    sidebar_resize_drag,
                    project_drag,
                    sidebar_hover,
                    // Before the layout rebuild, so a workspace switch or
                    // a scroll repaints in the same frame as the gesture.
                    sidebar_wheel,
                    sidebar_slide,
                    sync_sidebar_camera,
                    sidebar_layout,
                    // After the layout rebuild (which respawns the entities)
                    // and before input, so a hidden sidebar is both unseen
                    // and unclickable in the same frame.
                    sync_sidebar_visibility,
                    sidebar_input,
                    rename_keyboard,
                    apply_pending_actions,
                    // Right after the frame's spawns: catch any pane that
                    // was created without a project before the rest of the
                    // pipeline (and the cube) relies on membership.
                    assert_pane_project_invariant,
                    sync_visibility,
                    refocus_on_project_change,
                    mark_terminals_dirty_on_change,
                    publish_live_terminals,
                    apply_inference_suggestions,
                    save_if_dirty,
                )
                    .chain(),
            );
        // PostUpdate reset for `InputConsumed` is owned by
        // `jim_editor::EditorEmbedPlugin`.
    }
}

fn load_or_seed_projects(mut commands: Commands, mut pending: ResMut<PendingActions>) {
    let persisted = load_persisted();
    let sidebar_width = persisted
        .sidebar_width
        .unwrap_or(SIDEBAR_DEFAULT_WIDTH)
        .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
    // Restore per-level canvas views (pan + zoom). Keys are
    // `"project:canvas"`; legacy saves stored a bare `"project"` (root
    // level), so accept both forms.
    let mut canvas_view = crate::canvas::CanvasView::default();
    for (k, v) in &persisted.canvas_views {
        let level = match k.split_once(':') {
            Some((p, c)) => match (p.parse::<u64>(), c.parse::<u64>()) {
                (Ok(p), Ok(c)) => Some((p, c)),
                _ => None,
            },
            None => k.parse::<u64>().ok().map(|p| (p, 0)),
        };
        if let Some(level) = level {
            let mut state = *v;
            state.clamp_zoom();
            canvas_view.per_level.insert(level, state);
        }
    }
    commands.insert_resource(canvas_view);
    let mut projects = Projects::from_persisted(persisted.clone());
    if projects.list.is_empty() {
        projects.create();
        projects.dirty = true;
    }
    // Enforce the active-is-switchable invariant at load: a persisted
    // `active` pointing at a now-parked project (or none at all) re-homes
    // to the first switchable one.
    let active_ok = projects
        .active
        .map(|a| !projects.is_hidden(a))
        .unwrap_or(false);
    if !active_ok {
        projects.active = projects.first_switchable();
    }

    // Queue restore for any pane whose project still exists. The
    // exclusive `apply_pending_actions` system spawns them on the
    // first Update tick. Old saves only had `terminals`; convert them
    // into PaneSnapshot form so the unified restore path handles them.
    let known_projects: std::collections::HashSet<u64> =
        projects.list.iter().map(|p| p.id).collect();
    for snap in persisted.panes {
        let belongs = snap
            .project_id
            .map(|p| known_projects.contains(&p))
            .unwrap_or(true);
        if belongs {
            pending.restore_panes.push(snap);
        }
    }
    for legacy in persisted.terminals {
        if !known_projects.contains(&legacy.project_id) {
            continue;
        }
        pending.restore_panes.push(PaneSnapshot {
            kind: "terminal".into(),
            project_id: Some(legacy.project_id),
            pos: legacy.pos,
            size: legacy.size,
            z: legacy.z,
            config: serde_json::json!({ "session_id": legacy.session_id }),
            pinned: false,
            canvas: 0,
            group: None,
            snap_id: 0,
            dock_group: None,
            dock_slot: 0,
        });
    }

    commands.insert_resource(projects);
    commands.insert_resource(Sidebar {
        width: sidebar_width,
    });
}

// ---------- Sidebar layout ----------

/// One list of projects to draw this frame.
///
/// At rest there is exactly one, sitting at `dx == 0`. During a slide
/// there are two, and everything that differs between them — which
/// projects, which is active, where it sits, whether it takes clicks —
/// is in here, so the drawing code below stays a single pass over rows.
struct SidebarPass {
    /// Indices into `Projects::list`, each with its parked flag *in this
    /// pass's workspace* (parked rows appear only under `show_hidden`).
    rows: Vec<(usize, bool)>,
    /// Workspace being shown, for the header name.
    workspace: u64,
    /// The active project in THIS workspace. The list being left keeps
    /// showing its own selection as it goes.
    active: Option<u64>,
    /// Horizontal offset in logical px.
    dx: f32,
    /// How far this list is scrolled, in px from the top.
    scroll: f32,
    /// Only the arriving list takes clicks. Clicking the departing one
    /// would act on a workspace you have already left.
    interactive: bool,
}

/// Rows to list for `workspace`, in `Projects::list` order.
fn sidebar_rows_for(projects: &Projects, workspace: u64) -> Vec<(usize, bool)> {
    let parked = projects
        .workspaces
        .iter()
        .find(|w| w.id == workspace)
        .map(|w| w.hidden.as_slice())
        .unwrap_or(&[]);
    (0..projects.list.len())
        .filter_map(|i| {
            let hidden = parked.contains(&projects.list[i].id);
            (projects.show_hidden || !hidden).then_some((i, hidden))
        })
        .collect()
}

fn sidebar_passes(
    projects: &Projects,
    slide: &SidebarSlide,
    scroll: &SidebarScroll,
    width: f32,
    win_h: f32,
) -> Vec<SidebarPass> {
    let rows = sidebar_rows_for(projects, projects.active_workspace);
    let arriving = SidebarPass {
        scroll: scroll.clamped(projects.active_workspace, rows.len(), win_h),
        rows,
        workspace: projects.active_workspace,
        active: projects.active,
        dx: 0.0,
        interactive: true,
    };
    let Some(from) = slide.from else {
        return vec![arriving];
    };
    // A full sidebar width of travel: the arriving list starts entirely
    // off the edge it is coming from, the departing one leaves by the
    // opposite edge. Anything less reads as a nudge rather than a page
    // turn, and the camera viewport means the overshoot costs nothing.
    let p = slide.eased();
    let leaving = sidebar_rows_for(projects, from);
    vec![
        SidebarPass {
            scroll: scroll.clamped(from, leaving.len(), win_h),
            rows: leaving,
            workspace: from,
            // `switch_workspace_toward` stamped the project we were in
            // onto the workspace we left, so it is still recorded there.
            active: projects
                .workspaces
                .iter()
                .find(|w| w.id == from)
                .and_then(|w| w.active),
            dx: -slide.dir * width * p,
            interactive: false,
        },
        SidebarPass {
            dx: slide.dir * width * (1.0 - p),
            ..arriving
        },
    ]
}

/// Clip `s` to what fits in `room` pixels at `advance` px per character,
/// marking the cut with an ellipsis. The sidebar font is monospace, so
/// character count is an exact width — no shaping needed.
fn truncate_to_width(s: &str, room: f32, advance: f32) -> String {
    if advance <= 0.0 {
        return s.to_string();
    }
    let fits = (room / advance).floor().max(0.0) as usize;
    if s.chars().count() <= fits {
        return s.to_string();
    }
    if fits <= 1 {
        return String::new();
    }
    s.chars()
        .take(fits - 1)
        .chain(std::iter::once('…'))
        .collect()
}

/// Window dims at the time of the last sidebar rebuild — when the
/// window resizes we must rebuild so the bg sprite + hit-test bounds
/// follow it.
#[derive(Default)]
struct LastWindowDims(Option<Vec2>);

/// Projects that owned at least one live terminal during the previous
/// sidebar layout pass. Keeping this separate from persisted project state
/// makes the presence marker follow actual ECS lifetime, including restores
/// and pane closes.
#[derive(Default)]
struct LastTerminalProjects(HashSet<u64>);

fn terminal_project_ids<'a>(
    panes: impl Iterator<Item = (&'a ProjectMembership, &'a PaneKindMarker)>,
) -> HashSet<u64> {
    panes
        .filter_map(|(membership, kind)| {
            (kind.0 == jim_terminal::PANE_KIND).then_some(membership.0)
        })
        .collect()
}

/// Rebuild the sidebar entity tree when project state, rename state, or
/// window size changes. Otherwise this system early-returns.
fn sidebar_layout(
    mut commands: Commands,
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    theme: Res<jim_style::Theme>,
    mut projects: ResMut<Projects>,
    slide: Res<SidebarSlide>,
    scroll: Res<SidebarScroll>,
    renaming: Res<Renaming>,
    hover: Res<SidebarHover>,
    drag: Res<ProjectDrag>,
    font: Res<MonoFont>,
    metrics: Res<MonoMetrics>,
    panes: Query<(&ProjectMembership, &PaneKindMarker)>,
    // Roots only. Each list is a parent with its rows as children, and
    // `despawn` takes the subtree — so despawning children too would
    // spam "entity is invalid" for every row on every rebuild.
    existing: Query<Entity, (With<SidebarEntity>, Without<ChildOf>)>,
    mut last_dims: Local<LastWindowDims>,
    mut last_terminal_projects: Local<LastTerminalProjects>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let win_w = window.width();
    let win_h = window.height();
    let width = sidebar.width;
    let dims = Vec2::new(win_w, win_h);
    let palette = sidebar_palette(&theme);
    let terminal_projects = terminal_project_ids(panes.iter());

    let dims_changed = last_dims.0 != Some(dims);
    let terminal_presence_changed = last_terminal_projects.0 != terminal_projects;
    let mut needs_rebuild =
        projects.layout_dirty || dims_changed || theme.is_changed() || terminal_presence_changed;
    if existing.iter().next().is_none() {
        needs_rebuild = true;
    }
    if !needs_rebuild {
        return;
    }
    last_dims.0 = Some(dims);
    last_terminal_projects.0 = terminal_projects.clone();
    for e in &existing {
        commands.entity(e).despawn();
    }

    // Sidebar lives at the LEFT edge. Window coords are top-left origin
    // (matching cursor_position), world coords have the camera at (0,0)
    // with y-up — so x=0 in window is x=-win_w/2 in world, y=0 in window
    // is y=win_h/2 in world.
    let sidebar_origin_x_window = 0.0;
    let world_left_edge = -win_w * 0.5;
    let world_top_edge = win_h * 0.5;

    // Chrome that does NOT slide — the masks, the header, the footer —
    // hangs off one root so `spawn_eye` and friends have a parent and
    // the despawn sweep has a single entity to take.
    // Everything the sidebar draws is on its own render layer, seen only
    // by a camera viewport'd to the sidebar rect. That is what lets the
    // lists below slide a full sidebar width without smearing across the
    // canvas — and it clips over-long project names too, which used to
    // spill.
    let layers = RenderLayers::from_layers(&[SIDEBAR_LAYER]);
    let root = commands
        .spawn((
            SidebarEntity,
            layers.clone(),
            Transform::default(),
            Visibility::default(),
            Name::new("sidebar:chrome"),
        ))
        .id();

    // Container bg — full height.
    commands.spawn((
        SidebarEntity,
        layers.clone(),
        Sprite {
            color: palette.bg,
            custom_size: Some(Vec2::new(width, win_h)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(world_left_edge, world_top_edge, SIDEBAR_Z),
    ));

    // Right-edge divider (1px) so the sidebar has a clean shoulder
    // against the canvas without needing a contrasting bg.
    commands.spawn((
        SidebarEntity,
        layers.clone(),
        Sprite {
            color: palette.divider,
            custom_size: Some(Vec2::new(DIVIDER_H, win_h)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(
            world_left_edge + width - DIVIDER_H,
            world_top_edge,
            SIDEBAR_Z + 0.05,
        ),
    ));

    let strip = workspace_strip(&projects, width);
    let renaming_workspace = renaming.target == RenameTarget::Workspace
        && renaming.id == Some(projects.active_workspace);

    // The lists on screen this frame. At rest that is one, at its
    // resting offset, and the loop below is the same code it always was.
    // Mid-slide it is two: the workspace being left travelling out and
    // the one arriving travelling in.
    let passes = sidebar_passes(&projects, &slide, &scroll, width, win_h);
    for pass in &passes {
        // One parent per list, so the slide is a single transform rather
        // than an offset threaded through every row's coordinates.
        let content = commands
            .spawn((
                SidebarEntity,
                layers.clone(),
                Transform::from_xyz(pass.dx, 0.0, 0.0),
                Visibility::default(),
                Name::new("sidebar:list"),
            ))
            .id();
        // Header — the current workspace's name on the left, the workspace
        // switcher strip on the right. The name replaces the old static
        // "PROJECTS" caption: with a swipe gesture that silently swaps the
        // whole list, a header that never changes is worse than no header.
        {
            let line_h = HEADER_FONT_SIZE * 1.4;
            let pad_y = ((HEADER_H - line_h) * 0.5).max(0.0);
            let advance = metrics.cell_width * (HEADER_FONT_SIZE / FONT_SIZE);
            let name_room = (strip.left - ROW_PAD_X - WS_BAR_GAP).max(0.0);
            // Only the arriving list can be mid-rename; the one on its
            // way out shows its committed name.
            let renaming_this_name = renaming_workspace && pass.interactive;
            let label = if renaming_this_name {
                renaming.buffer.clone()
            } else {
                let full = projects
                    .workspaces
                    .iter()
                    .find(|w| w.id == pass.workspace)
                    .map_or("", |w| w.name.as_str())
                    .to_uppercase();
                truncate_to_width(&full, name_room, advance)
            };
            commands.spawn((
                SidebarEntity,
                ChildOf(content),
                layers.clone(),
                Text2d::new(label.clone()),
                TextFont {
                    font: (font.0.clone()).into(),
                    font_size: FontSize::Px(HEADER_FONT_SIZE),
                    ..default()
                },
                LineHeight::Px(line_h),
                TextColor(if renaming_this_name {
                    palette.text
                } else {
                    palette.text_faint
                }),
                Anchor::TOP_LEFT,
                Transform::from_xyz(
                    world_left_edge + ROW_PAD_X,
                    world_top_edge - pad_y,
                    SIDEBAR_Z + Z_HEADER_TEXT,
                ),
            ));
            // Double-click target for renaming the workspace.
            commands.spawn((
                SidebarEntity,
                ChildOf(content),
                layers.clone(),
                Transform::from_xyz(world_left_edge, world_top_edge, SIDEBAR_Z + 0.05),
                SidebarHit::WorkspaceName,
                SidebarBounds {
                    min: Vec2::new(0.0, 0.0),
                    max: Vec2::new(strip.left.max(0.0), HEADER_H),
                }
                .placed(pass),
            ));
            if renaming_this_name {
                let caret_h = 14.0;
                commands.spawn((
                    SidebarEntity,
                    ChildOf(content),
                    layers.clone(),
                    Sprite {
                        color: palette.edit_underline,
                        custom_size: Some(Vec2::new(2.0, caret_h)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(
                        world_left_edge + ROW_PAD_X + label.chars().count() as f32 * advance,
                        world_top_edge - (HEADER_H - caret_h) * 0.5,
                        SIDEBAR_Z + Z_HEADER_TEXT + 0.01,
                    ),
                ));
            }
        }

        // Project rows. Hidden projects are skipped unless `show_hidden` is
        // on (then they show dimmed). We index into the *visible* sequence so
        // rows stay gap-free no matter how many projects are hidden.
        let rows_top_window = HEADER_H - pass.scroll;
        for (idx, &(li, hidden_this)) in pass.rows.iter().enumerate() {
            let proj = &projects.list[li];
            let row_top_window = rows_top_window + idx as f32 * ROW_H;
            let row_top_world = world_top_edge - row_top_window;
            let active = pass.active == Some(proj.id);
            let renaming_this =
                renaming.target == RenameTarget::Project && renaming.id == Some(proj.id);
            let dragging_this = drag.dragging && drag.candidate == Some(proj.id);
            let has_terminal = terminal_projects.contains(&proj.id);

            // Row bg — terminal presence is a barely deeper version of the
            // sidebar ground. Active, rename, and drag states remain stronger
            // and take precedence because they describe an immediate action.
            if active || renaming_this || dragging_this || has_terminal {
                let bg_color = if renaming_this {
                    palette.row_renaming_bg
                } else if active || dragging_this {
                    palette.row_active_bg
                } else {
                    palette.terminal_row_bg
                };
                commands.spawn((
                    SidebarEntity,
                    ChildOf(content),
                    layers.clone(),
                    Sprite {
                        color: bg_color,
                        custom_size: Some(Vec2::new(width - DIVIDER_H, ROW_H)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(world_left_edge, row_top_world, SIDEBAR_Z + 0.1),
                ));
            }

            // Active accent stripe (thin coloured bar on the left edge).
            if active {
                commands.spawn((
                    SidebarEntity,
                    ChildOf(content),
                    layers.clone(),
                    Sprite {
                        color: palette.active_stripe,
                        custom_size: Some(Vec2::new(STRIPE_W, ROW_H)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(world_left_edge, row_top_world, SIDEBAR_Z + 0.15),
                ));
            }

            // Renaming underline — a 2px accent strip along the bottom of
            // the row that reads as the cursor of a text input field.
            if renaming_this {
                commands.spawn((
                    SidebarEntity,
                    ChildOf(content),
                    layers.clone(),
                    Sprite {
                        color: palette.edit_underline,
                        custom_size: Some(Vec2::new(width - DIVIDER_H, 2.0)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(
                        world_left_edge,
                        row_top_world - (ROW_H - 2.0),
                        SIDEBAR_Z + 0.15,
                    ),
                ));
            }

            // Project pick hit-region — covers the row minus the delete glyph.
            commands.spawn((
                SidebarEntity,
                ChildOf(content),
                layers.clone(),
                Transform::from_xyz(world_left_edge, row_top_world, SIDEBAR_Z + 0.05),
                SidebarHit::Project(proj.id),
                SidebarBounds {
                    min: Vec2::new(sidebar_origin_x_window, row_top_window),
                    max: Vec2::new(
                        sidebar_origin_x_window + width - DELETE_W - EYE_W - DIVIDER_H,
                        row_top_window + ROW_H,
                    ),
                }
                .placed(pass),
            ));

            // Label.
            let label = if renaming_this {
                renaming.buffer.clone()
            } else {
                proj.name.clone()
            };
            let label_color = if active || renaming_this {
                palette.text
            } else if hidden_this {
                palette.text_faint
            } else {
                palette.text_dim
            };
            {
                let line_h = TEXT_FONT_SIZE * 1.4;
                let pad_y = ((ROW_H - line_h) * 0.5).max(0.0);
                commands.spawn((
                    SidebarEntity,
                    ChildOf(content),
                    layers.clone(),
                    Text2d::new(label),
                    TextFont {
                        font: (font.0.clone()).into(),
                        font_size: FontSize::Px(TEXT_FONT_SIZE),
                        ..default()
                    },
                    LineHeight::Px(line_h),
                    TextColor(label_color),
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(
                        world_left_edge + ROW_PAD_X,
                        row_top_world - pad_y,
                        SIDEBAR_Z + 0.2,
                    ),
                ));
            }

            // Caret — real sprite (not a U+2502 glyph, which renders too
            // low because box-drawing chars don't share the letter baseline).
            // Positioned via monospace cell-width scaled from FONT_SIZE→TEXT_FONT_SIZE,
            // and vertically centred in the row so it doesn't sit at the descender.
            if renaming_this {
                let char_advance = metrics.cell_width * (TEXT_FONT_SIZE / FONT_SIZE);
                let caret_w = 2.0;
                let caret_h = 16.0;
                let caret_x = world_left_edge
                    + ROW_PAD_X
                    + renaming.buffer.chars().count() as f32 * char_advance;
                let caret_top_y = row_top_world - (ROW_H - caret_h) * 0.5;
                commands.spawn((
                    SidebarEntity,
                    ChildOf(content),
                    layers.clone(),
                    Sprite {
                        color: palette.edit_underline,
                        custom_size: Some(Vec2::new(caret_w, caret_h)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(caret_x, caret_top_y, SIDEBAR_Z + 0.25),
                ));
            }

            // Unread bell badge — right-aligned just before the delete X
            // when the project has any unseen bells. Uses the active-stripe
            // colour so it reads as a "this needs attention" cue regardless
            // of which project is currently selected.
            if let Some(&n) = projects.unread_bells.get(&proj.id)
                && n > 0
            {
                let badge_text = if n > 99 {
                    "99+".to_string()
                } else {
                    n.to_string()
                };
                let badge_anchor_x_world = world_left_edge + width - DELETE_W - EYE_W - 4.0;
                {
                    let line_h = TEXT_FONT_SIZE * 1.4;
                    let pad_y = ((ROW_H - line_h) * 0.5).max(0.0);
                    commands.spawn((
                        SidebarEntity,
                        ChildOf(content),
                        layers.clone(),
                        Text2d::new(badge_text),
                        TextFont {
                            font: (font.0.clone()).into(),
                            font_size: FontSize::Px(TEXT_FONT_SIZE),
                            ..default()
                        },
                        LineHeight::Px(line_h),
                        TextColor(palette.active_stripe),
                        Anchor::TOP_RIGHT,
                        Transform::from_xyz(
                            badge_anchor_x_world,
                            row_top_world - pad_y,
                            SIDEBAR_Z + 0.2,
                        ),
                    ));
                }
            }

            // Delete glyph — just a dim × at the right edge of the row.
            // No filled background; the bounds are still a tappable rect.
            let delete_x_window = sidebar_origin_x_window + width - DELETE_W;
            let delete_x_world = world_left_edge + width - DELETE_W;
            commands.spawn((
                SidebarEntity,
                ChildOf(content),
                layers.clone(),
                Transform::from_xyz(delete_x_world, row_top_world, SIDEBAR_Z + 0.05),
                SidebarHit::DeleteProject(proj.id),
                SidebarBounds {
                    min: Vec2::new(delete_x_window, row_top_window),
                    max: Vec2::new(delete_x_window + DELETE_W, row_top_window + ROW_H),
                }
                .placed(pass),
            ));
            {
                let glyph_size = TEXT_FONT_SIZE + 1.0;
                let line_h = glyph_size * 1.4;
                let pad_y = ((ROW_H - line_h) * 0.5).max(0.0);
                commands.spawn((
                    SidebarEntity,
                    ChildOf(content),
                    layers.clone(),
                    Text2d::new("\u{00D7}"), // multiplication sign — looks better than ASCII 'x'
                    TextFont {
                        font: (font.0.clone()).into(),
                        font_size: FontSize::Px(glyph_size),
                        ..default()
                    },
                    LineHeight::Px(line_h),
                    TextColor(palette.text_faint),
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(
                        delete_x_world + 6.0,
                        row_top_world - pad_y,
                        SIDEBAR_Z + 0.2,
                    ),
                ));
            }

            // Hide/show eye column — just left of the delete glyph. The
            // hit-rect is always live, but the eyeball only paints while this
            // row is hovered, or while the project is already hidden (so a
            // hidden project still advertises a way to un-hide it). A pupil
            // (filled inner dot) means "visible / eye open"; a bare ring means
            // "hidden / eye closed".
            let eye_x_window = sidebar_origin_x_window + width - DELETE_W - EYE_W;
            commands.spawn((
                SidebarEntity,
                ChildOf(content),
                layers.clone(),
                Transform::from_xyz(
                    world_left_edge + width - DELETE_W - EYE_W,
                    row_top_world,
                    SIDEBAR_Z + 0.05,
                ),
                SidebarHit::ToggleHidden(proj.id),
                SidebarBounds {
                    min: Vec2::new(eye_x_window, row_top_window),
                    max: Vec2::new(eye_x_window + EYE_W, row_top_window + ROW_H),
                }
                .placed(pass),
            ));
            if hover.row == Some(proj.id) || hidden_this {
                let eye_color = if hidden_this {
                    palette.text_faint
                } else {
                    palette.text_dim
                };
                spawn_eye(
                    &mut commands,
                    content,
                    &layers,
                    &font.0,
                    Vec3::new(
                        world_left_edge + width - DELETE_W - EYE_W * 0.5,
                        row_top_world - ROW_H * 0.5,
                        SIDEBAR_Z + 0.2,
                    ),
                    !hidden_this,
                    eye_color,
                    TEXT_FONT_SIZE,
                );
            }
        }

        // Divider before the "+ New Project" row.
        let after_rows_window = rows_top_window + pass.rows.len() as f32 * ROW_H;
        commands.spawn((
            SidebarEntity,
            ChildOf(content),
            layers.clone(),
            Sprite {
                color: palette.divider,
                custom_size: Some(Vec2::new(width - DIVIDER_H, DIVIDER_H)),
                ..default()
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(
                world_left_edge,
                world_top_edge - after_rows_window,
                SIDEBAR_Z + 0.05,
            ),
        ));

        let new_proj_top_window = after_rows_window + DIVIDER_H;
        let new_proj_top_world = world_top_edge - new_proj_top_window;
        // "+ New Project" — same row style as project rows. Hit area only,
        // no painted bg until you hover (we skip hover for now).
        commands.spawn((
            SidebarEntity,
            ChildOf(content),
            layers.clone(),
            Transform::from_xyz(world_left_edge, new_proj_top_world, SIDEBAR_Z + 0.05),
            SidebarHit::NewProject,
            SidebarBounds {
                min: Vec2::new(sidebar_origin_x_window, new_proj_top_window),
                max: Vec2::new(
                    sidebar_origin_x_window + width - DIVIDER_H,
                    new_proj_top_window + ROW_H,
                ),
            }
            .placed(pass),
        ));
        {
            let line_h = TEXT_FONT_SIZE * 1.4;
            let pad_y = ((ROW_H - line_h) * 0.5).max(0.0);
            commands.spawn((
                SidebarEntity,
                ChildOf(content),
                layers.clone(),
                Text2d::new("+  New Project"),
                TextFont {
                    font: (font.0.clone()).into(),
                    font_size: FontSize::Px(TEXT_FONT_SIZE),
                    ..default()
                },
                LineHeight::Px(line_h),
                TextColor(palette.text_dim),
                Anchor::TOP_LEFT,
                Transform::from_xyz(
                    world_left_edge + ROW_PAD_X,
                    new_proj_top_world - pad_y,
                    SIDEBAR_Z + 0.2,
                ),
            ));
        }
    }

    // Footer. A band the list can never reach, holding the global
    // "show hidden projects" eyeball.
    //
    // The eyeball used to be a bare hot-zone over whatever row happened
    // to be in the bottom-left corner. Once the list scrolls that stops
    // working: there is always a row under it, so the corner eats a
    // click meant for a project and the eyeball itself is invisible
    // against the rows. Reserving the band is what makes both of them
    // reliably clickable.
    let footer_top = (win_h - FOOTER_H).max(HEADER_H);
    commands.spawn((
        SidebarEntity,
        layers.clone(),
        Sprite {
            color: palette.bg,
            custom_size: Some(Vec2::new(width - DIVIDER_H, win_h - footer_top)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(
            world_left_edge,
            world_top_edge - footer_top,
            SIDEBAR_Z + Z_FOOTER_MASK,
        ),
    ));
    commands.spawn((
        SidebarEntity,
        layers.clone(),
        Sprite {
            color: palette.divider,
            custom_size: Some(Vec2::new(width - DIVIDER_H, DIVIDER_H)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(
            world_left_edge,
            world_top_edge - footer_top,
            SIDEBAR_Z + Z_FOOTER_RULE,
        ),
    ));
    {
        // Always painted now that it has somewhere to live: an open
        // pupil in the accent colour means hidden projects are showing,
        // a bare dim ring means they are tucked away. It brightens on
        // hover rather than appearing from nothing.
        let zone = eyeball_zone(win_h, width);
        let color = if projects.show_hidden {
            palette.active_stripe
        } else if hover.eyeball {
            palette.text
        } else {
            palette.text_faint
        };
        spawn_eye(
            &mut commands,
            root,
            &layers,
            &font.0,
            Vec3::new(
                world_left_edge + (zone.min.x + zone.max.x) * 0.5,
                world_top_edge - (zone.min.y + zone.max.y) * 0.5,
                SIDEBAR_Z + Z_FOOTER_ITEM,
            ),
            projects.show_hidden,
            color,
            15.0,
        );
    }

    // Scroll indicator. Only drawn when the list actually overflows —
    // which is also the only time anyone needs telling that it scrolls,
    // and the reason this feature exists. Tracks the arriving list, so
    // mid-slide it already reports on where you are going.
    if let Some(pass) = passes.last() {
        let track_top = HEADER_H;
        let track_h = rows_room(win_h);
        let content_h = pass.rows.len() as f32 * ROW_H + DIVIDER_H + ROW_H;
        let travel = max_scroll(pass.rows.len(), win_h);
        if travel > 0.0 && content_h > 0.0 {
            let thumb_h = (track_h * (track_h / content_h)).clamp(SCROLLBAR_MIN_H, track_h);
            let progress = (pass.scroll / travel).clamp(0.0, 1.0);
            let thumb_top = track_top + (track_h - thumb_h) * progress;
            commands.spawn((
                SidebarEntity,
                layers.clone(),
                Sprite {
                    color: palette.text_faint,
                    custom_size: Some(Vec2::new(SCROLLBAR_W, thumb_h)),
                    ..default()
                },
                Anchor::TOP_LEFT,
                Transform::from_xyz(
                    world_left_edge + width - DIVIDER_H - SCROLLBAR_W - 1.0,
                    world_top_edge - thumb_top,
                    SIDEBAR_Z + Z_SCROLLBAR,
                ),
            ));
        }
    }

    // Sticky header. The rows scroll UNDER it rather than over it, so
    // an opaque band in the sidebar's own colour sits between the list
    // and the header's own contents. Invisible at rest — it is the same
    // flat colour as the background it covers.
    commands.spawn((
        SidebarEntity,
        layers.clone(),
        Sprite {
            color: palette.bg,
            custom_size: Some(Vec2::new(width - DIVIDER_H, HEADER_H)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(world_left_edge, world_top_edge, SIDEBAR_Z + Z_HEADER_MASK),
    ));

    // Mask behind the strip, in the sidebar's own colour. The workspace
    // name slides the full width of the sidebar and would otherwise
    // travel straight over the bars; this hides it a few pixels early
    // instead. Invisible at rest — the background it covers is the same
    // flat colour.
    if !strip.bars.is_empty() || strip.add.is_some() {
        let mask_left = strip.left - WS_BAR_GAP;
        commands.spawn((
            SidebarEntity,
            layers.clone(),
            Sprite {
                color: palette.bg,
                custom_size: Some(Vec2::new(
                    (width - DIVIDER_H - mask_left).max(0.0),
                    HEADER_H,
                )),
                ..default()
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(
                world_left_edge + mask_left,
                world_top_edge,
                SIDEBAR_Z + 0.26,
            ),
        ));
    }
    // Workspace strip. One bar per workspace, the current one lit —
    // clickable as a direct jump, and the readout for the swipe gesture.
    for (id, b) in &strip.bars {
        let current = *id == projects.active_workspace;
        commands.spawn((
            SidebarEntity,
            layers.clone(),
            Sprite {
                color: if current {
                    palette.active_stripe
                } else {
                    palette.text_faint
                },
                custom_size: Some(Vec2::new(WS_BAR_W, WS_BAR_H)),
                ..default()
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(
                world_left_edge + b.min.x,
                world_top_edge - (HEADER_H - WS_BAR_H) * 0.5,
                SIDEBAR_Z + Z_STRIP,
            ),
        ));
        commands.spawn((
            SidebarEntity,
            layers.clone(),
            Transform::from_xyz(world_left_edge + b.min.x, world_top_edge, SIDEBAR_Z + 0.05),
            SidebarHit::Workspace(*id),
            *b,
        ));
    }
    if let Some(b) = strip.add {
        commands.spawn((
            SidebarEntity,
            layers.clone(),
            Transform::from_xyz(world_left_edge + b.min.x, world_top_edge, SIDEBAR_Z + 0.05),
            SidebarHit::NewWorkspace,
            b,
        ));
        let glyph = HEADER_FONT_SIZE + 1.0;
        let line_h = glyph * 1.4;
        commands.spawn((
            SidebarEntity,
            layers.clone(),
            Text2d::new("+"),
            TextFont {
                font: (font.0.clone()).into(),
                font_size: FontSize::Px(glyph),
                ..default()
            },
            LineHeight::Px(line_h),
            TextColor(palette.text_faint),
            Anchor::TOP_LEFT,
            Transform::from_xyz(
                world_left_edge + b.min.x + 4.0,
                world_top_edge - ((HEADER_H - line_h) * 0.5).max(0.0),
                SIDEBAR_Z + Z_STRIP,
            ),
        ));
    }
    // Header divider.
    commands.spawn((
        SidebarEntity,
        layers.clone(),
        Sprite {
            color: palette.divider,
            custom_size: Some(Vec2::new(width - DIVIDER_H, DIVIDER_H)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(
            world_left_edge,
            world_top_edge - HEADER_H,
            SIDEBAR_Z + Z_HEADER_RULE,
        ),
    ));

    projects.layout_dirty = false;
}

/// Draw an "eyeball" out of two stacked Text2d glyphs (SF Mono lacks a
/// real eye glyph): a hollow ring `○` for the sclera, plus a smaller
/// filled `●` pupil when `open`. Centered on `center` (world coords,
/// `.z` is the base layer; the pupil sits just above it).
fn spawn_eye(
    commands: &mut Commands,
    parent: Entity,
    layers: &RenderLayers,
    font: &Handle<Font>,
    center: Vec3,
    open: bool,
    color: Color,
    outer_size: f32,
) {
    commands.spawn((
        SidebarEntity,
        ChildOf(parent),
        layers.clone(),
        Text2d::new("\u{25CB}"), // ○ white circle
        TextFont {
            font: (font.clone()).into(),
            font_size: FontSize::Px(outer_size),
            ..default()
        },
        TextColor(color),
        Anchor::CENTER,
        Transform::from_xyz(center.x, center.y, center.z),
    ));
    if open {
        commands.spawn((
            SidebarEntity,
            ChildOf(parent),
            layers.clone(),
            Text2d::new("\u{25CF}"), // ● black circle (pupil)
            TextFont {
                font: (font.clone()).into(),
                font_size: FontSize::Px(outer_size * 0.46),
                ..default()
            },
            TextColor(color),
            Anchor::CENTER,
            Transform::from_xyz(center.x, center.y, center.z + 0.05),
        ));
    }
}

// ---------- Sidebar input ----------

/// Tracks click state for double-click detection on project rows.
#[derive(Resource, Default)]
pub struct ClickTracker {
    last_project: Option<u64>,
    /// Set when the previous click landed on the workspace name, so the
    /// header gets its own double-click without borrowing a project's.
    last_workspace_name: bool,
    last_time: f64,
}

/// Show or hide the whole sidebar.
///
/// The sidebar is chrome: during a talk it must not sit down the left edge
/// of every slide, and on an `application:` slide it is only wanted when
/// the demo is about the app's own navigation. `SidebarEntity` is
/// despawned and respawned by `layout_sidebar` on every rebuild, so this
/// runs every frame rather than on change — it is a handful of writes.
pub fn sync_sidebar_visibility(
    presentation: Res<crate::present::Presentation>,
    mut sidebar_entities: Query<&mut Visibility, With<SidebarEntity>>,
) {
    let want = if presentation.sidebar_visible() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for mut vis in &mut sidebar_entities {
        if *vis != want {
            *vis = want;
        }
    }
}

pub fn sidebar_input(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    consumed: Res<InputConsumed>,
    presentation: Res<crate::present::Presentation>,
    sidebar: Res<Sidebar>,
    slide: Res<SidebarSlide>,
    time: Res<Time>,
    hits: Query<(&SidebarHit, &SidebarBounds)>,
    mut projects: ResMut<Projects>,
    mut renaming: ResMut<Renaming>,
    mut tracker: Local<ClickTracker>,
) {
    if consumed.0 {
        return;
    }
    // A hidden sidebar takes no clicks. Without this it stayed live at its
    // real window coordinates underneath a slide, eating presses in an
    // invisible strip — and on a slide showing the app, a stray hit would
    // switch project out from under the talk.
    if !presentation.sidebar_visible() {
        return;
    }
    // Nothing is clickable mid-slide. The rows are in motion, so whatever
    // is under the cursor at press time isn't what the user aimed at —
    // and it's only ~220ms.
    if slide.animating() {
        return;
    }
    if !buttons.just_pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(pt) = window.cursor_position() else {
        return;
    };
    // Sidebar is on the LEFT now — anything past `sidebar.width` is canvas.
    if pt.x >= sidebar.width {
        // Click hit the canvas — if we were renaming, commit on click-out.
        if renaming.id.is_some() {
            commit_rename(&mut projects, &mut renaming);
        }
        return;
    }

    // Global "show hidden" eyeball — handled by geometry (not a hit
    // entity) so it always wins over any project row that happens to sit
    // in the bottom-left corner.
    if in_bounds(pt, &eyeball_zone(window.height(), sidebar.width)) {
        if renaming.id.is_some() {
            commit_rename(&mut projects, &mut renaming);
        }
        projects.show_hidden = !projects.show_hidden;
        projects.layout_dirty = true;
        return;
    }

    // Pick the topmost hit. Only the row bgs / buttons carry SidebarHit
    // so we don't need z-sorting — they're disjoint by construction.
    let mut chosen: Option<SidebarHit> = None;
    for (hit, b) in &hits {
        if pt.x >= b.min.x && pt.x <= b.max.x && pt.y >= b.min.y && pt.y <= b.max.y {
            chosen = Some(*hit);
            break;
        }
    }
    let Some(hit) = chosen else {
        return;
    };

    match hit {
        SidebarHit::Project(id) => {
            // Double-click on the already-active project starts rename mode.
            let now = time.elapsed_secs_f64();
            let is_double = tracker.last_project == Some(id) && now - tracker.last_time < 0.4;
            tracker.last_project = Some(id);
            tracker.last_workspace_name = false;
            tracker.last_time = now;

            if is_double {
                let current = projects.name_of(id).unwrap_or("").to_string();
                renaming.id = Some(id);
                renaming.target = RenameTarget::Project;
                renaming.buffer = current;
            } else {
                if renaming.id.is_some() {
                    commit_rename(&mut projects, &mut renaming);
                }
                projects.set_active(id);
            }
        }
        SidebarHit::ToggleHidden(id) => {
            // Flip the project's hidden flag. Commit any in-flight rename
            // first so the click doesn't silently drop a typed name.
            if renaming.id.is_some() {
                commit_rename(&mut projects, &mut renaming);
            }
            projects.toggle_hidden(id);
        }
        SidebarHit::DeleteProject(id) => {
            // Drop the project from state. apply_pending_actions sweeps
            // any terminals whose ProjectMembership now points at a
            // missing project and shuts them down.
            projects.delete(id);
            if renaming.id == Some(id) {
                renaming.id = None;
                renaming.buffer.clear();
            }
        }
        SidebarHit::NewProject => {
            if renaming.id.is_some() {
                commit_rename(&mut projects, &mut renaming);
            }
            let id = projects.create();
            projects.set_active(id);
            // Open rename immediately so the user can name it. Start
            // empty so typing replaces the auto-generated "Project N"
            // instead of appending to it.
            renaming.id = Some(id);
            renaming.target = RenameTarget::Project;
            renaming.buffer.clear();
        }
        SidebarHit::Workspace(id) => {
            if renaming.id.is_some() {
                commit_rename(&mut projects, &mut renaming);
            }
            projects.switch_workspace(id);
        }
        SidebarHit::NewWorkspace => {
            if renaming.id.is_some() {
                commit_rename(&mut projects, &mut renaming);
            }
            let id = projects.create_workspace(None);
            // Straight into rename, like "+ New Project": an unnamed
            // workspace is much harder to tell apart than an unnamed
            // project, because only its name distinguishes it.
            renaming.id = Some(id);
            renaming.target = RenameTarget::Workspace;
            renaming.buffer.clear();
        }
        SidebarHit::WorkspaceName => {
            // Double-click the header to rename the workspace. A single
            // click does nothing: the header is not a target you should
            // be able to disturb by brushing past the list.
            let now = time.elapsed_secs_f64();
            let is_double = tracker.last_workspace_name && now - tracker.last_time < 0.4;
            tracker.last_project = None;
            tracker.last_workspace_name = true;
            tracker.last_time = now;
            if is_double {
                let current = projects.workspace_name().to_string();
                renaming.id = Some(projects.active_workspace);
                renaming.target = RenameTarget::Workspace;
                renaming.buffer = current;
            } else if renaming.id.is_some() {
                commit_rename(&mut projects, &mut renaming);
            }
        }
    }
}

fn commit_rename(projects: &mut Projects, renaming: &mut Renaming) {
    if let Some(id) = renaming.id.take() {
        let target = renaming.target;
        renaming.target = RenameTarget::Project;
        let mut name = std::mem::take(&mut renaming.buffer);
        let trimmed = name.trim();
        if trimmed.is_empty() {
            // Reject empty — keep the old name. Still need a redraw to
            // shed the caret + edit-state styling on the row.
            projects.layout_dirty = true;
        } else {
            name = trimmed.to_string();
            match target {
                RenameTarget::Project => projects.rename(id, name),
                RenameTarget::Workspace => projects.rename_workspace(id, name),
            }
        }
    }
}

// ---------- Rename keyboard ----------

fn rename_keyboard(
    mut events: MessageReader<KeyboardInput>,
    mut renaming: ResMut<Renaming>,
    mut projects: ResMut<Projects>,
) {
    if renaming.id.is_none() {
        return;
    }
    let mut changed = false;
    for ev in events.read() {
        if !ev.state.is_pressed() {
            continue;
        }
        match (&ev.key_code, &ev.logical_key) {
            (KeyCode::Enter, _) | (KeyCode::NumpadEnter, _) => {
                commit_rename(&mut projects, &mut renaming);
                return;
            }
            (KeyCode::Escape, _) => {
                renaming.id = None;
                renaming.buffer.clear();
                projects.layout_dirty = true;
                return;
            }
            (KeyCode::Backspace, _) => {
                renaming.buffer.pop();
                changed = true;
            }
            (_, Key::Character(s)) => {
                for c in s.chars() {
                    if !c.is_control() {
                        renaming.buffer.push(c);
                        changed = true;
                    }
                }
            }
            (_, Key::Space) => {
                renaming.buffer.push(' ');
                changed = true;
            }
            _ => {}
        }
    }
    if changed {
        projects.layout_dirty = true;
    }
}

// ---------- Apply pending actions ----------

/// Exclusive system — pane spawning and registry callbacks both need
/// `&mut World`. Restores first, then handles project-deletion sweeps,
/// then new-pane requests, then open-file requests.
fn apply_pending_actions(world: &mut World) {
    let _t_prof = jim_pane::prof::sys_span("apply_pending_actions");
    let actions = std::mem::take(&mut *world.resource_mut::<PendingActions>());
    for (project_id, root) in &actions.emacs_workspaces {
        crate::open_emacs_workspace(world, *project_id, root.clone());
    }
    let sidebar_width = world.resource::<Sidebar>().width;
    // Project the user is currently looking at; new panes spawned into it
    // follow the current scroll/pan instead of sitting near the origin.
    let active_project = world.resource::<Projects>().active;
    // Nested-canvas level the user is currently viewing in that project.
    // New panes spawned into the active project are gathered onto it, and
    // the cascade origin uses that level's pan.
    let active_canvas = active_project
        .map(|p| world.resource::<crate::canvas_pane::CanvasNav>().level(p))
        .unwrap_or(0);

    // Restore persisted panes first so they appear before any new ones
    // queued in the same frame.
    for snap in actions.restore_panes {
        restore_pane(world, snap);
        world.resource_mut::<Projects>().layout_dirty = true;
    }

    // Project deletion sweep: any pane whose project no longer exists
    // is queued for close (pane-bevy's apply_pending_pane_actions runs
    // the kind's on_close + despawns).
    let active_ids: std::collections::HashSet<u64> = world
        .resource::<Projects>()
        .list
        .iter()
        .map(|p| p.id)
        .collect();
    let orphans: Vec<Entity> = {
        let mut q = world.query::<(Entity, &PaneProject, &PaneTag)>();
        q.iter(world)
            .filter_map(|(e, m, _)| (!active_ids.contains(&m.0)).then_some(e))
            .collect()
    };
    if !orphans.is_empty() {
        let mut close_q = world.resource_mut::<jim_pane::PendingPaneActions>();
        for e in orphans {
            close_q.close.push(e);
        }
    }

    // `tbclose` requests: close panes in a project, optionally filtered
    // to a kind. Resolve to entities here (world query), then route
    // through the same close path as a close-button click.
    for (project_id, kind_filter, title_filter) in actions.close_panes {
        let targets: Vec<Entity> = {
            let mut q = world.query::<(
                Entity,
                &PaneProject,
                &PaneKindMarker,
                &jim_pane::PaneTitle,
                &PaneTag,
            )>();
            q.iter(world)
                .filter(|(_, m, _, _, _)| m.0 == project_id)
                .filter(|(_, _, k, _, _)| kind_filter.as_deref().map_or(true, |want| k.0 == want))
                .filter(|(_, _, _, t, _)| {
                    title_filter
                        .as_ref()
                        .map_or(true, |ts| ts.iter().any(|w| w == &t.0))
                })
                .map(|(e, _, _, _, _)| e)
                .collect()
        };
        if !targets.is_empty() {
            let mut close_q = world.resource_mut::<jim_pane::PendingPaneActions>();
            for e in targets {
                close_q.close.push(e);
            }
        }
    }

    // `jimctl move` requests: re-home panes into another project.
    // `PaneProject` is the whole of project membership — `sync_visibility`
    // reads it every frame and `mark_terminals_dirty_on_change` watches it
    // for persistence — so the move itself is one component write.
    //
    // Two components have to be reset along with it, because both gate
    // visibility RELATIVE to a project:
    //   * `PaneCanvas` — a nested-canvas level inside the source project.
    //     `sync_visibility` shows a pane only when its level equals the
    //     level its project is parked on, so a pane carrying level 3 into
    //     a project sitting at root is hidden in both. Land on root.
    //   * `PaneGroup` — a named group is only revealed per-project, so a
    //     moved pane in an unrevealed group would arrive invisible.
    // Getting either wrong looks exactly like "the move deleted my pane",
    // so they are cleared rather than carried.
    for (src_id, dest_id, kind_filter, title_filter) in actions.move_panes {
        if src_id == dest_id {
            eprintln!("[ipc] move_panes: source and destination are the same project");
            continue;
        }
        let (targets, docked): (Vec<Entity>, Vec<String>) = {
            let mut q = world.query::<(
                Entity,
                &PaneProject,
                &PaneKindMarker,
                &jim_pane::PaneTitle,
                Has<jim_pane::dock::DockMember>,
                Has<jim_pane::dock::Dock>,
                &PaneTag,
            )>();
            let matched: Vec<(Entity, String, bool)> = q
                .iter(world)
                .filter(|(_, m, _, _, _, _, _)| m.0 == src_id)
                .filter(|(_, _, k, _, _, _, _)| {
                    kind_filter.as_deref().is_none_or(|want| k.0 == want)
                })
                .filter(|(_, _, _, t, _, _, _)| {
                    title_filter
                        .as_ref()
                        .is_none_or(|ts| ts.iter().any(|w| w == &t.0))
                })
                .map(|(e, _, _, t, is_member, is_dock, _)| (e, t.0.clone(), is_member || is_dock))
                .collect();
            let mut ok = Vec::new();
            let mut bad = Vec::new();
            for (e, title, is_docked) in matched {
                // A dock drives its members' rects. Moving one out from
                // under its dock leaves both sides inconsistent, so refuse
                // loudly instead of half-doing it.
                if is_docked {
                    bad.push(title);
                } else {
                    ok.push(e);
                }
            }
            (ok, bad)
        };
        if !docked.is_empty() {
            eprintln!(
                "[ipc] move_panes: refusing to move docked pane(s) {docked:?} \
                 — undock them first (a dock owns its members' layout)"
            );
        }
        if targets.is_empty() {
            eprintln!("[ipc] move_panes: no panes matched");
            continue;
        }
        let moved = targets.len();
        for e in targets {
            let mut ent = world.entity_mut(e);
            ent.insert(PaneProject(dest_id));
            ent.insert(jim_pane::PaneCanvas(0));
            ent.remove::<jim_pane::PaneGroup>();
        }
        eprintln!("[ipc] move_panes: moved {moved} pane(s) {src_id} -> {dest_id}");
    }

    // `jimctl group assign|clear`: put panes into a named group (or take
    // them out). Membership is what makes a pane revealable by name later;
    // see `crate::pane_groups`. Titles are matched exactly, and an empty
    // title list never reaches here (the IPC handler rejects it) so a typo
    // can't silently regroup a whole project.
    for (project_id, titles, group) in actions.set_pane_groups {
        let targets: Vec<Entity> = {
            let mut q = world.query::<(Entity, &PaneProject, &jim_pane::PaneTitle, &PaneTag)>();
            q.iter(world)
                .filter(|(_, m, _, _)| m.0 == project_id)
                .filter(|(_, _, t, _)| titles.iter().any(|w| w == &t.0))
                .map(|(e, _, _, _)| e)
                .collect()
        };
        if targets.is_empty() {
            eprintln!("[groups] no panes matched titles {titles:?}");
        }
        for e in targets {
            crate::pane_groups::set_group(world, e, group.clone());
        }
    }

    // `jimctl dock` requests: frame existing panes into a new dock. Pick
    // the members (by title order, or all free panes when no titles), then
    // hand them to `create_dock` (the same path the snap gesture builds).
    for (project_id, titles, template, empty, slots) in actions.dock_panes {
        let tmpl = template
            .as_deref()
            .and_then(jim_pane::DockTemplate::from_str)
            .unwrap_or(jim_pane::DockTemplate::Columns);
        // Empty skeleton: spawn a template of empty slots to fill by drag.
        if empty {
            let count = pane_count_in_project(world, jim_pane::DOCK_KIND, project_id);
            let pos = cascade_pos(sidebar_width, count);
            jim_pane::create_template_skeleton(
                world,
                tmpl,
                slots,
                Some(project_id),
                pos,
                Vec2::new(760.0, 520.0),
            );
            world.resource_mut::<Projects>().terminals_dirty = true;
            continue;
        }
        let rows: Vec<(Entity, String, bool)> = {
            // (entity, title, free?) for panes in this project. "free" =
            // a real top-level pane, not a dock/member/pinned one.
            let mut q = world.query::<(
                Entity,
                &PaneProject,
                &jim_pane::PaneTitle,
                Has<jim_pane::Dock>,
                Has<jim_pane::DockMember>,
                Has<PanePinned>,
            )>();
            q.iter(world)
                .filter(|(_, m, _, _, _, _)| m.0 == project_id)
                .map(|(e, _, t, d, mem, pin)| (e, t.0.clone(), !d && !mem && !pin))
                .collect()
        };
        let members: Vec<Entity> = if titles.is_empty() {
            rows.iter()
                .filter(|(_, _, free)| *free)
                .map(|(e, _, _)| *e)
                .collect()
        } else {
            let mut picked: Vec<Entity> = Vec::new();
            for want in &titles {
                if let Some((e, _, _)) = rows
                    .iter()
                    .find(|(e, t, free)| *free && t == want && !picked.contains(e))
                {
                    picked.push(*e);
                }
            }
            picked
        };
        if members.len() >= 2 {
            let tmpl = template
                .as_deref()
                .and_then(jim_pane::DockTemplate::from_str)
                .unwrap_or(jim_pane::DockTemplate::Columns);
            if jim_pane::create_dock_template(world, &members, tmpl).is_some() {
                world.resource_mut::<Projects>().terminals_dirty = true;
            }
        } else {
            eprintln!(
                "[ipc] dock_panes: need >=2 matched free panes in project {project_id} (got {})",
                members.len()
            );
        }
    }

    // Spawn requested panes (radial menu, RunButton creation, etc.).
    for mut req in actions.new_panes {
        // Make the spawning project's root directory available to funct
        // widgets via `params.project_root`, so panes like the diff
        // widget default to the project's git tree instead of the GUI's
        // launch cwd. Only script widgets carry a `params` blob, and we
        // never clobber an explicit value the caller already supplied.
        if req.kind == jim_widget::script_widget::PANE_KIND {
            if let Some(root) = world
                .resource::<Projects>()
                .default_cwd_of(req.project_id)
                .map(str::to_owned)
            {
                if let serde_json::Value::Object(map) = &mut req.config {
                    let params = map
                        .entry("params")
                        .or_insert_with(|| serde_json::Value::Object(Default::default()));
                    if let serde_json::Value::Object(pmap) = params {
                        pmap.entry("project_root")
                            .or_insert(serde_json::Value::String(root));
                    }
                }
            }
        }
        let pos = req.origin.unwrap_or_else(|| {
            let count_in_project = pane_count_in_project(world, &req.kind, req.project_id);
            let base = cascade_pos(sidebar_width, count_in_project);
            // Pan is the canvas-space point at the screen origin. Adding it
            // keeps the pane in the visible region as the user scrolls, but
            // only for the project they're actually viewing — panes spawned
            // into a different project keep their origin-relative cascade.
            if active_project == Some(req.project_id) {
                base + world
                    .resource::<crate::canvas::CanvasView>()
                    .state_for((req.project_id, active_canvas))
                    .pan
            } else {
                base
            }
        });
        let size = req
            .size
            .unwrap_or_else(|| {
                world
                    .resource::<PaneRegistry>()
                    .get(req.kind)
                    .map(|s| s.default_size)
                    .unwrap_or(Vec2::new(560.0, 360.0))
            })
            .max(MIN_PANE_SIZE);
        let next_z = jim_pane::next_pane_z(world);
        let rect = PaneRect {
            pos,
            size,
            z: next_z,
        };
        if let Some(entity) = spawn_pane_from_registry(
            world,
            kind_to_static(req.kind),
            kind_display_name(world, req.kind),
            rect,
            Some(req.project_id),
            &req.config,
        ) {
            // Gather the new pane onto the canvas level the user is
            // viewing (root = no marker), but only when it lands in the
            // active project — panes pushed into another project go to
            // that project's root.
            if active_project == Some(req.project_id) && active_canvas != 0 {
                world
                    .entity_mut(entity)
                    .insert(jim_pane::PaneCanvas(active_canvas));
            }
            world.resource_mut::<FocusedPane>().0 = Some(entity);
            let mut projects = world.resource_mut::<Projects>();
            projects.layout_dirty = true;
            projects.terminals_dirty = true;
        }
    }

    // Open files into editor panes.
    for req in actions.open_files {
        let project_id = match resolve_project(&req.project, world.resource::<Projects>()) {
            Some(id) => id,
            None => {
                eprintln!(
                    "[open_file] no matching project for {:?}; dropping request for {}",
                    req.project,
                    req.path.display()
                );
                continue;
            }
        };
        let text = match std::fs::read_to_string(&req.path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[open_file] read {} failed: {}", req.path.display(), e);
                continue;
            }
        };
        let pos = req.origin.unwrap_or_else(|| {
            let count_in_project = pane_count_in_project(world, "editor", project_id);
            let base = cascade_pos(sidebar_width, count_in_project);
            if active_project == Some(project_id) {
                base + world
                    .resource::<crate::canvas::CanvasView>()
                    .state_for((project_id, active_canvas))
                    .pan
            } else {
                base
            }
        });
        let next_z = jim_pane::next_pane_z(world);
        let rect = PaneRect {
            pos,
            size: Vec2::new(720.0, 480.0),
            z: next_z,
        };
        let mut config = serde_json::json!({
            "text": text,
            "path": req.path.to_string_lossy(),
        });
        if let Some(line) = req.line {
            config["line"] = serde_json::json!(line);
        }
        if let Some(column) = req.column {
            config["column"] = serde_json::json!(column);
        }
        if let Some(entity) =
            spawn_pane_from_registry(world, "editor", "Editor", rect, Some(project_id), &config)
        {
            // EditorFilePath is also added by the editor's spawn callback
            // when the config carries `path`; we add it here too in case
            // future kinds want a different file-tagging convention.
            world
                .entity_mut(entity)
                .insert(EditorFilePath(req.path.clone()));
            if active_project == Some(project_id) && active_canvas != 0 {
                world
                    .entity_mut(entity)
                    .insert(jim_pane::PaneCanvas(active_canvas));
            }
            if world.resource::<Projects>().active == Some(project_id) {
                world.resource_mut::<FocusedPane>().0 = Some(entity);
            }
            world.resource_mut::<Projects>().layout_dirty = true;
        }
    }
}

/// Restore one persisted pane via the registry. Reserves any embedded
/// session id in the allocator so subsequent spawns don't collide.
fn restore_pane(world: &mut World, snap: PaneSnapshot) {
    if snap.kind == "terminal" {
        if let Some(id) = snap.config.get("session_id").and_then(|v| v.as_u64()) {
            let mut projects = world.resource_mut::<Projects>();
            if projects.next_terminal_id <= id {
                projects.next_terminal_id = id + 1;
            }
        }
    }
    // Project membership is a hard invariant: a pane without it leaks
    // across every project (never hidden by `sync_visibility`, no cube
    // face of its own). A snapshot with no `project_id` is an orphan we
    // must NOT resurrect as a floating pane — drop it loudly instead.
    let Some(project_id) = snap.project_id else {
        eprintln!(
            "[restore] dropping orphan {} pane: snapshot has no project_id. \
             Project membership is required; refusing to restore it across projects.",
            snap.kind
        );
        return;
    };
    let kind_static = kind_to_static(&snap.kind);
    let display = kind_display_name(world, &snap.kind);
    // PaneRect is canvas-space now — restore directly from the snapshot.
    let rect = PaneRect {
        pos: Vec2::new(snap.pos[0], snap.pos[1]),
        size: Vec2::new(snap.size[0], snap.size[1]),
        z: snap.z,
    };
    let entity = spawn_pane_from_registry(
        world,
        kind_static,
        display,
        rect,
        Some(project_id),
        &snap.config,
    );
    if let Some(e) = entity {
        // Reapply the pin marker if this pane was pinned at save time.
        if snap.pinned {
            world.entity_mut(e).insert(PanePinned);
        }
        // Restore nested-canvas membership. `0` is the project root
        // (no component needed); a non-zero id confines the pane to the
        // canvas tile that owns it until the user descends into it.
        if snap.canvas != 0 {
            world
                .entity_mut(e)
                .insert(jim_pane::PaneCanvas(snap.canvas));
        }
        // Restore named-group membership, so a deck's dashboards stay
        // wired up across restarts and revealing one is still just a
        // visibility flip (see `pane_groups`).
        if let Some(group) = snap.group.clone() {
            world.entity_mut(e).insert(jim_pane::PaneGroup(group));
        }
        // Restore the stable thumbnail id so the tile can find this
        // (now-hidden) pane's saved snapshot PNG.
        if snap.snap_id != 0 {
            world
                .entity_mut(e)
                .insert(jim_pane::PaneSnapId(snap.snap_id));
        }
        // Defer dock relinking: stamp the group id/slot so
        // `link_restored_docks` can rebuild Dock/DockMember once every
        // pane in the group has spawned (spawn order is unspecified).
        if let Some(group) = snap.dock_group {
            world.entity_mut(e).insert(jim_pane::PendingDockLink {
                group,
                slot: snap.dock_slot,
            });
        }
    }
}

/// Look the kind up in the registry to get its registered `kind`
/// `&'static str` (so callers can pass owned `String` and we still hand
/// pane-bevy a static slice). Falls back to leaking the input if the
/// kind isn't registered, so the spawn-from-registry call still finds
/// it stored on the entity for diagnostics.
pub(crate) fn kind_to_static(kind: &str) -> &'static str {
    match kind {
        "terminal" => "terminal",
        "editor" => "editor",
        "run-button" => "run-button",
        "script_widget" => "script_widget",
        "dock" => "dock",
        "canvas" => crate::canvas_pane::PANE_KIND,
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

fn kind_display_name(world: &World, kind: &str) -> String {
    world
        .resource::<PaneRegistry>()
        .get(kind)
        .map(|s| s.display_name.to_string())
        .unwrap_or_else(|| kind.to_string())
}

fn pane_count_in_project(world: &mut World, kind: &str, project_id: u64) -> usize {
    let mut q = world.query::<(&PaneProject, &PaneKindMarker)>();
    q.iter(world)
        .filter(|(m, k)| k.0 == kind && m.0 == project_id)
        .count()
}

fn cascade_pos(sidebar_width: f32, n: usize) -> Vec2 {
    Vec2::new(
        sidebar_width + 60.0 + (n as f32) * NEW_TERMINAL_OFFSET,
        60.0 + (n as f32) * NEW_TERMINAL_OFFSET,
    )
}

pub fn resolve_project(target: &OpenProjectTarget, projects: &Projects) -> Option<u64> {
    match target {
        OpenProjectTarget::Active => projects.active,
        OpenProjectTarget::ById(id) => projects.list.iter().any(|p| p.id == *id).then_some(*id),
        OpenProjectTarget::ByName(name) => projects
            .list
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .map(|p| p.id),
    }
}

/// Map a working directory to the project that owns it: the project
/// whose `default_cwd` is `cwd` or a parent of it. When several match
/// (nested roots), the longest `default_cwd` wins. `None` if no project
/// has a `default_cwd` that contains `cwd` — callers treat that as
/// "unscoped / global". Mirrors the "this project = cwd's project" rule.
pub fn project_for_cwd(cwd: &std::path::Path, projects: &Projects) -> Option<u64> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut best: Option<(usize, u64)> = None;
    for p in &projects.list {
        let Some(dc) = p.default_cwd.as_deref() else {
            continue;
        };
        let root = std::path::Path::new(dc);
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if cwd.starts_with(&root) {
            let depth = root.components().count();
            if best.map_or(true, |(d, _)| depth > d) {
                best = Some((depth, p.id));
            }
        }
    }
    best.map(|(_, id)| id)
}

// ---------- Visibility sync ----------

/// Enforce the invariant that every pane belongs to a project.
///
/// Project membership (`PaneProject`) is what confines a pane to one
/// project: `sync_visibility` only governs panes that have it, and the
/// cube buckets each pane onto its project's face by it. A pane WITHOUT
/// it is invisible to both — it is never hidden when you switch projects
/// (so it shows in every project) and it cannot be placed on a single
/// cube face. There is no legitimate way to create such a pane: every
/// spawn path supplies a project and `restore_pane` rejects orphan
/// snapshots. So this firing means a NEW spawn path forgot to tag its
/// pane — fail loud and immediately, at the source, rather than letting
/// it float across the overview where the cause is invisible.
pub fn assert_pane_project_invariant(
    orphans: Query<(Entity, &PaneKindMarker), (With<PaneTag>, Without<PaneProject>)>,
) {
    if let Some((entity, kind)) = orphans.iter().next() {
        panic!(
            "pane {entity:?} (kind {:?}) has no PaneProject. Project membership is a \
             hard invariant — every pane MUST be spawned with a project. Some spawn \
             path is creating panes without one; fix it to pass a project_id rather \
             than letting the pane leak across every project in the cube.",
            kind.0
        );
    }
}

/// Hide panes whose project is not the active one. Panes mid-close
/// (`PaneClosing`) are excluded: they were force-hidden by the close
/// and despawn at the start of next frame — flipping one back to
/// Inherited here would show it for its final frame.
pub fn sync_visibility(
    projects: Res<Projects>,
    nav: Res<crate::canvas_pane::CanvasNav>,
    groups: Res<crate::pane_groups::VisibleGroups>,
    presentation: Res<crate::present::Presentation>,
    mut panes: Query<
        (
            Entity,
            &PaneProject,
            Option<&jim_pane::PaneCanvas>,
            Option<&jim_pane::PaneGroup>,
            &mut Visibility,
        ),
        (With<PaneTag>, Without<jim_pane::PaneClosing>),
    >,
) {
    let active = projects.active;
    let presenting = presentation.active();
    for (entity, m, canvas, group, mut vis) in &mut panes {
        // The deck holding the window is chrome for the duration of a talk:
        // it is screen-anchored, covers the display, and must survive the
        // presenter switching projects inside the live view on the slide.
        // Without this, clicking the mirrored sidebar hid the very deck
        // doing the presenting.
        if presenting == Some(entity) {
            // A slide that hands the window to the real application does
            // NOT hide the deck. The deck gets out of the way by going back
            // to being an ordinary pane on the canvas (see
            // `present::apply_presentation`) — it keeps rendering, in its
            // own place, like any other pane.
            //
            // That is the whole point of such a slide: you are looking at
            // the real app, you find the pane this deck lives in, and it is
            // showing a slide of the app that contains it. Hiding the deck
            // removed the one thing the slide was about.
            let want = Visibility::Inherited;
            if *vis != want {
                *vis = want;
            }
            continue;
        }
        let pane_level = canvas.map_or(0, |c| c.0);
        // Third visibility dimension, orthogonal to project and canvas
        // level: a pane in a named group also needs that group revealed
        // (see `pane_groups`). Ungrouped panes are unaffected.
        let group_ok = group.is_none_or(|g| groups.is_visible(&g.0));
        let project_visible = Some(m.0) == active;
        // A pane shows only when it sits on the nested-canvas level its
        // own project is parked on (root = 0 / no marker) — descending
        // swaps the visible set without moving any pane. An embedded
        // project view is a window onto that project as you left it, so
        // it frames that project's level too, not a hardcoded root.
        let level_visible = pane_level == nav.level(m.0);
        let want = if project_visible && level_visible && group_ok {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *vis != want {
            *vis = want;
        }
    }
}

/// When the active project changes, move keyboard focus into the new
/// project — preferring a terminal at the top of its z-stack. Without
/// this, `FocusedPane` keeps pointing at a now-hidden pane in the old
/// project, so typing goes nowhere (or worse, into a hidden widget).
/// `handle_pane_mouse` already filters out hidden panes, so it's only
/// the residual state we have to fix here.
fn refocus_on_project_change(
    projects: Res<Projects>,
    mut last_active: Local<Option<u64>>,
    mut focused: ResMut<FocusedPane>,
    panes: Query<(Entity, &PaneProject, &PaneKindMarker, &PaneRect), With<PaneTag>>,
) {
    if *last_active == projects.active {
        return;
    }
    *last_active = projects.active;

    let Some(active) = projects.active else {
        focused.0 = None;
        return;
    };

    // If the current focus is already in the active project, leave it.
    if let Some(cur) = focused.0 {
        if let Ok((_, proj, _, _)) = panes.get(cur) {
            if proj.0 == active {
                return;
            }
        }
    }

    // Pick a candidate from the active project: prefer terminals, break
    // ties by topmost z so the visually-frontmost pane gets focus.
    let pick = panes
        .iter()
        .filter(|(_, p, _, _)| p.0 == active)
        .max_by(|a, b| {
            let a_term = a.2.0 == jim_terminal::PANE_KIND;
            let b_term = b.2.0 == jim_terminal::PANE_KIND;
            a_term.cmp(&b_term).then(
                a.3.z
                    .partial_cmp(&b.3.z)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        })
        .map(|(e, _, _, _)| e);

    focused.0 = pick;
}

// ---------- Live terminals export ----------
//
// `~/.jim/terminals.json` is a small "what's open right now"
// snapshot that out-of-process widgets (e.g. claude-context-bars) can
// poll to learn which terminal sessions are open, what project they
// belong to, and their pane title. The full `projects.json` is too
// big and only persisted on mouse-up; this file is updated within a
// frame of any relevant change.

#[derive(Serialize)]
struct LiveTerminalEntry {
    session_id: u64,
    project_id: u64,
    project_name: String,
    title: String,
}

#[derive(Serialize)]
struct LiveTerminals {
    terminals: Vec<LiveTerminalEntry>,
}

fn live_terminals_path() -> Option<PathBuf> {
    save_path().map(|d| d.join("terminals.json"))
}

fn write_live_terminals(state: &LiveTerminals) -> std::io::Result<()> {
    let Some(file) = live_terminals_path() else {
        return Err(std::io::Error::other("no HOME"));
    };
    let Some(dir) = file.parent() else {
        return Err(std::io::Error::other("no parent"));
    };
    fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec(state)?;
    let tmp = file.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &file)
}

/// Emit `~/.jim/terminals.json` whenever the set of open
/// terminals, their project assignment, their title, or the project
/// catalog changes. Hashes the snapshot to suppress redundant writes.
fn publish_live_terminals(
    projects: Res<Projects>,
    terminals: Query<(&TerminalSession, &PaneProject, &PaneTitle), With<PaneTag>>,
    mut last_hash: Local<u64>,
) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let name_of = |id: u64| -> String {
        projects
            .list
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };

    let mut entries: Vec<LiveTerminalEntry> = terminals
        .iter()
        .map(|(s, p, t)| LiveTerminalEntry {
            session_id: s.0,
            project_id: p.0,
            project_name: name_of(p.0),
            title: t.0.clone(),
        })
        .collect();
    entries.sort_by_key(|e| e.session_id);

    let mut hasher = DefaultHasher::new();
    for e in &entries {
        e.session_id.hash(&mut hasher);
        e.project_id.hash(&mut hasher);
        e.project_name.hash(&mut hasher);
        e.title.hash(&mut hasher);
    }
    let h = hasher.finish();
    if h == *last_hash {
        return;
    }
    *last_hash = h;

    if let Err(e) = write_live_terminals(&LiveTerminals { terminals: entries }) {
        eprintln!("[projects] write terminals.json: {}", e);
    }
}

// ---------- Inference consumer ----------

/// Confidence below which we don't auto-write the suggestion onto a
/// project. Picked conservatively — the model is small and the cost
/// of a wrong default (new terminals open in a stale directory) is
/// felt every spawn. Above the threshold we still only apply when
/// `good_default = true`.
const INFERENCE_AUTO_APPLY_THRESHOLD: f32 = 0.7;

/// Subscribes to `inference.project_default_cwd_suggested` events
/// from the bus and writes the verdict onto the owning project's
/// `default_cwd`. The owning project is resolved by:
///
///   `terminal_session_id` → `TerminalSession` component
///                         → `ProjectMembership` (== `PaneProject`)
///                         → project id
///
/// If the matching pane has no `ProjectMembership` (standalone test
/// pane) we drop the suggestion silently.
///
/// Only writes when `good_default = true` AND `confidence >=
/// INFERENCE_AUTO_APPLY_THRESHOLD`; the inferences pane still shows
/// every verdict so the user can see what was filtered out.
fn apply_inference_suggestions(
    mut events: MessageReader<claude_bus_bevy::ClaudeBusEvent>,
    panes: Query<(&jim_terminal::TerminalSession, Option<&ProjectMembership>)>,
    mut projects: ResMut<Projects>,
) {
    for ev in events.read() {
        if ev.kind != "inference.project_default_cwd_suggested" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<InferenceSuggestionPayload>(&ev.payload_json)
        else {
            continue;
        };
        if !payload.good_default || payload.confidence < INFERENCE_AUTO_APPLY_THRESHOLD {
            continue;
        }
        let Ok(sid) = ev.terminal_session_id.parse::<u64>() else {
            continue;
        };
        // Find the pane whose TerminalSession matches; read its project
        // membership. There's at most one matching pane (session ids
        // are unique).
        let project_id = panes
            .iter()
            .find(|(ts, _)| ts.0 == sid)
            .and_then(|(_, pm)| pm.map(|p| p.0));
        let Some(project_id) = project_id else {
            continue;
        };
        if projects.set_default_cwd(project_id, Some(payload.cwd.clone())) {
            info!(
                "[projects] project {} default_cwd ← {} (confidence {:.2})",
                project_id, payload.cwd, payload.confidence
            );
        }
    }
}

#[derive(serde::Deserialize)]
struct InferenceSuggestionPayload {
    good_default: bool,
    confidence: f32,
    cwd: String,
}

// ---------- Persistence flush ----------

fn save_if_dirty(world: &mut World) {
    // While Exposé is open the panes are displaced into a transient grid
    // (or animating back). Never persist that layout — panes always tween
    // back to their exact original rects, so once the grid closes the next
    // save writes the real positions.
    if world.resource::<crate::expose::Expose>().active {
        return;
    }
    let mouse_down = world
        .resource::<ButtonInput<MouseButton>>()
        .pressed(MouseButton::Left);
    if mouse_down {
        return;
    }
    {
        let projects = world.resource::<Projects>();
        if !projects.dirty && !projects.terminals_dirty {
            return;
        }
    }
    let panes = collect_pane_snapshots(world);
    let projects = world.resource::<Projects>();
    let sidebar_width = world.resource::<Sidebar>().width;
    let canvas_views: std::collections::HashMap<String, crate::canvas::CanvasViewState> = world
        .resource::<crate::canvas::CanvasView>()
        .per_level
        .iter()
        .map(|((p, c), v)| (format!("{p}:{c}"), *v))
        .collect();
    let snapshot = PersistedState {
        projects: projects.list.clone(),
        active: projects.active,
        next_id: projects.next_id,
        // Fold the live active project back into the workspace holding
        // it, so a save taken mid-session records where you actually
        // are — `switch_workspace` only writes that on the way out.
        workspaces: projects
            .workspaces
            .iter()
            .map(|w| WorkspaceData {
                active: if w.id == projects.active_workspace {
                    projects.active
                } else {
                    w.active
                },
                ..w.clone()
            })
            .collect(),
        active_workspace: Some(projects.active_workspace),
        next_workspace_id: projects.next_workspace_id,
        sidebar_width: Some(sidebar_width),
        terminals: Vec::new(),
        panes,
        next_terminal_id: projects.next_terminal_id,
        next_canvas_id: projects.next_canvas_id,
        next_snap_id: projects.next_snap_id,
        canvas_views,
    };
    save_persisted(&snapshot);
    let mut projects = world.resource_mut::<Projects>();
    projects.dirty = false;
    projects.terminals_dirty = false;
}

/// Walk every PaneTag entity, ask the registered kind for a snapshot,
/// and bundle them into a Vec<PaneSnapshot>. `PaneRect` is canvas-space
/// in the new model, so we just write its values directly.
fn collect_pane_snapshots(world: &mut World) -> Vec<PaneSnapshot> {
    // Map every docked pane (and each dock container) to its group id +
    // slot, so membership survives a restart. Group id = the dock
    // entity's bits; the container records itself at slot 0.
    let dock_info: std::collections::HashMap<Entity, (u64, usize)> = {
        let mut q = world.query::<(Entity, &jim_pane::Dock)>();
        let mut m = std::collections::HashMap::new();
        for (dock_e, dock) in q.iter(world) {
            let group = dock_e.to_bits();
            m.insert(dock_e, (group, 0usize));
            for (i, mem) in dock.member_entities().iter().enumerate() {
                m.insert(*mem, (group, i));
            }
        }
        m
    };
    let entries: Vec<(
        Entity,
        String,
        Option<u64>,
        PaneRect,
        bool,
        u64,
        u64,
        Option<String>,
    )> = {
        let mut q = world.query::<(
            Entity,
            &PaneKindMarker,
            Option<&PaneProject>,
            &PaneRect,
            Has<PanePinned>,
            Option<&jim_pane::PaneCanvas>,
            Option<&jim_pane::PaneSnapId>,
            Option<&jim_pane::PaneGroup>,
        )>();
        q.iter(world)
            .map(|(e, k, p, r, pinned, canvas, snap, group)| {
                (
                    e,
                    k.0.to_string(),
                    p.map(|p| p.0),
                    *r,
                    pinned,
                    canvas.map_or(0, |c| c.0),
                    snap.map_or(0, |s| s.0),
                    group.map(|g| g.0.clone()),
                )
            })
            .collect()
    };
    let snapshots: Vec<PaneSnapshot> = entries
        .into_iter()
        .filter_map(
            |(entity, kind, project_id, rect, pinned, canvas, snap_id, group)| {
                let snap_fn = world
                    .resource::<PaneRegistry>()
                    .get(&kind)
                    .map(|s| s.snapshot)?;
                let config = (snap_fn)(world, entity);
                let (dock_group, dock_slot) = dock_info
                    .get(&entity)
                    .map(|(g, s)| (Some(*g), *s))
                    .unwrap_or((None, 0));
                Some(PaneSnapshot {
                    kind,
                    project_id,
                    pos: [rect.pos.x, rect.pos.y],
                    size: [rect.size.x, rect.size.y],
                    z: rect.z,
                    config,
                    pinned,
                    canvas,
                    snap_id,
                    dock_group,
                    dock_slot,
                    group,
                })
            },
        )
        .collect();
    snapshots
}

/// Mark the persisted layout dirty whenever any pane's rect or
/// project membership changes. Save itself is debounced to mouse-up by
/// `save_if_dirty`.
fn mark_terminals_dirty_on_change(
    rect_changed: Query<
        (),
        (
            With<PaneTag>,
            Or<(
                Changed<PaneRect>,
                Changed<PaneProject>,
                Changed<jim_pane::PaneCanvas>,
            )>,
        ),
    >,
    pin_added: Query<(), Added<PanePinned>>,
    mut pin_removed: RemovedComponents<PanePinned>,
    mut projects: ResMut<Projects>,
) {
    if !rect_changed.is_empty() || !pin_added.is_empty() || pin_removed.read().next().is_some() {
        projects.terminals_dirty = true;
    }
}

/// Click + drag inside the resize hit-strip on the sidebar's right
/// edge to resize. Live-updates `Sidebar.width` and triggers a layout
/// rebuild; defers the disk save until mouse-up so we don't write a
/// hundred files during one drag.
fn sidebar_resize_drag(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut sidebar: ResMut<Sidebar>,
    mut resize: ResMut<SidebarResize>,
    mut consumed: ResMut<InputConsumed>,
    mut projects: ResMut<Projects>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(pt) = window.cursor_position() else {
        return;
    };

    if buttons.just_released(MouseButton::Left) && resize.active {
        resize.active = false;
        if resize.dirty_pending {
            resize.dirty_pending = false;
            // Reuse the project save channel — both project state and
            // sidebar width live in the same JSON file.
            projects.dirty = true;
        }
    }

    let in_handle =
        pt.x >= sidebar.width - SIDEBAR_RESIZE_HALF && pt.x <= sidebar.width + SIDEBAR_RESIZE_HALF;

    if buttons.just_pressed(MouseButton::Left) && in_handle && !resize.active {
        resize.active = true;
        resize.grab_offset_x = pt.x - sidebar.width;
        consumed.0 = true;
        return;
    }

    if resize.active && buttons.pressed(MouseButton::Left) {
        let new_width = (pt.x - resize.grab_offset_x).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        if (new_width - sidebar.width).abs() > 0.5 {
            sidebar.width = new_width;
            projects.layout_dirty = true;
            resize.dirty_pending = true;
        }
        consumed.0 = true;
    }
}

/// Track which project row / the bottom-left eyeball corner the cursor is
/// over, so the reveal-on-hover eye affordances can appear. Marks the
/// sidebar layout dirty only when the hover target actually changes, so a
/// resting cursor doesn't churn the entity tree.
fn sidebar_hover(
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    scroll: Res<SidebarScroll>,
    mut hover: ResMut<SidebarHover>,
    mut projects: ResMut<Projects>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let scrolled = scroll.clamped(
        projects.active_workspace,
        projects.sidebar_ids().len(),
        window.height(),
    );
    let mut new_row = None;
    let mut new_eyeball = false;
    if let Some(pt) = window.cursor_position() {
        if pt.x < sidebar.width {
            if in_rows_band(pt.y, window.height()) {
                let visible = projects.sidebar_ids();
                let slot = row_slot_at(pt.y, scrolled);
                if slot >= 0 && (slot as usize) < visible.len() {
                    new_row = Some(visible[slot as usize]);
                }
            }
            new_eyeball = in_bounds(pt, &eyeball_zone(window.height(), sidebar.width));
        }
    }
    if hover.row != new_row || hover.eyeball != new_eyeball {
        hover.row = new_row;
        hover.eyeball = new_eyeball;
        projects.layout_dirty = true;
    }
}

/// Drag a project row up/down to reorder it. A press in the row's body
/// (left of the eye/delete columns) arms a candidate without consuming
/// the click, so a plain click still selects/renames; once the cursor
/// moves past `DRAG_THRESHOLD` the press becomes a drag and the list
/// reorders live under the cursor. Persists on mouse-up, mirroring the
/// resize handle's debounce.
fn project_drag(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    sidebar: Res<Sidebar>,
    scroll: Res<SidebarScroll>,
    mut drag: ResMut<ProjectDrag>,
    mut projects: ResMut<Projects>,
    mut consumed: ResMut<InputConsumed>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let scrolled = scroll.clamped(
        projects.active_workspace,
        projects.sidebar_ids().len(),
        window.height(),
    );

    if buttons.just_released(MouseButton::Left) {
        if drag.dragging && drag.dirty_pending {
            // Reuse the project save channel — order lives in the same file.
            projects.dirty = true;
        }
        drag.candidate = None;
        drag.dragging = false;
        drag.dirty_pending = false;
        return;
    }

    let Some(pt) = window.cursor_position() else {
        return;
    };

    if buttons.just_pressed(MouseButton::Left) {
        drag.candidate = None;
        drag.dragging = false;
        // Only the row body arms a drag — not the eye/delete columns, the
        // resize handle, or the bottom-left eyeball corner.
        let in_row_body =
            pt.x < sidebar.width - DELETE_W - EYE_W && in_rows_band(pt.y, window.height());
        if in_row_body {
            let visible = projects.sidebar_ids();
            let slot = row_slot_at(pt.y, scrolled);
            if slot >= 0 && (slot as usize) < visible.len() {
                drag.candidate = Some(visible[slot as usize]);
                drag.press = pt;
            }
        }
        return;
    }

    if buttons.pressed(MouseButton::Left) {
        let Some(id) = drag.candidate else {
            return;
        };
        if !drag.dragging && (pt - drag.press).length() < DRAG_THRESHOLD {
            return;
        }
        drag.dragging = true;
        consumed.0 = true;

        let visible_len = projects.sidebar_ids().len();
        if visible_len == 0 {
            return;
        }
        let target_slot = row_slot_at(pt.y, scrolled).clamp(0, visible_len as i64 - 1) as usize;
        if reorder_visible(&mut projects, id, target_slot) {
            projects.layout_dirty = true;
            drag.dirty_pending = true;
        }
    }
}

/// Move project `id` to visible slot `target_slot` (0-based among the
/// currently-visible rows) by stepping it past one visible neighbour at a
/// time. Swapping adjacent *visible* entries leaves any interleaved hidden
/// projects pinned in place. Returns true if the order changed.
fn reorder_visible(projects: &mut Projects, id: u64, target_slot: usize) -> bool {
    let mut changed = false;
    loop {
        let order: Vec<usize> = (0..projects.list.len())
            .filter(|&i| projects.show_hidden || !projects.is_hidden(projects.list[i].id))
            .collect();
        let Some(cur) = order.iter().position(|&i| projects.list[i].id == id) else {
            return changed;
        };
        let tgt = target_slot.min(order.len().saturating_sub(1));
        if cur == tgt {
            return changed;
        }
        if cur < tgt {
            projects.list.swap(order[cur], order[cur + 1]);
        } else {
            projects.list.swap(order[cur], order[cur - 1]);
        }
        changed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_presence_uses_only_real_terminal_panes_and_deduplicates_projects() {
        let memberships = [
            PaneProject(7),
            PaneProject(7),
            PaneProject(11),
            PaneProject(13),
        ];
        let kinds = [
            PaneKindMarker(jim_terminal::PANE_KIND),
            PaneKindMarker(jim_terminal::PANE_KIND),
            PaneKindMarker(jim_terminal::PANE_KIND),
            PaneKindMarker("emacs"),
        ];
        let ids = terminal_project_ids(memberships.iter().zip(kinds.iter()));

        assert_eq!(ids, HashSet::from([7, 11]));
    }

    /// A `Projects` with `n` projects and one workspace, as
    /// `load_or_seed_projects` would leave it.
    fn seeded(n: usize) -> Projects {
        let mut p = Projects {
            next_id: 1,
            next_workspace_id: 1,
            ..Default::default()
        };
        p.create_workspace(Some(DEFAULT_WORKSPACE_NAME.into()));
        for _ in 0..n {
            p.create();
        }
        p
    }

    /// Upgrading must not change what the sidebar shows. An old save has
    /// no workspaces and carries its parked projects as per-project
    /// flags; those flags ARE a sidebar configuration, just the only one
    /// that existed.
    ///
    /// Goes through real JSON on purpose. Building `ProjectData` in Rust
    /// tests the migration but not the field name it reads from, and
    /// that is exactly what broke first: renaming the field to
    /// `legacy_hidden` renamed the serde key with it, so every save
    /// migrated to "nothing parked" while this test stayed green.
    #[test]
    fn an_old_save_migrates_its_hidden_flags_into_one_workspace() {
        let raw = serde_json::json!({
            "projects": [
                { "id": 1, "name": "p1" },
                { "id": 2, "name": "p2", "hidden": true },
                { "id": 3, "name": "p3", "hidden": false },
            ],
            "active": 3,
            "next_id": 4,
        });
        let persisted: PersistedState =
            serde_json::from_value(raw).expect("an old save must still parse");
        assert!(
            persisted.projects[1].legacy_hidden,
            "the on-disk key is `hidden`, whatever the Rust field is called"
        );
        let projects = Projects::from_persisted(persisted);
        assert_eq!(projects.workspaces.len(), 1);
        assert_eq!(projects.workspace_name(), DEFAULT_WORKSPACE_NAME);
        assert!(projects.is_hidden(2), "the parked project stays parked");
        assert!(!projects.is_hidden(1));
        assert_eq!(projects.switchable_ids(), vec![1, 3]);
        assert_eq!(projects.active, Some(3));
    }

    /// The point of the whole feature: the same project can be listed in
    /// one workspace and tucked away in the next, without either
    /// workspace's choice leaking into the other.
    #[test]
    fn parking_is_per_workspace() {
        let mut p = seeded(3);
        let first = p.active_workspace;
        p.set_hidden(2, true);
        let second = p.create_workspace(Some("Other".into()));

        // A new workspace forks the current one, so it inherits the park.
        assert!(p.is_hidden(2));
        p.set_hidden(2, false);
        p.set_hidden(3, true);
        assert_eq!(p.switchable_ids(), vec![1, 2]);

        p.switch_workspace(first);
        assert!(p.is_hidden(2), "the first workspace kept its own parking");
        assert!(!p.is_hidden(3), "and never saw the second one's");
        assert_eq!(p.switchable_ids(), vec![1, 3]);

        p.switch_workspace(second);
        assert_eq!(p.switchable_ids(), vec![1, 2]);
    }

    /// Swiping away and back has to land where you were, or a workspace
    /// is just a filter rather than a place.
    #[test]
    fn a_workspace_remembers_the_project_you_left_it_in() {
        let mut p = seeded(3);
        let first = p.active_workspace;
        p.set_active(3);
        let second = p.create_workspace(None);
        p.set_active(1);

        p.switch_workspace(first);
        assert_eq!(p.active, Some(3));
        p.switch_workspace(second);
        assert_eq!(p.active, Some(1));
    }

    /// The active-is-switchable invariant has to survive a workspace
    /// change too, where the SET of switchable projects moves under
    /// `active` rather than the other way round.
    #[test]
    fn switching_re_homes_an_active_project_parked_over_there() {
        let mut p = seeded(3);
        let first = p.active_workspace;
        p.set_active(3);
        let second = p.create_workspace(None);
        // Park the project the other workspace is sitting in.
        p.set_hidden(3, true);
        p.switch_workspace(first);
        p.set_active(3);
        p.switch_workspace(second);
        assert_ne!(p.active, Some(3), "never land on a parked project");
        assert_eq!(p.active, p.first_switchable());
    }

    /// A deleted project must not leave an id behind that would re-park
    /// whatever project inherits it, or restore a ghost on a swipe back.
    #[test]
    fn deleting_a_project_sweeps_it_out_of_every_workspace() {
        let mut p = seeded(2);
        let first = p.active_workspace;
        p.set_active(2);
        p.create_workspace(None);
        p.set_hidden(2, true);
        p.switch_workspace(first);
        p.delete(2);
        assert!(p.workspaces.iter().all(|w| w.hidden.is_empty()));
        assert!(p.workspaces.iter().all(|w| w.active != Some(2)));
    }

    /// A project made in one workspace should not turn up in all of
    /// them; that would make the separation pointless within a session.
    #[test]
    fn a_new_project_is_listed_only_where_it_was_made() {
        let mut p = seeded(1);
        let first = p.active_workspace;
        let second = p.create_workspace(None);
        let made_here = p.create();
        assert!(!p.is_hidden(made_here));
        p.switch_workspace(first);
        assert!(
            p.is_hidden(made_here),
            "parked in the workspace it wasn't made in"
        );
        p.switch_workspace(second);
        assert!(!p.is_hidden(made_here));
    }

    /// Every hide decision in the app lives inside a workspace, so there
    /// always has to be one to stand on.
    #[test]
    fn the_last_workspace_cannot_be_deleted() {
        let mut p = seeded(1);
        let only = p.active_workspace;
        assert!(!p.delete_workspace(only));
        let second = p.create_workspace(None);
        assert!(p.delete_workspace(second));
        assert_eq!(p.workspaces.len(), 1);
        assert_eq!(p.active_workspace, only);
    }

    /// Explicit next/previous commands retain their ring behavior.
    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut p = seeded(1);
        let a = p.active_workspace;
        let b = p.create_workspace(None);
        let c = p.create_workspace(None);
        p.switch_workspace(a);
        assert_eq!(p.cycle_workspace(-1), Some(c), "back from the first wraps");
        assert_eq!(p.cycle_workspace(1), Some(a), "forward from the last wraps");
        assert_eq!(p.cycle_workspace(1), Some(b));
    }

    #[test]
    fn swiping_stops_at_both_ends_of_the_strip() {
        let mut p = seeded(1);
        let first = p.active_workspace;
        let second = p.create_workspace(None);

        p.switch_workspace(first);
        assert_eq!(p.swipe_workspace(-1), None);
        assert_eq!(p.active_workspace, first);
        assert_eq!(p.swipe_workspace(1), Some(second));
        assert_eq!(p.swipe_workspace(1), None);
        assert_eq!(p.active_workspace, second);
    }

    /// With one workspace there is nowhere to swipe to, and a swipe that
    /// silently "succeeded" would latch the gesture for no reason.
    #[test]
    fn cycling_a_lone_workspace_does_nothing() {
        let mut p = seeded(1);
        assert_eq!(p.cycle_workspace(1), None);
    }

    /// The strip is the only readout of where a swipe landed. Dropping
    /// the bar you are standing on would make it lie.
    #[test]
    fn an_overflowing_strip_keeps_the_current_workspace_visible() {
        let mut p = seeded(0);
        p.create_workspace(Some("w0".into()));
        for i in 1..12 {
            p.create_workspace(Some(format!("w{i}")));
        }
        for &target in &p.workspaces.iter().map(|w| w.id).collect::<Vec<_>>() {
            p.switch_workspace(target);
            let strip = workspace_strip(&p, SIDEBAR_MIN_WIDTH);
            assert!(
                strip.bars.len() < p.workspaces.len(),
                "this test is pointless unless the strip actually overflows"
            );
            assert!(
                strip.bars.iter().any(|(id, _)| *id == target),
                "the current workspace fell out of the strip"
            );
        }
    }

    /// Hit rects and sprites both come from `workspace_strip`, so the
    /// only way they can disagree is if the strip runs off the sidebar.
    #[test]
    fn the_strip_stays_inside_the_sidebar() {
        let mut p = seeded(0);
        p.create_workspace(Some("one".into()));
        for w in [SIDEBAR_MIN_WIDTH, SIDEBAR_DEFAULT_WIDTH, SIDEBAR_MAX_WIDTH] {
            for _ in 0..4 {
                p.create_workspace(None);
                let strip = workspace_strip(&p, w);
                for (_, b) in &strip.bars {
                    assert!(b.min.x >= 0.0 && b.max.x <= w, "bar outside sidebar at {w}");
                }
                if let Some(add) = strip.add {
                    assert!(add.max.x <= w, "+ outside sidebar at {w}");
                }
                assert!(strip.left >= 0.0);
            }
        }
    }

    /// The complaint that started this: you could not swipe back
    /// immediately. macOS keeps sending momentum events in the direction
    /// of the flick for up to a second, so the "gesture ended" gap never
    /// arrives between two quick swipes. A sign flip has to end it
    /// instead — momentum never reverses.
    #[test]
    fn a_reversal_ends_the_gesture_even_under_momentum() {
        let mut swipe = SidebarSwipe {
            // A flick left, past the threshold and already switched.
            accum: -SWIPE_THRESHOLD_PX - 10.0,
            fired: true,
            ..Default::default()
        };
        assert!(
            !swipe.is_reversal(-8.0),
            "decaying momentum must not read as a new swipe"
        );
        assert!(
            !swipe.is_reversal(0.0),
            "and an axis-less event ends nothing"
        );
        assert!(
            swipe.is_reversal(12.0),
            "the opposite direction is always a new gesture"
        );
        swipe.begin();
        assert!(!swipe.fired && swipe.accum == 0.0);
        assert!(
            !swipe.is_reversal(12.0),
            "a fresh gesture has no direction to contradict yet"
        );
    }

    #[test]
    fn a_new_same_direction_stroke_ends_the_momentum_gesture() {
        let mut swipe = SidebarSwipe {
            accum: -SWIPE_THRESHOLD_PX - 10.0,
            fired: true,
            last_dx_abs: 16.0,
            ..Default::default()
        };

        swipe.observe_horizontal_burst(-10.0);
        assert!(!swipe.tail_armed);
        swipe.observe_horizontal_burst(-6.0);
        assert!(swipe.tail_armed, "two decaying bursts arm a new stroke");
        assert!(
            !swipe.is_same_direction_restart(-9.0),
            "small momentum variation must stay in the old gesture"
        );
        assert!(
            swipe.is_same_direction_restart(-12.0),
            "a strong same-direction rebound is a fresh swipe"
        );
    }

    #[test]
    fn acceleration_within_one_flick_does_not_restart_it() {
        let mut swipe = SidebarSwipe {
            accum: -SWIPE_THRESHOLD_PX - 10.0,
            fired: true,
            last_dx_abs: 4.0,
            ..Default::default()
        };
        for dx in [-7.0, -12.0, -18.0, -15.0] {
            assert!(!swipe.is_same_direction_restart(dx));
            swipe.observe_horizontal_burst(dx);
        }
        assert!(!swipe.tail_armed);
    }

    /// A slide has to know which way to travel, and cycling off the end
    /// of the strip onto the other end is still forward motion — the
    /// index going backwards is an artefact of the wrap, not a direction.
    #[test]
    fn a_wrapping_cycle_still_travels_forward() {
        let mut p = seeded(1);
        let a = p.active_workspace;
        p.create_workspace(None);
        let c = p.create_workspace(None);
        p.switch_workspace(a);
        p.pending_switch = None;

        p.cycle_workspace(-1);
        assert_eq!(p.active_workspace, c);
        assert_eq!(
            p.pending_switch.map(|(_, dir)| dir),
            Some(-1.0),
            "wrapping backwards off the first workspace travels backwards"
        );

        p.pending_switch = None;
        p.cycle_workspace(1);
        assert_eq!(p.active_workspace, a);
        assert_eq!(
            p.pending_switch.map(|(_, dir)| dir),
            Some(1.0),
            "and wrapping forwards off the last travels forwards"
        );
    }

    /// A click on the strip has no direction of its own, so it is read
    /// off the positions — jumping rightwards along the strip should
    /// bring the new list in from the right.
    #[test]
    fn a_direct_jump_takes_its_direction_from_the_strip() {
        let mut p = seeded(1);
        let a = p.active_workspace;
        let b = p.create_workspace(None);
        p.switch_workspace(a);

        p.pending_switch = None;
        p.switch_workspace(b);
        assert_eq!(
            p.pending_switch.map(|(from, dir)| (from, dir)),
            Some((a, 1.0))
        );

        p.pending_switch = None;
        p.switch_workspace(a);
        assert_eq!(
            p.pending_switch.map(|(from, dir)| (from, dir)),
            Some((b, -1.0))
        );
    }

    /// Mid-slide there are two lists: the one you left going out and the
    /// one you arrived at coming in, from opposite edges. Only the
    /// arriving one may take clicks.
    #[test]
    fn a_slide_draws_both_lists_travelling_opposite_ways() {
        let mut p = seeded(2);
        let first = p.active_workspace;
        p.set_hidden(2, true);
        let second = p.create_workspace(None);
        p.set_hidden(2, false);
        p.set_hidden(1, true);
        p.switch_workspace(first);

        let width = SIDEBAR_DEFAULT_WIDTH;
        let (from, dir) = p.pending_switch.expect("a switch queues a slide");
        let slide = SidebarSlide {
            from: Some(from),
            dir,
            t: 0.5,
        };
        let passes = sidebar_passes(&p, &slide, &SidebarScroll::default(), width, 900.0);
        assert_eq!(passes.len(), 2);
        let (out, incoming) = (&passes[0], &passes[1]);
        assert_eq!(out.workspace, second, "the list being left");
        assert_eq!(incoming.workspace, first, "the list arriving");
        assert!(!out.interactive && incoming.interactive);
        assert!(
            out.dx.signum() != incoming.dx.signum(),
            "they pass each other, {} vs {}",
            out.dx,
            incoming.dx
        );
        assert!(out.dx.abs() <= width && incoming.dx.abs() <= width);
        // Each shows its OWN workspace's projects, not the other's.
        let ids = |pass: &SidebarPass| -> Vec<u64> {
            pass.rows.iter().map(|&(i, _)| p.list[i].id).collect()
        };
        assert_eq!(
            ids(out),
            vec![2],
            "the workspace being left parked project 1"
        );
        assert_eq!(
            ids(incoming),
            vec![1],
            "and the one arriving parked project 2"
        );
    }

    /// At rest there is one list, at its resting position, taking clicks.
    #[test]
    fn at_rest_there_is_one_list_and_it_does_not_move() {
        let p = seeded(2);
        let passes = sidebar_passes(
            &p,
            &SidebarSlide::default(),
            &SidebarScroll::default(),
            SIDEBAR_DEFAULT_WIDTH,
            900.0,
        );
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].dx, 0.0);
        assert!(passes[0].interactive);
    }

    /// The eased curve has to actually start at the far edge and land
    /// exactly at rest, or the list jumps on the first or last frame.
    #[test]
    fn the_slide_starts_off_screen_and_lands_at_rest() {
        let mut p = seeded(1);
        let a = p.active_workspace;
        let b = p.create_workspace(None);
        p.switch_workspace(a);
        p.switch_workspace(b);
        let (from, dir) = p.pending_switch.expect("queued");
        let width = SIDEBAR_DEFAULT_WIDTH;

        let at = |t: f32| {
            let slide = SidebarSlide {
                from: Some(from),
                dir,
                t,
            };
            let passes = sidebar_passes(&p, &slide, &SidebarScroll::default(), width, 900.0);
            (passes[0].dx, passes[1].dx)
        };
        let (out0, in0) = at(0.0);
        assert_eq!(out0, 0.0, "the departing list starts where it sat");
        assert_eq!(
            in0.abs(),
            width,
            "the arriving list starts a full width out"
        );
        let (out1, in1) = at(1.0);
        assert_eq!(out1.abs(), width, "and it leaves by a full width");
        assert_eq!(in1, 0.0, "landing exactly at rest");
    }

    /// Only the arriving list's hit rects are reachable, and they travel
    /// with it. Two live rects for the same row in different workspaces
    /// would make the picker's first-match-wins arbitrary.
    #[test]
    fn only_the_arriving_lists_hit_rects_are_reachable() {
        let base = SidebarBounds {
            min: Vec2::new(0.0, 10.0),
            max: Vec2::new(100.0, 40.0),
        };
        let arriving = SidebarPass {
            rows: Vec::new(),
            workspace: 1,
            active: None,
            dx: -30.0,
            scroll: 0.0,
            interactive: true,
        };
        let moved = base.placed(&arriving);
        assert_eq!(moved.min.x, -30.0, "the rect follows its row");
        assert_eq!(moved.min.y, 10.0, "and only sideways");

        let departing = SidebarPass {
            interactive: false,
            ..arriving
        };
        let gone = base.placed(&departing);
        assert!(!in_bounds(Vec2::new(50.0, 20.0), &gone));
        assert!(!in_bounds(Vec2::new(f32::MAX, f32::MAX), &gone));
    }

    /// A list that fits must not scroll at all — the sidebar looks and
    /// behaves exactly as it did before scrolling existed.
    #[test]
    fn a_list_that_fits_cannot_scroll() {
        let win_h = 900.0;
        let fits = ((rows_room(win_h) - DIVIDER_H - ROW_H) / ROW_H).floor() as usize;
        assert_eq!(max_scroll(fits, win_h), 0.0);
        assert!(max_scroll(fits + 1, win_h) > 0.0, "one more row overflows");
    }

    /// The footer is reserved: the list can never scroll into it, so the
    /// show-hidden eyeball is always visible and always clickable. Before
    /// this there was permanently a project row under that corner, which
    /// ate the click and hid the eyeball.
    #[test]
    fn the_list_can_never_reach_the_footer() {
        let win_h = 600.0;
        assert_eq!(rows_room(win_h), win_h - HEADER_H - FOOTER_H);
        // Scrolled as far as it goes, the content's last pixel stops at
        // the top of the footer rather than the bottom of the window.
        let rows = 60;
        let content = rows as f32 * ROW_H + DIVIDER_H + ROW_H;
        assert_eq!(
            HEADER_H - max_scroll(rows, win_h) + content,
            win_h - FOOTER_H
        );
        // And nothing in the footer band counts as a row hit.
        assert!(in_rows_band(win_h - FOOTER_H - 1.0, win_h));
        assert!(!in_rows_band(win_h - FOOTER_H, win_h));
        assert!(
            !in_rows_band(win_h - 1.0, win_h),
            "the eyeball's own corner"
        );
        assert!(!in_rows_band(HEADER_H - 1.0, win_h), "and under the header");
    }

    /// A window too short for header + footer must not produce a
    /// negative-height row band that would make every hit test nonsense.
    #[test]
    fn a_tiny_window_degrades_to_no_row_band() {
        let win_h = HEADER_H + FOOTER_H * 0.5;
        assert_eq!(rows_room(win_h), 0.0);
        assert!(!in_rows_band(HEADER_H + 1.0, win_h));
        assert!(max_scroll(50, win_h) > 0.0, "still scrollable, just unseen");
    }

    /// The travel has to cover the "+ New Project" row and its divider,
    /// not just the projects — otherwise the one row you need to reach
    /// when the list is long is the one you cannot.
    #[test]
    fn the_scroll_range_reaches_the_new_project_row() {
        let win_h = 300.0;
        let rows = 40;
        let travel = max_scroll(rows, win_h);
        let content = rows as f32 * ROW_H + DIVIDER_H + ROW_H;
        assert_eq!(travel, content - rows_room(win_h));
        // Scrolled fully, the bottom of the content sits on the footer,
        // so the last thing in the list is on screen.
        assert_eq!(HEADER_H - travel + content, win_h - FOOTER_H);
    }

    /// Hover, press and reorder all map a cursor onto a row. If they
    /// disagree you highlight one project and grab another.
    #[test]
    fn a_scrolled_row_is_hit_where_it_is_drawn() {
        // Unscrolled, the first row starts right under the header.
        assert_eq!(row_slot_at(HEADER_H + 1.0, 0.0), 0);
        assert_eq!(row_slot_at(HEADER_H + ROW_H + 1.0, 0.0), 1);
        // Scrolled by exactly two rows, the row drawn just under the
        // header is the third one.
        assert_eq!(row_slot_at(HEADER_H + 1.0, ROW_H * 2.0), 2);
        // And the row drawn where row 0 was is no longer row 0.
        assert_ne!(row_slot_at(HEADER_H + 1.0, ROW_H * 2.0), 0);
    }

    /// Each workspace keeps its own scroll position: they hold different
    /// numbers of projects, and coming back should find the list where
    /// you left it.
    #[test]
    fn scroll_is_remembered_per_workspace() {
        let win_h = 200.0;
        let mut scroll = SidebarScroll::default();
        scroll.per_workspace.insert(1, 120.0);
        scroll.per_workspace.insert(2, 40.0);
        assert_eq!(scroll.clamped(1, 40, win_h), 120.0);
        assert_eq!(scroll.clamped(2, 40, win_h), 40.0);
        assert_eq!(
            scroll.clamped(3, 40, win_h),
            0.0,
            "unvisited starts at the top"
        );
    }

    /// Reading clamped as well as writing clamped: deleting projects or
    /// shrinking the window must not leave a list parked past its end,
    /// showing empty space where rows used to be.
    #[test]
    fn a_shrinking_list_pulls_its_scroll_back() {
        let win_h = 400.0;
        let mut scroll = SidebarScroll::default();
        // Parked well past the end of even the long list.
        scroll.per_workspace.insert(1, 5_000.0);
        assert_eq!(scroll.clamped(1, 40, win_h), max_scroll(40, win_h));
        assert_eq!(scroll.clamped(1, 2, win_h), 0.0, "now everything fits");
    }

    /// A gesture commits to one axis and keeps it. Trackpads leak each
    /// axis into the other, and a scroll that swapped the whole sidebar
    /// mid-flick would be the worst failure available here.
    #[test]
    fn a_gesture_picks_one_axis_and_keeps_it() {
        let mut g = SidebarSwipe::default();
        // Too small to call yet.
        g.accum = 2.0;
        g.accum_y = 1.0;
        assert_eq!(g.decide_axis(), GestureAxis::Undecided);

        // Clearly sideways.
        g.accum = 40.0;
        g.accum_y = 3.0;
        assert_eq!(g.decide_axis(), GestureAxis::Swipe);
        // Drifting vertically later must not change its mind.
        g.accum_y = 400.0;
        assert_eq!(g.decide_axis(), GestureAxis::Swipe);

        // A fresh, clearly vertical gesture.
        g.begin();
        g.accum = 4.0;
        g.accum_y = 40.0;
        assert_eq!(g.decide_axis(), GestureAxis::Scroll);
        g.accum = 400.0;
        assert_eq!(g.decide_axis(), GestureAxis::Scroll);
    }

    /// The one-sided ratio: a mostly-vertical gesture with real sideways
    /// drift scrolls. Getting this wrong swaps the sidebar under someone
    /// who was only scrolling.
    #[test]
    fn a_wobbly_scroll_does_not_switch_workspace() {
        let mut g = SidebarSwipe::default();
        g.accum = 20.0;
        g.accum_y = 30.0;
        assert_eq!(
            g.decide_axis(),
            GestureAxis::Scroll,
            "sideways has to clearly dominate, not merely be present"
        );
    }

    #[test]
    fn a_long_workspace_name_is_clipped_not_overflowed() {
        assert_eq!(truncate_to_width("MAIN", 100.0, 8.0), "MAIN");
        assert_eq!(truncate_to_width("WORKSPACE", 24.0, 8.0), "WO…");
        assert_eq!(truncate_to_width("WORKSPACE", 4.0, 8.0), "");
    }
}
