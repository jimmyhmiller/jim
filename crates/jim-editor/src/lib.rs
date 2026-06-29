//! Editor view layer built on Bevy's 2D/text pipeline.
//!
//! Pane chrome (drag, resize, close, focus, z-order) is owned by
//! `pane-bevy`. This crate only owns editor-specific concerns:
//! rendering text spans into the pane's content_root, caret/selection
//! visuals, scroll, keyboard input, and the highlighter.
//!
//! # Embedding
//!
//! - `EditorPlugin` — standalone editor app: camera, font, clear color,
//!   winit settings, plus `PanePlugin` and `EditorEmbedPlugin`.
//! - `EditorEmbedPlugin` — for hosts (like `terminal-bevy`) that already
//!   own the camera, clear color, etc. Registers the "editor" pane kind
//!   in `PaneRegistry` and adds editor-specific systems. Hosts must
//!   also add `PanePlugin` and provide `EditorFont` + `EditorMetrics`
//!   via `setup_editor_font`.

use std::collections::HashMap;
use std::path::PathBuf;

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::text::{LineHeight, TextSpan};
use editor_core::commands::{
    cursor_char_left, cursor_char_right, cursor_doc_end, cursor_doc_start, cursor_line_down,
    cursor_line_end, cursor_line_start, cursor_line_up, cursor_word_left, cursor_word_right,
    delete_char_backward, delete_char_forward, indent_more, insert_newline_and_indent, select_all,
    select_char_left, select_char_right, select_doc_end, select_doc_start, select_line_down,
    select_line_end, select_line_start, select_line_up, select_word_left, select_word_right,
};
use editor_core::history::{redo, undo};
use editor_core::selection::{Range, Selection};
use editor_core::state::EditorState;
use editor_core::transaction::{Change, Transaction};
use jim_pane::{
    spawn_pane, FocusedPane, PaneChrome, PaneContentPressed, PaneFont, PaneKindMarker,
    PanePlugin, PaneRect, PaneRegistry, PaneTag, SpawnedPane, MARGIN, TITLE_H,
};
use serde_json::Value;

pub mod highlight;
pub mod markdown;
pub mod wrap;
use highlight::{color_for, Highlighter, SyntaxPalette};
use markdown::{wysiwyg_active, MarkdownMode, MdLayout};

pub const FONT_SIZE: f32 = 16.0;
pub const LINE_HEIGHT: f32 = 20.0;

pub const PANE_KIND: &str = "editor";

// ---------- Components: editor-specific ----------

/// Filename a pane was opened from. Hosts (terminal-bevy) attach this on
/// open-file requests; the editor crate doesn't read it itself, but
/// pane-bevy persistence routes through `editor_snapshot` which inspects it.
#[derive(Component, Clone, Debug)]
pub struct EditorFilePath(pub PathBuf);

#[derive(Component)]
pub struct EditorStateComp(pub EditorState);

/// Pool of rendered text rows, keyed by **global visual row** (not
/// logical line — with soft-wrap one logical line spans several rows).
#[derive(Component, Default)]
pub struct LineRows(pub HashMap<usize, Entity>);

/// Cached soft-wrap layout for an editor, rebuilt by
/// [`compute_wrap_layout`] whenever the doc or pane width changes. Every
/// geometry system (render, caret, selection, click, scroll, vertical
/// motion) reads this one map so they always agree on where wrapped rows
/// fall.
#[derive(Component, Default)]
pub struct EditorWrapLayout(pub wrap::WrapLayout);

#[derive(Component, Copy, Clone, Default)]
pub struct EditorScroll {
    pub x: f32,
    pub y: f32,
}

/// Mount-agnostic geometry for an editor: *where* its text content lives
/// and *how big* the viewport is, with no pane chrome in the picture.
///
/// Every render system (wrap layout, line rows, caret, selection, scroll
/// transform) reads this one component, so the same systems drive both a
/// real floating pane and an embedded "portal" living inside a funct
/// widget. For pane editors, [`sync_pane_editor_view`] fills it each
/// frame from `PaneRect` + `PaneChrome`; embedders fill it themselves.
#[derive(Component, Clone, Copy)]
pub struct EditorView {
    /// Scroll-root entity. Text rows, the caret, and selection rects are
    /// spawned as children of this. Its `Transform.translation` is set
    /// each frame to `origin + (0, scroll.y)`.
    pub render_root: Entity,
    /// Content-area (viewport) size in canvas units. Drives wrap columns,
    /// the visible row range, and caret/selection visibility clipping.
    pub size: Vec2,
    /// Offset of `render_root` within its parent's local space. For a
    /// pane this clears the title bar + margin (`MARGIN, -(TITLE_H +
    /// MARGIN)`); for a flush-mounted portal it is zero.
    pub origin: Vec2,
}

/// Pane → [`EditorView`] adapter: keep an editor pane's view geometry in
/// sync with its chrome. Writes only when something actually changed, so
/// `EditorView` change-detection stays meaningful for `compute_wrap_layout`.
fn sync_pane_editor_view(
    pane_zoom: Res<jim_pane::PaneZoom>,
    mut editors: Query<(&PaneRect, &PaneChrome, &mut EditorView, &PaneKindMarker), With<PaneTag>>,
) {
    let zoom = pane_zoom.0;
    let origin = Vec2::new(MARGIN, -(TITLE_H + MARGIN));
    for (rect, chrome, mut view, kind) in &mut editors {
        if kind.0 != PANE_KIND {
            continue;
        }
        let size = content_area_size_zoomed(rect, zoom);
        if view.render_root != chrome.content_root || view.size != size || view.origin != origin {
            view.render_root = chrome.content_root;
            view.size = size;
            view.origin = origin;
        }
    }
}

/// Anchor char offset of an in-progress text-selection drag.
#[derive(Component, Default)]
pub struct TextDragAnchor(pub Option<usize>);

#[derive(Component)]
pub struct EditorHighlighter(pub Highlighter);

#[derive(Component)]
struct LineRender {
    text: String,
    rev: u64,
    /// Snapshot of `SyntaxPalette.rev` at the time we built the
    /// colored spans. A theme switch bumps the palette rev; we rebuild
    /// the line so the new colors apply.
    palette_rev: u64,
}

#[derive(Component)]
pub struct SelRect {
    pub editor: Entity,
}

// ---------- Shared resources ----------

#[derive(Resource)]
pub struct EditorFont(pub Handle<Font>);

#[derive(Resource, Copy, Clone)]
pub struct EditorMetrics {
    pub cell_width: f32,
}

// ---------- Plugins ----------

pub struct EditorEmbedPlugin;

impl Plugin for EditorEmbedPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(highlight::HighlightPlugin)
            .init_resource::<FocusedEmbeddedEditor>()
            .init_resource::<markdown::MdDebugViz>()
            .add_systems(
                Update,
                (
                    markdown::markdown_debug_toggle,
                    markdown::markdown_debug_boxes,
                    markdown::markdown_debug_hide_text,
                ),
            )
            .add_message::<EmbeddedEditorPress>()
            .add_message::<EmbeddedEditorDrag>()
            .add_message::<EmbeddedEditorRelease>()
            .add_message::<EmbeddedEditorScroll>()
            .add_message::<EmbeddedEditorSubmit>()
            .add_systems(Startup, (register_editor_kind, markdown::setup_markdown_fonts))
            .add_systems(
                Update,
                (
                    markdown::detect_markdown_mode,
                    handle_pane_content_press,
                    handle_text_select_drag,
                    handle_scroll,
                    handle_input,
                    handle_embedded_press,
                    handle_embedded_drag,
                    handle_embedded_release,
                    handle_embedded_scroll,
                    handle_embedded_keys,
                    sync_pane_editor_view,
                    compute_wrap_layout,
                    update_highlight,
                    sync_text,
                    sync_content_root,
                    sync_caret,
                    sync_selection,
                    markdown::ensure_md_keepalive,
                    markdown::markdown_render,
                    markdown::markdown_decorations,
                )
                    .chain(),
            )
            .add_systems(
                PostUpdate,
                // Read back fresh glyph layout and position the caret /
                // selection / lines in the SAME frame the text was laid
                // out — *after* text layout so the geometry is current,
                // *before* transform propagation so the transforms we set
                // reach GlobalTransform this frame. Positioning from a
                // prior frame's layout would map the caret against stale
                // source offsets (every keystroke shifts them), making it
                // flicker onto the next line / jump to the doc end.
                (markdown::markdown_readback, markdown::markdown_position)
                    .chain()
                    .after(bevy::sprite::update_text2d_layout)
                    .before(bevy::transform::TransformSystems::Propagate),
            );
    }
}

fn register_editor_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(jim_pane::PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Editor",
        radial_icon: Some("{}"),
        default_size: Vec2::new(640.0, 420.0),
        spawn: editor_spawn_from_config,
        snapshot: editor_snapshot,
        on_close: None,
    });
}

pub struct EditorPlugin;

impl Plugin for EditorPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(bevy::winit::WinitSettings {
            focused_mode: bevy::winit::UpdateMode::Continuous,
            unfocused_mode: bevy::winit::UpdateMode::Continuous,
        })
        .insert_resource(ClearColor(Color::srgb(0.10, 0.11, 0.13)))
        .add_systems(
            Startup,
            (setup_editor_camera, setup_editor_font),
        )
        .add_systems(PostStartup, release_os_focus)
        .add_plugins(PanePlugin::default())
        .add_plugins(EditorEmbedPlugin);
    }
}

#[cfg(target_os = "macos")]
fn release_os_focus() {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        unsafe { app.deactivate() };
    }
}

#[cfg(not(target_os = "macos"))]
fn release_os_focus() {}

/// Headless plugin for keybinding tests. No PanePlugin dependency —
/// tests spawn a bare editor entity and drive `handle_input` directly.
pub struct HeadlessEditorPlugin;

impl Plugin for HeadlessEditorPlugin {
    fn build(&self, app: &mut App) {
        // `handle_input` reads these as required resources; insert them so
        // a bare headless App (tests) is self-sufficient. `KeyboardOwner`
        // defaults to `Unmanaged`, which allows the focused pane to type.
        app.init_resource::<FocusedPane>()
            .init_resource::<jim_pane::KeyboardOwner>()
            .init_resource::<jim_pane::PaneZoom>()
            .add_systems(Update, handle_input);
    }
}

// Bevy 0.19 migrated text shaping from cosmic-text to parley. System-font
// fallback is now discovered automatically by parley/fontique, so the old
// explicit `CosmicFontSystem::db_mut().load_system_fonts()` startup pass is
// gone — see the `FontCx` resource in bevy_text.

const EMBEDDED_FONT: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

fn measure_cell_width(font_bytes: &[u8], font_size: f32) -> f32 {
    use skrifa::instance::{LocationRef, Size};
    use skrifa::{FontRef, MetadataProvider};

    let font = FontRef::from_index(font_bytes, 0).expect("embedded font must parse");
    let metrics = font.glyph_metrics(Size::new(font_size), LocationRef::default());
    let gid = font
        .charmap()
        .map('M')
        .expect("embedded font must contain 'M'");
    metrics
        .advance_width(gid)
        .expect("'M' must have an advance width")
}

pub fn setup_editor_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

/// Load the editor font and insert both `EditorFont` and `PaneFont`
/// (the latter is what pane-bevy uses for chrome). Standalone hosts
/// can call this; embedders that already have a font may skip it and
/// insert their own.
///
/// Resolves the font through [`jim_style::FontRegistry`] using
/// `FONT_FAMILY_MONO` from the active theme, so adding a real mono
/// family later (or pointing the token at a different bundled name)
/// retones the editor without code changes. `measure_cell_width` still
/// reads from the bundled bytes since skrifa needs raw bytes.
pub fn setup_editor_font(
    mut commands: Commands,
    mut fonts: ResMut<Assets<Font>>,
    mut registry: ResMut<jim_style::FontRegistry>,
    theme: Res<jim_style::Theme>,
) {
    // Make sure the registry has its bundled families — Startup-system
    // ordering is unreliable across plugins, so push entries from here
    // too. `ensure_initialized` is idempotent.
    jim_style::fonts::ensure_initialized(&mut registry, &mut fonts);
    let mono_name = theme.str_value(jim_style::tokens::FONT_FAMILY_MONO).to_string();
    let font = registry.resolve(&mono_name);
    let measure_bytes = registry.bytes(&mono_name).unwrap_or(EMBEDDED_FONT);
    commands.insert_resource(EditorFont(font.clone()));
    commands.insert_resource(EditorMetrics {
        cell_width: measure_cell_width(measure_bytes, FONT_SIZE),
    });
    commands.insert_resource(PaneFont(font));
    // `PaneFont` is what pane-bevy and every cosmic-text-backed pane
    // (Issues, text inputs) actually *render* with, so `PaneFontMetrics`
    // — which those panes use to place carets/selection on a
    // `col * cell_width` grid and to compute word-wrap — must describe
    // THIS font. We set both here (overwriting any host default) so the
    // measured advance always matches the rendered glyphs; otherwise the
    // caret drifts right of the text by the per-glyph advance difference,
    // growing with column. Measured at `FONT_SIZE`; callers scale linearly
    // for other sizes (the mono family is fixed-pitch).
    commands.insert_resource(jim_pane::PaneFontMetrics {
        cell_width: measure_cell_width(measure_bytes, FONT_SIZE),
        font_size: FONT_SIZE,
    });
}

// ---------- Spawn ----------

/// Spawn an editor pane with the given initial text. Returns the pane
/// entity. Uses pane-bevy chrome under the hood.
pub fn spawn_editor_pane(
    world: &mut World,
    initial_text: &str,
    rect: PaneRect,
    project_id: Option<u64>,
) -> Entity {
    let SpawnedPane {
        entity,
        content_root,
    } = spawn_pane(world, PANE_KIND, "Editor", rect, project_id);
    populate_editor_pane(world, entity, content_root, initial_text);
    entity
}

/// Insert editor-specific components on an already-spawned pane and
/// add the caret child under `content_root`. Shared between
/// `spawn_editor_pane` and the registry restore path.
fn populate_editor_pane(
    world: &mut World,
    entity: Entity,
    content_root: Entity,
    initial_text: &str,
) {
    world.entity_mut(entity).insert((
        EditorStateComp(
            EditorState::new(
                ropey::Rope::from_str(initial_text),
                Selection::cursor(0),
            )
            .with_indent_unit("    "),
        ),
        EditorHighlighter(Highlighter::new()),
        LineRows::default(),
        EditorScroll::default(),
        TextDragAnchor::default(),
        // Seeded with the pane offset; `sync_pane_editor_view` fills the
        // real size on the first frame. `size = ZERO` keeps the wrap
        // layout from building against a bogus width before then.
        EditorView {
            render_root: content_root,
            size: Vec2::ZERO,
            origin: Vec2::new(MARGIN, -(TITLE_H + MARGIN)),
        },
    ));
    let caret_color = world
        .get_resource::<jim_style::Theme>()
        .map(|t| Color::LinearRgba(t.color(jim_style::tokens::CARET)))
        .unwrap_or_else(|| Color::srgb(0.55, 0.85, 1.0));
    let _caret = world
        .spawn((
            ChildOf(content_root),
            EditorCaret(entity),
            Sprite {
                color: caret_color,
                custom_size: Some(Vec2::new(2.0, LINE_HEIGHT)),
                ..default()
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(0.0, 0.0, 1.0),
        ))
        .id();
}

/// Marker for the caret child of an editor pane. Holds the parent
/// pane entity so the caret-sync system can join its position back to
/// the pane's editor state without needing the pane chrome to track
/// caret separately.
#[derive(Component)]
pub struct EditorCaret(pub Entity);

/// Registry callback — invoked by pane-bevy on restore. The config
/// blob is whatever `editor_snapshot` produced (currently `{ text,
/// path }`).
fn editor_spawn_from_config(world: &mut World, entity: Entity, content_root: Entity, config: &Value) {
    let text = config
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    populate_editor_pane(world, entity, content_root, &text);
    if let Some(path) = config.get("path").and_then(|v| v.as_str()) {
        world
            .entity_mut(entity)
            .insert(EditorFilePath(PathBuf::from(path)));
    }
}

fn editor_snapshot(world: &World, entity: Entity) -> Value {
    let text = world
        .get::<EditorStateComp>(entity)
        .map(|s| s.0.doc.to_string())
        .unwrap_or_default();
    let mut obj = serde_json::Map::new();
    obj.insert("text".into(), Value::String(text));
    if let Some(path) = world.get::<EditorFilePath>(entity) {
        obj.insert(
            "path".into(),
            Value::String(path.0.to_string_lossy().into()),
        );
    }
    Value::Object(obj)
}

/// Build a ready-to-run App with a single editor loaded from `initial`.
pub fn build_app(initial: &str) -> App {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "editor".into(),
            resolution: (900u32, 600u32).into(),
            ..default()
        }),
        ..default()
    }));
    app.add_plugins(EditorPlugin);

    let initial = initial.to_string();
    app.add_systems(
        Startup,
        (move |world: &mut World| {
            let initial = initial.clone();
            let e = spawn_editor_pane(
                world,
                &initial,
                PaneRect {
                    pos: Vec2::new(40.0, 40.0),
                    size: Vec2::new(820.0, 500.0),
                    z: 1.0,
                },
                None,
            );
            world.resource_mut::<FocusedPane>().0 = Some(e);
        })
            .after(setup_editor_font),
    );
    app
}

// ---------- Content-area geometry ----------

/// Legacy: `PaneRect` is now canvas-space directly (the pane entity's
/// Transform handles zoom). Zoom is ignored; kept so existing call
/// sites compile.
fn content_area_size_zoomed(rect: &PaneRect, _zoom: f32) -> Vec2 {
    content_area_size(rect)
}

fn content_area_size(rect: &PaneRect) -> Vec2 {
    Vec2::new(
        (rect.size.x - 2.0 * MARGIN).max(0.0),
        (rect.size.y - TITLE_H - 2.0 * MARGIN).max(0.0),
    )
}

fn max_cols(content_width: f32, cell_width: f32) -> usize {
    if cell_width <= 0.0 {
        return 0;
    }
    ((content_width / cell_width).floor() as usize).max(1)
}

/// Rebuild each editor's [`EditorWrapLayout`] when its doc or width
/// changes. Runs after input edits and before the render/caret/selection
/// systems, so they all see a layout consistent with the current frame.
/// Click/drag/scroll handlers (earlier in the chain) read the prior
/// frame's layout — which is exactly what the user clicked on.
fn compute_wrap_layout(
    metrics: Option<Res<EditorMetrics>>,
    mut commands: Commands,
    mut editors: Query<(
        Entity,
        Ref<EditorStateComp>,
        Ref<EditorView>,
        Option<&mut EditorWrapLayout>,
    )>,
) {
    let Some(metrics) = metrics else {
        return;
    };
    for (entity, state, view, layout) in &mut editors {
        let cols = max_cols(view.size.x, metrics.cell_width);
        let stale = match &layout {
            Some(l) => l.0.cols != cols || state.is_changed() || view.is_changed(),
            None => true,
        };
        if !stale {
            continue;
        }
        let effective = effective_line_count(&state.0.doc);
        let built = wrap::WrapLayout::build(&state.0.doc, cols, effective);
        match layout {
            Some(mut l) => l.0 = built,
            None => {
                commands.entity(entity).insert(EditorWrapLayout(built));
            }
        }
    }
}

fn byte_offset_for_col(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(b, _)| b).unwrap_or(s.len())
}

// ---------- Systems ----------

fn line_text(doc: &ropey::Rope, idx: usize) -> String {
    let s = doc.line(idx).to_string();
    s.strip_suffix('\n').map(str::to_string).unwrap_or(s)
}

fn update_highlight(mut editors: Query<(&EditorStateComp, &mut EditorHighlighter)>) {
    for (state, mut hl) in &mut editors {
        hl.0.maybe_reparse(&state.0.doc);
    }
}

fn ensure_caret_visible(
    state: &EditorState,
    content: Vec2,
    scroll: &mut EditorScroll,
    layout: &wrap::WrapLayout,
) {
    let head = state.selection.primary_range().head;
    let (line, col) = char_to_line_col(&state.doc, head);
    let (row, _x_col) = layout.pos_to_visual(line, col);
    if content.y <= 0.0 {
        return;
    }

    // Soft-wrap means no horizontal scroll: every row fits the width.
    let row_top = row as f32 * LINE_HEIGHT;
    let row_bottom = row_top + LINE_HEIGHT;
    if row_top < scroll.y {
        scroll.y = row_top;
    } else if row_bottom > scroll.y + content.y {
        scroll.y = row_bottom - content.y;
    }
    scroll.x = 0.0;
    scroll.y = scroll.y.max(0.0);
}

fn sync_text(
    font: Res<EditorFont>,
    metrics: Res<EditorMetrics>,
    palette: Res<SyntaxPalette>,
    project_palettes: Res<highlight::ProjectSyntaxPalettes>,
    mut editors: Query<(
        Entity,
        &EditorStateComp,
        &EditorView,
        &EditorScroll,
        &EditorHighlighter,
        &mut LineRows,
        Option<&jim_pane::PaneProject>,
        Option<&EditorWrapLayout>,
        Option<&MarkdownMode>,
    )>,
    mut line_q: Query<&mut LineRender>,
    children_q: Query<&Children>,
    mut commands: Commands,
) {
    for (entity, state, view, scroll, hl, mut pool, proj, layout, md) in &mut editors {
        // WYSIWYG markdown owns rendering; tear down grid rows.
        if wysiwyg_active(md) {
            for (_row, e) in pool.0.drain() {
                commands.entity(e).despawn();
            }
            continue;
        }
        let Some(layout) = layout else {
            continue; // layout not built yet (first frame)
        };
        let _prof = jim_pane::prof::pane_span(entity.to_bits(), "editor");
        // This editor's project palette if cached, else the global one.
        let pal = proj
            .and_then(|p| project_palettes.get(p.0))
            .unwrap_or(&palette);
        sync_editor_lines(
            &state.0,
            view,
            *scroll,
            &layout.0,
            &hl.0,
            pal,
            &mut pool,
            &font,
            &metrics,
            &mut line_q,
            &children_q,
            &mut commands,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_editor_lines(
    state: &EditorState,
    view: &EditorView,
    scroll: EditorScroll,
    layout: &wrap::WrapLayout,
    hl: &Highlighter,
    palette: &SyntaxPalette,
    pool: &mut LineRows,
    font: &EditorFont,
    metrics: &EditorMetrics,
    line_q: &mut Query<&mut LineRender>,
    children_q: &Query<&Children>,
    commands: &mut Commands,
) {
    let _ = metrics; // cell width is baked into the layout's column model
    let rope = &state.doc;
    let total_rows = layout.total_rows();
    let content_size = view.size;
    // Viewport not sized yet, or transiently collapsed by a host re-layout
    // (e.g. a flex parent that resolves to ~0 height for a frame). Don't
    // re-pool against a zero viewport: `viewport_row_range` would collapse
    // to row 0, despawn every currently-visible row, and — if we're scrolled
    // down — leave row 0 off-screen, blanking the portal until the next
    // content change repaints it. Keep what's drawn; the next sized frame
    // re-pools correctly.
    if content_size.y <= 0.0 {
        return;
    }
    // Pool entries are keyed by GLOBAL VISUAL ROW (one wrapped row each).
    let (first, last) = viewport_row_range(content_size.y, scroll.y, total_rows);

    pool.0.retain(|&row, entity| {
        let keep = row < total_rows && row >= first && row <= last;
        if !keep {
            commands.entity(*entity).despawn();
        }
        keep
    });

    if total_rows == 0 {
        return;
    }

    for row in first..=last.min(total_rows.saturating_sub(1)) {
        let Some((line, seg_start, seg_end)) = layout.segment_at_row(row) else {
            continue;
        };
        let full = line_text(rope, line);
        let start_byte = byte_offset_for_col(&full, seg_start);
        let end_byte = byte_offset_for_col(&full, seg_end);
        let slice = full[start_byte..end_byte].to_string();
        if slice.is_empty() {
            // Empty visual row (e.g. a blank line) — no glyphs to draw,
            // but the row still occupies vertical space in the layout.
            if let Some(entity) = pool.0.remove(&row) {
                commands.entity(entity).despawn();
            }
            continue;
        }
        match pool.0.get(&row).copied() {
            Some(entity) => {
                let content_changed = line_q
                    .get(entity)
                    .map(|lr| {
                        lr.text != slice || lr.rev != hl.rev || lr.palette_rev != palette.rev
                    })
                    .unwrap_or(true);
                // Invariant: a pooled, visible, non-empty row must own its
                // glyph spans. If it somehow lost them (e.g. a transient
                // hierarchy churn during a select+scroll gesture) the
                // content-change check above stays false for a static doc,
                // so the row would render blank forever until the next real
                // edit. Detect the empty-children case and force a rebuild
                // so the portal can never get stuck blank.
                let spans_missing = children_q
                    .get(entity)
                    .map(|c| c.is_empty())
                    .unwrap_or(true);
                if spans_missing && !content_changed {
                    eprintln!(
                        "[editor] repaired blank row {row} (spans lost; doc unchanged) \
                         total_rows={total_rows} scroll={:.1} size={:?}",
                        scroll.y, content_size
                    );
                }
                let needs_rebuild = content_changed || spans_missing;
                if needs_rebuild {
                    rebuild_line_spans(
                        commands, entity, children_q, hl, palette, rope, line, start_byte, &slice,
                        font,
                    );
                    if let Ok(mut lr) = line_q.get_mut(entity) {
                        lr.text = slice;
                        lr.rev = hl.rev;
                        lr.palette_rev = palette.rev;
                    } else {
                        commands.entity(entity).insert(LineRender {
                            text: slice,
                            rev: hl.rev,
                            palette_rev: palette.rev,
                        });
                    }
                }
                // The pool key is the visual row, so the entity already
                // sits at `-row * LINE_HEIGHT`; no transform update needed.
            }
            None => {
                let entity = commands
                    .spawn((
                        ChildOf(view.render_root),
                        Text2d::new(String::new()),
                        TextFont {
                            font: (font.0.clone()).into(),
                            font_size: FontSize::Px(FONT_SIZE),
                            ..default()
                        },
                        LineHeight::Px(LINE_HEIGHT),
                        TextColor(palette.color_for(highlight::HighlightKind::Default)),
                        Anchor::TOP_LEFT,
                        Transform::from_xyz(0.0, -(row as f32) * LINE_HEIGHT, 0.0),
                        LineRender {
                            text: slice.clone(),
                            rev: hl.rev,
                            palette_rev: palette.rev,
                        },
                    ))
                    .id();
                rebuild_line_spans(
                    commands, entity, children_q, hl, palette, rope, line, start_byte, &slice, font,
                );
                pool.0.insert(row, entity);
            }
        }
    }
}

fn rebuild_line_spans(
    commands: &mut Commands,
    parent: Entity,
    children_q: &Query<&Children>,
    hl: &Highlighter,
    palette: &SyntaxPalette,
    rope: &ropey::Rope,
    line_idx: usize,
    byte_offset: usize,
    line_text: &str,
    font: &EditorFont,
) {
    if let Ok(children) = children_q.get(parent) {
        for child in children.iter() {
            commands.entity(child).despawn();
        }
    }
    for (text, kind) in hl.line_chunks(rope, line_idx, byte_offset, line_text) {
        commands.spawn((
            ChildOf(parent),
            TextSpan::new(text),
            TextFont {
                font: (font.0.clone()).into(),
                font_size: FontSize::Px(FONT_SIZE),
                ..default()
            },
            LineHeight::Px(LINE_HEIGHT),
            TextColor(color_for(palette, kind)),
        ));
    }
}

fn effective_line_count(rope: &ropey::Rope) -> usize {
    let n = rope.len_lines();
    if n == 0 {
        1
    } else if n > 1 && rope.line(n - 1).len_chars() == 0 {
        n - 1
    } else {
        n
    }
}

/// Visible global-visual-row range `[first, last]` for a given scroll
/// and content height. Rows are `LINE_HEIGHT` tall.
fn viewport_row_range(content_height: f32, scroll: f32, total_rows: usize) -> (usize, usize) {
    if content_height <= 0.0 || total_rows == 0 {
        return (0, 0);
    }
    let top = scroll.max(0.0);
    let bottom = (scroll + content_height).max(0.0);
    let first = ((top / LINE_HEIGHT).floor().max(0.0) as usize).min(total_rows - 1);
    let last_excl = (bottom / LINE_HEIGHT).ceil().max(0.0) as usize;
    let last = last_excl.saturating_sub(1).min(total_rows - 1);
    (first, last)
}

/// Apply scroll Y to the scroll-root transform. (X scroll is handled
/// by per-line slicing inside sync_editor_lines.) The root sits at the
/// view's `origin` (pane chrome offset, or zero for a portal) plus the
/// current vertical scroll.
fn sync_content_root(
    editors: Query<(&EditorScroll, &EditorView), With<EditorStateComp>>,
    mut t_q: Query<&mut Transform>,
) {
    for (scroll, view) in &editors {
        if let Ok(mut t) = t_q.get_mut(view.render_root) {
            t.translation.x = view.origin.x;
            t.translation.y = view.origin.y + scroll.y;
        }
    }
}

fn sync_caret(
    metrics: Res<EditorMetrics>,
    theme: Res<jim_style::Theme>,
    editors: Query<
        (
            &EditorStateComp,
            &EditorView,
            &EditorScroll,
            Option<&EditorWrapLayout>,
            Option<&MarkdownMode>,
        ),
    >,
    carets: Query<(Entity, &EditorCaret)>,
    mut t_q: Query<&mut Transform>,
    mut vis_q: Query<&mut Visibility>,
    mut sprite_q: Query<&mut Sprite>,
) {
    let caret_color = Color::LinearRgba(theme.color(jim_style::tokens::CARET));
    let theme_changed = theme.is_changed();
    for (caret_entity, parent) in &carets {
        let Ok((state, view, scroll, layout, md)) = editors.get(parent.0) else {
            continue;
        };
        // Markdown WYSIWYG positions the caret itself (markdown_position).
        if wysiwyg_active(md) {
            continue;
        }
        let Some(layout) = layout else { continue };
        let head = state.0.selection.primary_range().head;
        let (line, col) = char_to_line_col(&state.0.doc, head);
        let (row, x_col) = layout.0.pos_to_visual(line, col);
        let content = view.size;

        // No horizontal scroll under wrap: the caret's x is its column
        // within the wrapped row.
        let x = x_col as f32 * metrics.cell_width;
        let y = row as f32 * LINE_HEIGHT;
        let visible = x >= 0.0
            && x <= content.x
            && (row as f32 + 1.0) * LINE_HEIGHT > scroll.y
            && (row as f32) * LINE_HEIGHT < scroll.y + content.y;

        if let Ok(mut t) = t_q.get_mut(caret_entity) {
            t.translation.x = x;
            t.translation.y = -y;
            t.translation.z = 1.0;
        }
        if let Ok(mut v) = vis_q.get_mut(caret_entity) {
            *v = if visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
        }
        if theme_changed {
            if let Ok(mut s) = sprite_q.get_mut(caret_entity) {
                s.color = caret_color;
            }
        }
    }
}

pub fn caret_x_in_line(col: usize, cell_width: f32) -> f32 {
    col as f32 * cell_width
}

pub fn char_to_line_col(doc: &ropey::Rope, char_idx: usize) -> (usize, usize) {
    let line = doc.char_to_line(char_idx);
    let line_start = doc.line_to_char(line);
    (line, char_idx - line_start)
}

fn sync_selection(
    metrics: Res<EditorMetrics>,
    theme: Res<jim_style::Theme>,
    editors: Query<
        (
            Entity,
            &EditorStateComp,
            &EditorView,
            Option<&EditorWrapLayout>,
            Option<&MarkdownMode>,
        ),
    >,
    existing: Query<(Entity, &SelRect)>,
    mut commands: Commands,
) {
    for (entity, _) in &existing {
        commands.entity(entity).despawn();
    }

    for (editor_entity, state, view, layout, md) in &editors {
        // Markdown WYSIWYG draws its own selection (markdown_position).
        if wysiwyg_active(md) {
            continue;
        }
        let Some(layout) = layout else { continue };
        let range = state.0.selection.primary_range();
        let (from, to) = (range.from(), range.to());
        if from == to {
            continue;
        }

        let (start_line, start_col) = char_to_line_col(&state.0.doc, from);
        let (end_line, end_col) = char_to_line_col(&state.0.doc, to);

        let content = view.size;
        let rope = &state.0.doc;
        for line in start_line..=end_line {
            let line_chars = line_char_len(rope, line);
            // Selected column range on this logical line.
            let lo = if line == start_line { start_col } else { 0 };
            let hi = if line == end_line { end_col } else { line_chars };
            // A line strictly before the selection end also selects its
            // trailing newline — show a small nub past the line's end.
            let ends_mid_doc = line < end_line;

            let Some(segs) = layout.0.line_segs.get(line) else {
                continue;
            };
            let base_row = layout.0.rows_before[line];
            for (seg_idx, &(s, e)) in segs.iter().enumerate() {
                // Intersect [lo, hi] with this segment's [s, e].
                let sel_lo = lo.max(s);
                let sel_hi = hi.min(e);
                if sel_hi < sel_lo {
                    continue;
                }
                let is_last_seg = seg_idx + 1 == segs.len();
                // Empty intersection only renders when it's the newline
                // nub at the line's true end (last segment).
                if sel_hi == sel_lo && !(ends_mid_doc && is_last_seg && sel_hi == e) {
                    continue;
                }
                let x0 = (sel_lo - s) as f32 * metrics.cell_width;
                let mut x1 = (sel_hi - s) as f32 * metrics.cell_width;
                if ends_mid_doc && is_last_seg && hi <= e {
                    x1 += LINE_HEIGHT * 0.3;
                }
                let x0 = x0.max(0.0);
                let x1 = x1.min(content.x);
                if x1 <= x0 {
                    continue;
                }
                let y_top = (base_row + seg_idx) as f32 * LINE_HEIGHT;
                commands.spawn((
                    SelRect {
                        editor: editor_entity,
                    },
                    ChildOf(view.render_root),
                    Sprite {
                        color: Color::LinearRgba(theme.color(jim_style::tokens::SELECTION)),
                        custom_size: Some(Vec2::new((x1 - x0).max(1.0), LINE_HEIGHT)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(x0, -y_top, 0.5),
                ));
            }
        }
    }
}

pub fn line_selection_span(lo_col: usize, hi_col: usize, cell_width: f32) -> (f32, f32) {
    (lo_col as f32 * cell_width, hi_col as f32 * cell_width)
}

fn line_char_len(rope: &ropey::Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        return 0;
    }
    let slice = rope.line(line);
    let n = slice.len_chars();
    if slice
        .chars_at(n)
        .reversed()
        .next()
        .is_some_and(|c| c == '\n')
    {
        n - 1
    } else {
        n
    }
}

fn handle_scroll(
    mut wheel: MessageReader<MouseWheel>,
    metrics: Res<EditorMetrics>,
    windows: Query<&Window>,
    keys: Res<ButtonInput<KeyCode>>,
    pane_zoom: Res<jim_pane::PaneZoom>,
    viewport: Res<jim_pane::PaneViewport>,
    all_panes: Query<(Entity, &PaneRect, Option<&Visibility>), With<PaneTag>>,
    mut editors: Query<
        (
            Entity,
            &PaneRect,
            &EditorStateComp,
            &mut EditorScroll,
            &PaneKindMarker,
            Option<&EditorWrapLayout>,
            Option<&MarkdownMode>,
            Option<&MdLayout>,
        ),
        With<PaneTag>,
    >,
) {
    let zoom = pane_zoom.0;
    // Cmd+scroll is the host's canvas pan gesture; don't double-scroll.
    if keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight) {
        wheel.clear();
        return;
    }
    let mut dx_px = 0.0;
    let mut dy_px = 0.0;
    for ev in wheel.read() {
        let (ux, uy) = match ev.unit {
            MouseScrollUnit::Pixel => (ev.x, ev.y),
            MouseScrollUnit::Line => (ev.x * metrics.cell_width, ev.y * LINE_HEIGHT),
        };
        dx_px += ux;
        dy_px += uy;
    }
    if dx_px == 0.0 && dy_px == 0.0 {
        return;
    }
    let Ok(win) = windows.single() else { return };
    let Some(pt) = win.cursor_position() else { return };

    // Topmost pane of ANY kind under the cursor. Bail if it isn't an
    // editor — otherwise scrolling on a widget/terminal that happens
    // to overlap an editor underneath would also scroll the editor.
    let all_rects: Vec<(Entity, PaneRect)> = all_panes
        .iter()
        .filter(|(_, _, vis)| !matches!(vis, Some(Visibility::Hidden)))
        .map(|(e, r, _)| (e, *r))
        .collect();
    let Some(editor) = jim_pane::topmost_pane_at(viewport.window_to_canvas(pt), &all_rects)
    else {
        return;
    };
    let Ok((_, _, _, _, kind, _, _, _)) = editors.get(editor) else {
        return;
    };
    if kind.0 != PANE_KIND {
        return;
    }
    let _ = dx_px; // soft-wrap: no horizontal scrolling

    if let Ok((_, rect, state, mut scroll, _, layout, md, md_layout)) = editors.get_mut(editor) {
        let content_size = content_area_size_zoomed(rect, zoom);
        let doc_height = if wysiwyg_active(md) {
            md_layout.map(|l| l.total_height).unwrap_or(0.0)
        } else {
            // Document height is total *visual* rows (wrapped), falling
            // back to logical lines until the layout's been built.
            let total_rows = layout
                .map(|l| l.0.total_rows())
                .unwrap_or_else(|| state.0.doc.len_lines());
            total_rows as f32 * LINE_HEIGHT
        };
        let y_max = (doc_height - content_size.y).max(0.0);
        scroll.y = (scroll.y - dy_px).clamp(0.0, y_max);
        scroll.x = 0.0;
    }
}

fn handle_input(
    mut keys: MessageReader<KeyboardInput>,
    mods: Res<ButtonInput<KeyCode>>,
    focused: Res<FocusedPane>,
    owner: Res<jim_pane::KeyboardOwner>,
    metrics: Option<Res<EditorMetrics>>,
    pane_zoom: Res<jim_pane::PaneZoom>,
    mut editors: Query<
        (
            &mut EditorStateComp,
            &PaneRect,
            &mut EditorScroll,
            &PaneKindMarker,
            Option<&EditorFilePath>,
            Option<&EditorWrapLayout>,
            Option<&mut MarkdownMode>,
            Option<&MdLayout>,
        ),
        With<PaneTag>,
    >,
) {
    let zoom = pane_zoom.0;
    let Some(target) = focused.0 else {
        keys.read().for_each(|_| {});
        return;
    };
    // A text modal (command palette / rename) owns the keyboard — don't
    // type into the focused editor while it's up.
    if !owner.allows_pane(target) {
        keys.read().for_each(|_| {});
        return;
    }
    let Ok((mut state_comp, rect, mut scroll, kind, file_path, wrap_layout, mut md, md_layout)) =
        editors.get_mut(target)
    else {
        keys.read().for_each(|_| {});
        return;
    };
    if kind.0 != PANE_KIND {
        keys.read().for_each(|_| {});
        return;
    }
    let state = &mut state_comp.0;
    let mut state_mutated = false;

    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);
    let alt = mods.pressed(KeyCode::AltLeft) || mods.pressed(KeyCode::AltRight);
    let meta = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);
    let mod_word = alt || ctrl;
    let mod_doc = meta || ctrl;

    for ev in keys.read() {
        if !ev.state.is_pressed() {
            continue;
        }

        let cmd_result = match ev.key_code {
            KeyCode::ArrowLeft => Some(if shift {
                if mod_word {
                    run(state, select_word_left)
                } else {
                    run(state, select_char_left)
                }
            } else if mod_word {
                run(state, cursor_word_left)
            } else {
                run(state, cursor_char_left)
            }),
            KeyCode::ArrowRight => Some(if shift {
                if mod_word {
                    run(state, select_word_right)
                } else {
                    run(state, select_char_right)
                }
            } else if mod_word {
                run(state, cursor_word_right)
            } else {
                run(state, cursor_char_right)
            }),
            // Cmd/Ctrl+/ toggles the rendered/raw markdown view.
            KeyCode::Slash if mod_doc => {
                if let Some(m) = md.as_deref_mut() {
                    if m.enabled {
                        m.raw = !m.raw;
                    }
                }
                Some(None)
            }
            KeyCode::ArrowUp => {
                let wys = md.as_deref().map(|m| m.enabled && !m.raw).unwrap_or(false);
                Some(if wys {
                    md_vertical_move(state, md_layout, shift, -1)
                } else {
                    visual_vertical_move(state, wrap_layout, shift, -1)
                })
            }
            KeyCode::ArrowDown => {
                let wys = md.as_deref().map(|m| m.enabled && !m.raw).unwrap_or(false);
                Some(if wys {
                    md_vertical_move(state, md_layout, shift, 1)
                } else {
                    visual_vertical_move(state, wrap_layout, shift, 1)
                })
            }
            KeyCode::Home => Some(if shift {
                if mod_doc {
                    run(state, select_doc_start)
                } else {
                    run(state, select_line_start)
                }
            } else if mod_doc {
                run(state, cursor_doc_start)
            } else {
                run(state, cursor_line_start)
            }),
            KeyCode::End => Some(if shift {
                if mod_doc {
                    run(state, select_doc_end)
                } else {
                    run(state, select_line_end)
                }
            } else if mod_doc {
                run(state, cursor_doc_end)
            } else {
                run(state, cursor_line_end)
            }),
            KeyCode::Backspace => Some(run_history(state, delete_char_backward)),
            KeyCode::Delete => Some(run_history(state, delete_char_forward)),
            KeyCode::Enter | KeyCode::NumpadEnter => {
                Some(run_history(state, insert_newline_and_indent))
            }
            KeyCode::Tab => Some(run_history(state, indent_more)),
            KeyCode::KeyA if mod_doc => Some(run(state, select_all)),
            KeyCode::KeyZ if mod_doc => Some(if shift {
                redo(state).map(|new| (new, true))
            } else {
                undo(state).map(|new| (new, true))
            }),
            KeyCode::KeyC if mod_doc => {
                copy_selection(state);
                Some(None)
            }
            KeyCode::KeyX if mod_doc => {
                copy_selection(state);
                Some(delete_selection(state))
            }
            // Paste is Cmd/Ctrl+V *without* Shift — Cmd+Shift+V is reserved
            // for app-global shortcuts (e.g. the profiler's vsync toggle) so
            // it must not leak into a focused editor as a paste.
            KeyCode::KeyV if mod_doc && !shift => Some(paste_from_clipboard(state)),
            // Cmd/Ctrl+S — save the buffer to its file. Side effect only
            // (no document change), so it returns `Some(None)`.
            KeyCode::KeyS if mod_doc => {
                match file_path {
                    Some(path) => {
                        let text = state.doc.to_string();
                        match std::fs::write(&path.0, text) {
                            Ok(()) => eprintln!("[editor] saved {}", path.0.display()),
                            Err(e) => {
                                eprintln!("[editor] save failed {}: {}", path.0.display(), e)
                            }
                        }
                    }
                    None => eprintln!("[editor] save: no file path for this pane"),
                }
                Some(None)
            }
            _ => None,
        };

        if let Some(Some((new_state, _))) = cmd_result {
            *state = new_state;
            state_mutated = true;
            continue;
        }
        if let Some(None) = cmd_result {
            continue;
        }

        if mod_doc || alt {
            continue;
        }
        let text: Option<String> = match &ev.logical_key {
            Key::Character(s) => Some(s.chars().take(1).collect()),
            Key::Space => Some(" ".into()),
            _ => None,
        };
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            let tr = Transaction::new()
                .change(Change::new(
                    state.selection.primary_range().from(),
                    state.selection.primary_range().to(),
                    text.clone(),
                ))
                .select(Selection::cursor(
                    state.selection.primary_range().from() + text.chars().count(),
                ));
            *state = state.apply_with_history(&tr);
            state_mutated = true;
        }
    }

    if state_mutated {
        let wys = md.as_deref().map(|m| m.enabled && !m.raw).unwrap_or(false);
        if wys {
            if let Some(layout) = md_layout {
                let head = state.selection.primary_range().head;
                if let Some((_x, y, h)) = markdown::caret_pos(layout, head) {
                    let content_h = content_area_size(rect).y;
                    if y < scroll.y {
                        scroll.y = y;
                    } else if y + h > scroll.y + content_h {
                        scroll.y = y + h - content_h;
                    }
                    scroll.y = scroll.y.max(0.0);
                }
            }
        } else if let Some(layout) = wrap_layout {
            ensure_caret_visible(state, content_area_size_zoomed(rect, zoom), &mut scroll, &layout.0);
        }
    }
    let _ = metrics;
}

/// Compute the result of an Up/Down (or shift-extended) caret move that
/// respects soft-wrap: motion is by *visual* row, not logical line.
/// Falls back to logical-line motion if the layout isn't built yet.
fn visual_vertical_move(
    state: &EditorState,
    layout: Option<&EditorWrapLayout>,
    shift: bool,
    delta: i32,
) -> Option<(EditorState, bool)> {
    let Some(layout) = layout else {
        let cmd = match (shift, delta < 0) {
            (true, true) => select_line_up,
            (true, false) => select_line_down,
            (false, true) => cursor_line_up,
            (false, false) => cursor_line_down,
        };
        return run(state, cmd);
    };
    let layout = &layout.0;
    let range = state.selection.primary_range();
    let (line, col) = char_to_line_col(&state.doc, range.head);
    let (row, x_col) = layout.pos_to_visual(line, col);
    let total = layout.total_rows();
    let target_row = if delta < 0 {
        row.saturating_sub(1)
    } else {
        (row + 1).min(total.saturating_sub(1))
    };
    if target_row == row {
        return None; // already at the top/bottom visual row
    }
    let (tline, tcol) = layout.visual_to_pos(target_row, x_col);
    let new_head = char_from_line_col(state, tline, tcol);
    let new_sel = if shift {
        Selection::single(Range::new(range.anchor, new_head))
    } else {
        Selection::cursor(new_head)
    };
    let tr = Transaction::new().select(new_sel);
    Some((state.apply(&tr), true))
}

/// Markdown-mode vertical caret motion, driven by the WYSIWYG glyph
/// layout instead of the monospace grid.
fn md_vertical_move(
    state: &EditorState,
    layout: Option<&MdLayout>,
    shift: bool,
    delta: i32,
) -> Option<(EditorState, bool)> {
    let layout = layout?;
    let range = state.selection.primary_range();
    let new_head = markdown::vertical_move(layout, range.head, delta)?;
    let new_sel = if shift {
        Selection::single(Range::new(range.anchor, new_head))
    } else {
        Selection::cursor(new_head)
    };
    let tr = Transaction::new().select(new_sel);
    Some((state.apply(&tr), true))
}

pub fn mouse_col_at_x(local_x: f32, cell_width: f32) -> usize {
    if cell_width <= 0.0 {
        return 0;
    }
    let col = (local_x / cell_width + 0.5).floor();
    if col <= 0.0 { 0 } else { col as usize }
}

pub fn char_from_line_col(state: &EditorState, line: usize, col: usize) -> usize {
    let n_lines = state.doc.len_lines().max(1);
    let last_line = n_lines - 1;
    let line = line.min(last_line);
    let line_start = state.doc.line_to_char(line);
    let line_chars = line_char_len(&state.doc, line);
    line_start + col.min(line_chars)
}

// ---------- Content-area input (pane-bevy events) ----------

/// React to `PaneContentPressed` for editor panes: place caret or
/// extend selection from the previous anchor (shift held).
fn handle_pane_content_press(
    mut presses: MessageReader<PaneContentPressed>,
    metrics: Option<Res<EditorMetrics>>,
    mut editors: Query<
        (
            &mut EditorStateComp,
            &EditorScroll,
            &mut TextDragAnchor,
            &PaneKindMarker,
            Option<&EditorWrapLayout>,
            Option<&MarkdownMode>,
            Option<&MdLayout>,
        ),
        With<PaneTag>,
    >,
) {
    let Some(metrics) = metrics else {
        presses.read().for_each(|_| {});
        return;
    };
    for ev in presses.read() {
        let Ok((mut state_comp, scroll, mut drag, kind, layout, md, md_layout)) =
            editors.get_mut(ev.pane)
        else {
            continue;
        };
        if kind.0 != PANE_KIND {
            continue;
        }
        let state = &mut state_comp.0;
        let pos = if wysiwyg_active(md) {
            let Some(md_layout) = md_layout else { continue };
            markdown::offset_at_point(md_layout, ev.local_pt.x, ev.local_pt.y + scroll.y)
                .unwrap_or(0)
        } else {
            let Some(layout) = layout else { continue };
            // No horizontal scroll under wrap; only scroll.y offsets rows.
            let row = ((ev.local_pt.y + scroll.y) / LINE_HEIGHT).floor().max(0.0) as usize;
            let x_col = mouse_col_at_x(ev.local_pt.x, metrics.cell_width);
            let (line, col) = layout.0.visual_to_pos(row, x_col);
            char_from_line_col(state, line, col)
        };
        if ev.shift {
            let anchor = state.selection.primary_range().anchor;
            drag.0 = Some(anchor);
            *state = apply_selection(state, anchor, pos);
        } else {
            drag.0 = Some(pos);
            *state = apply_selection(state, pos, pos);
        }
    }
}

/// Each frame while LMB is held, drag the head of any in-progress
/// text selection. Cleared on release.
fn handle_text_select_drag(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    metrics: Option<Res<EditorMetrics>>,
    viewport: Res<jim_pane::PaneViewport>,
    mut editors: Query<
        (
            &mut EditorStateComp,
            &PaneRect,
            &EditorScroll,
            &mut TextDragAnchor,
            &PaneKindMarker,
            Option<&EditorWrapLayout>,
            Option<&MarkdownMode>,
            Option<&MdLayout>,
        ),
        With<PaneTag>,
    >,
) {
    let Some(metrics) = metrics else {
        return;
    };
    if buttons.just_released(MouseButton::Left) {
        for (_, _, _, mut drag, kind, _, _, _) in &mut editors {
            if kind.0 == PANE_KIND {
                drag.0 = None;
            }
        }
        return;
    }
    if !buttons.pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else { return };
    let Some(pt) = window.cursor_position() else { return };
    let pt_canvas = viewport.window_to_canvas(pt);

    for (mut state_comp, rect, scroll, drag, kind, layout, md, md_layout) in &mut editors {
        if kind.0 != PANE_KIND {
            continue;
        }
        let Some(anchor) = drag.0 else { continue };
        let local = jim_pane::pt_to_content_local(pt_canvas, rect);
        let head = if wysiwyg_active(md) {
            let Some(md_layout) = md_layout else { continue };
            markdown::offset_at_point(md_layout, local.x, local.y + scroll.y)
                .unwrap_or(anchor)
        } else {
            let Some(layout) = layout else { continue };
            let row = ((local.y + scroll.y) / LINE_HEIGHT).floor().max(0.0) as usize;
            let x_col = mouse_col_at_x(local.x, metrics.cell_width);
            let (line, col) = layout.0.visual_to_pos(row, x_col);
            char_from_line_col(&state_comp.0, line, col)
        };
        let cur = state_comp.0.selection.primary_range();
        if cur.anchor != anchor || cur.head != head {
            state_comp.0 = apply_selection(&state_comp.0, anchor, head);
        }
    }
}

fn apply_selection(state: &EditorState, anchor: usize, head: usize) -> EditorState {
    let tr = Transaction::new().select(Selection::single(Range::new(anchor, head)));
    state.apply_with_history(&tr)
}

// ---------- Embedded editors (portals) ----------
//
// A real, live editor mounted *inside* something that isn't a floating
// pane — currently a funct widget. The host owns geometry (it positions
// the portal's scroll-root at a layout rect and fills `EditorView`); this
// crate owns the editor itself (text, caret, selection, scroll, syntax,
// keyboard). All the render systems are already mount-agnostic (they key
// off `EditorStateComp` + `EditorView`), so a portal only needs its own
// *input* path — pane input filters on `With<PaneTag>`, which portals
// lack. The systems below are that path, driven by host-sent messages
// (pointer) and `FocusedEmbeddedEditor` (keyboard).

/// Marker for an embedded editor (portal) entity, as opposed to a pane
/// editor. The entity itself carries `EditorStateComp` + `EditorView`
/// and is a child of the host's content root.
#[derive(Component)]
pub struct EmbeddedEditor;

/// Read-only embedded editor: keyboard caret motion, selection, copy, and
/// scroll all still work (so any multi-line text dump gets real cosmic-text
/// selection + an I-beam), but every document mutation — typing, delete,
/// paste, undo/redo, save — is dropped. The host can still replace the
/// whole buffer via [`set_embedded_editor_text`] (e.g. to push new output).
#[derive(Component)]
pub struct EditorReadOnly;

/// The embedded editor that currently owns the keyboard, if any. The
/// host sets this when a portal is clicked and clears it when focus
/// leaves the host pane. Distinct from `FocusedPane`: a portal lives
/// inside a widget pane, so the *pane* keeps pane-focus while the portal
/// owns text input.
#[derive(Resource, Default)]
pub struct FocusedEmbeddedEditor(pub Option<Entity>);

/// Host → editor: a pointer pressed inside a portal at `local_pt`
/// (scroll-root local space, top-left origin, pre-scroll). Places the
/// caret / starts a selection — the portal analogue of
/// `PaneContentPressed`.
#[derive(Message, Clone, Copy, Debug)]
pub struct EmbeddedEditorPress {
    pub editor: Entity,
    pub local_pt: Vec2,
    pub shift: bool,
}

/// Host → editor: pointer dragged (button held) over a portal; extends
/// the in-progress selection to `local_pt`.
#[derive(Message, Clone, Copy, Debug)]
pub struct EmbeddedEditorDrag {
    pub editor: Entity,
    pub local_pt: Vec2,
}

/// Host → editor: the pointer button was released; ends any selection drag.
#[derive(Message, Clone, Copy, Debug)]
pub struct EmbeddedEditorRelease;

/// Host → editor: wheel scrolled over a portal by `dy` pixels (positive =
/// content moves up, same sign convention as the pane scroll handler).
#[derive(Message, Clone, Copy, Debug)]
pub struct EmbeddedEditorScroll {
    pub editor: Entity,
    pub dy: f32,
}

/// Editor → host: the user hit the submit/"run" chord (Cmd/Ctrl+Enter)
/// while a portal was focused. Carries the current selection text (empty
/// if nothing is selected) and the full buffer, so a host can run
/// "selection if non-empty else whole buffer" with no extra round-trip.
/// Drives the funct `on_editor_submit(id, selection, full)` handler. Fired
/// for read-only editors too — it's non-mutating.
#[derive(Message, Clone, Debug)]
pub struct EmbeddedEditorSubmit {
    pub editor: Entity,
    pub selection: String,
    pub full: String,
}

/// Config for [`spawn_embedded_editor`].
pub struct EmbeddedEditorConfig {
    pub text: String,
    /// File to save to on Cmd/Ctrl+S. `None` for value-backed editors.
    pub path: Option<PathBuf>,
    /// Language hint (informational; the highlighter is Rust-only today,
    /// matching the pane editor).
    pub lang: Option<String>,
    /// Initial viewport size; the host keeps `EditorView.size` current.
    pub size: Vec2,
    pub caret_color: Color,
    /// Selectable but not editable (see [`EditorReadOnly`]).
    pub read_only: bool,
}

/// Spawn a live embedded editor under `parent` (the host's content root).
/// Returns the **container** entity, which carries `EditorStateComp` and
/// is what the host positions each frame; its child scroll-root is
/// `EditorView.render_root`. The host tracks the returned entity so it
/// survives the widget re-render diff.
pub fn spawn_embedded_editor(
    commands: &mut Commands,
    parent: Entity,
    cfg: EmbeddedEditorConfig,
) -> Entity {
    let _ = &cfg.lang; // highlighter is Rust-only for now (see EmbeddedEditorConfig)
    let container = commands
        .spawn((
            EmbeddedEditor,
            EditorStateComp(
                EditorState::new(ropey::Rope::from_str(&cfg.text), Selection::cursor(0))
                    .with_indent_unit("    "),
            ),
            EditorHighlighter(Highlighter::new()),
            LineRows::default(),
            EditorScroll::default(),
            TextDragAnchor::default(),
            ChildOf(parent),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();
    let scroll_root = commands
        .spawn((
            ChildOf(container),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();
    commands.entity(container).insert(EditorView {
        render_root: scroll_root,
        size: cfg.size,
        origin: Vec2::ZERO,
    });
    commands.spawn((
        ChildOf(scroll_root),
        EditorCaret(container),
        Sprite {
            color: cfg.caret_color,
            custom_size: Some(Vec2::new(2.0, LINE_HEIGHT)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(0.0, 0.0, 1.0),
    ));
    if let Some(path) = cfg.path {
        commands.entity(container).insert(EditorFilePath(path));
    }
    if cfg.read_only {
        commands.entity(container).insert(EditorReadOnly);
    }
    container
}

/// Replace a portal's whole document (value-backed two-way resync). Drops
/// undo history and resets the caret — intended for when the *script*
/// changes the bound value, not for live typing.
pub fn set_embedded_editor_text(state_comp: &mut EditorStateComp, text: &str) {
    state_comp.0 = EditorState::new(ropey::Rope::from_str(text), Selection::cursor(0))
        .with_indent_unit("    ");
}

/// Safely resync a live portal to a new buffer (the script changed the
/// bound `value`). Beyond swapping the text + caret, this resets scroll to
/// the top and **drops the cached [`EditorWrapLayout`]**: the new buffer may
/// be shorter than the old, and every render system reads the wrap layout
/// for its visual-row→line map. If the layout still described the old
/// (longer) doc, `sync_editor_lines` would index a now-out-of-range line
/// against the shorter rope and panic (`rope.line(N)` past the end). All the
/// render systems guard `Some(layout)`, so dropping it makes them skip for
/// the one frame until `compute_wrap_layout` rebuilds it against the new
/// rope. Use this — not raw `EditorStateComp` mutation — for value resync.
pub fn resync_embedded_editor(world: &mut World, editor: Entity, text: &str) {
    let Some(mut sc) = world.get_mut::<EditorStateComp>(editor) else {
        return;
    };
    set_embedded_editor_text(&mut sc, text);
    if let Some(mut scroll) = world.get_mut::<EditorScroll>(editor) {
        scroll.x = 0.0;
        scroll.y = 0.0;
    }
    world.entity_mut(editor).remove::<EditorWrapLayout>();
}

fn handle_embedded_press(
    mut presses: MessageReader<EmbeddedEditorPress>,
    metrics: Option<Res<EditorMetrics>>,
    mut editors: Query<
        (
            &mut EditorStateComp,
            &EditorScroll,
            &mut TextDragAnchor,
            Option<&EditorWrapLayout>,
        ),
        With<EmbeddedEditor>,
    >,
) {
    let Some(metrics) = metrics else {
        presses.read().for_each(|_| {});
        return;
    };
    for ev in presses.read() {
        let Ok((mut sc, scroll, mut drag, layout)) = editors.get_mut(ev.editor) else {
            continue;
        };
        let Some(layout) = layout else { continue };
        let state = &mut sc.0;
        let row = ((ev.local_pt.y + scroll.y) / LINE_HEIGHT).floor().max(0.0) as usize;
        let x_col = mouse_col_at_x(ev.local_pt.x, metrics.cell_width);
        let (line, col) = layout.0.visual_to_pos(row, x_col);
        let pos = char_from_line_col(state, line, col);
        if ev.shift {
            let anchor = state.selection.primary_range().anchor;
            drag.0 = Some(anchor);
            *state = apply_selection(state, anchor, pos);
        } else {
            drag.0 = Some(pos);
            *state = apply_selection(state, pos, pos);
        }
    }
}

fn handle_embedded_drag(
    mut drags: MessageReader<EmbeddedEditorDrag>,
    metrics: Option<Res<EditorMetrics>>,
    mut editors: Query<
        (
            &mut EditorStateComp,
            &EditorScroll,
            &TextDragAnchor,
            Option<&EditorWrapLayout>,
        ),
        With<EmbeddedEditor>,
    >,
) {
    let Some(metrics) = metrics else {
        drags.read().for_each(|_| {});
        return;
    };
    for ev in drags.read() {
        let Ok((mut sc, scroll, drag, layout)) = editors.get_mut(ev.editor) else {
            continue;
        };
        let Some(anchor) = drag.0 else { continue };
        let Some(layout) = layout else { continue };
        let row = ((ev.local_pt.y + scroll.y) / LINE_HEIGHT).floor().max(0.0) as usize;
        let x_col = mouse_col_at_x(ev.local_pt.x, metrics.cell_width);
        let (line, col) = layout.0.visual_to_pos(row, x_col);
        let head = char_from_line_col(&sc.0, line, col);
        let cur = sc.0.selection.primary_range();
        if cur.anchor != anchor || cur.head != head {
            sc.0 = apply_selection(&sc.0, anchor, head);
        }
    }
}

fn handle_embedded_release(
    mut rel: MessageReader<EmbeddedEditorRelease>,
    mut editors: Query<&mut TextDragAnchor, With<EmbeddedEditor>>,
) {
    if rel.read().count() == 0 {
        return;
    }
    for mut drag in &mut editors {
        drag.0 = None;
    }
}

fn handle_embedded_scroll(
    mut scrolls: MessageReader<EmbeddedEditorScroll>,
    mut editors: Query<
        (
            &EditorView,
            &mut EditorScroll,
            &EditorStateComp,
            Option<&EditorWrapLayout>,
        ),
        With<EmbeddedEditor>,
    >,
) {
    for ev in scrolls.read() {
        let Ok((view, mut scroll, sc, layout)) = editors.get_mut(ev.editor) else {
            continue;
        };
        let total_rows = layout
            .map(|l| l.0.total_rows())
            .unwrap_or_else(|| sc.0.doc.len_lines());
        let doc_height = total_rows as f32 * LINE_HEIGHT;
        let y_max = (doc_height - view.size.y).max(0.0);
        scroll.y = (scroll.y - ev.dy).clamp(0.0, y_max);
        scroll.x = 0.0;
    }
}

/// Keyboard for the focused portal. Mirrors `handle_input`'s non-markdown
/// arms (kept separate so the pane editor stays untouched). `KeyboardInput`
/// is read per-system, so this and `handle_input` never fight over events:
/// `handle_input` no-ops unless `FocusedPane` is an editor *pane*, and this
/// no-ops unless `FocusedEmbeddedEditor` is set.
fn handle_embedded_keys(
    mut keys: MessageReader<KeyboardInput>,
    mods: Res<ButtonInput<KeyCode>>,
    focused: Res<FocusedEmbeddedEditor>,
    mut submit_w: MessageWriter<EmbeddedEditorSubmit>,
    mut editors: Query<
        (
            &mut EditorStateComp,
            &EditorView,
            &mut EditorScroll,
            Option<&EditorWrapLayout>,
            Option<&EditorFilePath>,
            Option<&EditorReadOnly>,
        ),
        With<EmbeddedEditor>,
    >,
) {
    let Some(target) = focused.0 else {
        keys.read().for_each(|_| {});
        return;
    };
    let Ok((mut state_comp, view, mut scroll, wrap_layout, file_path, read_only)) =
        editors.get_mut(target)
    else {
        keys.read().for_each(|_| {});
        return;
    };
    let read_only = read_only.is_some();
    let state = &mut state_comp.0;
    let mut state_mutated = false;

    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);
    let alt = mods.pressed(KeyCode::AltLeft) || mods.pressed(KeyCode::AltRight);
    let meta = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);
    let mod_word = alt || ctrl;
    let mod_doc = meta || ctrl;

    for ev in keys.read() {
        if !ev.state.is_pressed() {
            continue;
        }
        // Submit/"run" chord: Cmd/Ctrl+Enter forwards the buffer to the host
        // (drives `on_editor_submit`) instead of inserting a newline. Fires
        // for read-only editors too (non-mutating). Pre-empts the plain
        // Enter arm below.
        if matches!(ev.key_code, KeyCode::Enter | KeyCode::NumpadEnter) && mod_doc {
            let range = state.selection.primary_range();
            let selection = if range.from() != range.to() {
                state.doc.slice(range.from()..range.to()).to_string()
            } else {
                String::new()
            };
            submit_w.write(EmbeddedEditorSubmit {
                editor: target,
                selection,
                full: state.doc.to_string(),
            });
            continue;
        }
        let cmd_result: Option<Option<(EditorState, bool)>> = match ev.key_code {
            KeyCode::ArrowLeft => Some(if shift {
                if mod_word {
                    run(state, select_word_left)
                } else {
                    run(state, select_char_left)
                }
            } else if mod_word {
                run(state, cursor_word_left)
            } else {
                run(state, cursor_char_left)
            }),
            KeyCode::ArrowRight => Some(if shift {
                if mod_word {
                    run(state, select_word_right)
                } else {
                    run(state, select_char_right)
                }
            } else if mod_word {
                run(state, cursor_word_right)
            } else {
                run(state, cursor_char_right)
            }),
            KeyCode::ArrowUp => Some(visual_vertical_move(state, wrap_layout, shift, -1)),
            KeyCode::ArrowDown => Some(visual_vertical_move(state, wrap_layout, shift, 1)),
            KeyCode::Home => Some(if shift {
                if mod_doc {
                    run(state, select_doc_start)
                } else {
                    run(state, select_line_start)
                }
            } else if mod_doc {
                run(state, cursor_doc_start)
            } else {
                run(state, cursor_line_start)
            }),
            KeyCode::End => Some(if shift {
                if mod_doc {
                    run(state, select_doc_end)
                } else {
                    run(state, select_line_end)
                }
            } else if mod_doc {
                run(state, cursor_doc_end)
            } else {
                run(state, cursor_line_end)
            }),
            KeyCode::Backspace => Some(run_history(state, delete_char_backward)),
            KeyCode::Delete => Some(run_history(state, delete_char_forward)),
            KeyCode::Enter | KeyCode::NumpadEnter => {
                Some(run_history(state, insert_newline_and_indent))
            }
            KeyCode::Tab => Some(run_history(state, indent_more)),
            KeyCode::KeyA if mod_doc => Some(run(state, select_all)),
            KeyCode::KeyZ if mod_doc => Some(if shift {
                redo(state).map(|new| (new, true))
            } else {
                undo(state).map(|new| (new, true))
            }),
            KeyCode::KeyC if mod_doc => {
                copy_selection(state);
                Some(None)
            }
            KeyCode::KeyX if mod_doc => {
                copy_selection(state);
                Some(delete_selection(state))
            }
            KeyCode::KeyV if mod_doc && !shift => Some(paste_from_clipboard(state)),
            KeyCode::KeyS if mod_doc => {
                match file_path {
                    Some(path) if !read_only => {
                        let text = state.doc.to_string();
                        match std::fs::write(&path.0, text) {
                            Ok(()) => eprintln!("[editor] saved {}", path.0.display()),
                            Err(e) => eprintln!("[editor] save failed {}: {}", path.0.display(), e),
                        }
                    }
                    _ => {}
                }
                Some(None)
            }
            _ => None,
        };

        if let Some(Some((new_state, _))) = cmd_result {
            // Read-only: keep caret/selection moves (doc unchanged) but
            // drop any command that would mutate the document (undo/redo,
            // cut, …).
            if read_only && new_state.doc != state.doc {
                continue;
            }
            *state = new_state;
            state_mutated = true;
            continue;
        }
        if let Some(None) = cmd_result {
            continue;
        }
        if mod_doc || alt || read_only {
            continue;
        }
        let text: Option<String> = match &ev.logical_key {
            Key::Character(s) => Some(s.chars().take(1).collect()),
            Key::Space => Some(" ".into()),
            _ => None,
        };
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            let tr = Transaction::new()
                .change(Change::new(
                    state.selection.primary_range().from(),
                    state.selection.primary_range().to(),
                    text.clone(),
                ))
                .select(Selection::cursor(
                    state.selection.primary_range().from() + text.chars().count(),
                ));
            *state = state.apply_with_history(&tr);
            state_mutated = true;
        }
    }

    if state_mutated {
        if let Some(layout) = wrap_layout {
            ensure_caret_visible(state, view.size, &mut scroll, &layout.0);
        }
    }
}

fn copy_selection(state: &EditorState) {
    let range = state.selection.primary_range();
    if range.from() == range.to() {
        return;
    }
    let text = state.doc.slice(range.from()..range.to()).to_string();
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text);
    }
}

fn delete_selection(state: &EditorState) -> Option<(EditorState, bool)> {
    let range = state.selection.primary_range();
    if range.from() == range.to() {
        return None;
    }
    let tr = Transaction::new()
        .change(Change::new(range.from(), range.to(), String::new()))
        .select(Selection::cursor(range.from()));
    Some((state.apply_with_history(&tr), true))
}

fn paste_from_clipboard(state: &EditorState) -> Option<(EditorState, bool)> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let text = cb.get_text().ok()?;
    if text.is_empty() {
        return None;
    }
    let range = state.selection.primary_range();
    let end = range.from() + text.chars().count();
    let tr = Transaction::new()
        .change(Change::new(range.from(), range.to(), text))
        .select(Selection::cursor(end));
    Some((state.apply_with_history_isolated(&tr), true))
}

fn run(
    state: &EditorState,
    cmd: fn(&EditorState) -> Option<Transaction>,
) -> Option<(EditorState, bool)> {
    cmd(state).map(|tr| (state.apply(&tr), true))
}

fn run_history(
    state: &EditorState,
    cmd: fn(&EditorState) -> Option<Transaction>,
) -> Option<(EditorState, bool)> {
    cmd(state).map(|tr| (state.apply_with_history(&tr), true))
}
