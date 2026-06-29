//! WYSIWYG markdown mode for the editor pane (Typora-like).
//!
//! When the open file is `*.md`/`*.markdown`, the pane gains a
//! [`MarkdownMode`]. While *rendered* (the default; toggle raw with
//! `Cmd+/`), the editor stops using the monospace-grid renderer and
//! instead lays out the document with `markdown_core`:
//!
//! - Each source line becomes a `Text2d` "render line" with one
//!   `TextSpan` per styled run. Bevy lays it out (variable font sizes /
//!   faces, real wrapping).
//! - [`markdown_readback`] runs after Bevy's text layout and reads each
//!   line's `TextLayoutInfo.glyphs` back into an [`MdLayout`]: a map from
//!   document char offset to on-screen rect. Caret, selection, click
//!   hit-testing and vertical motion all read that map, so they line up
//!   with whatever Bevy actually drew — no monospace assumption.
//! - Markdown syntax (`#`, `**`, `` ` ``, …) is hidden in inactive blocks
//!   and revealed (dimmed) in the block holding the caret.
//!
//! The geometry feeds back with a one-frame latency (readback in
//! `PostUpdate`, consumed next `Update`), which is imperceptible at
//! interactive rates and matches how the grid renderer already treats
//! clicks (prior-frame layout).

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::text::{
    FontWeight, Justify, LineBreak, LineHeight, TextBounds, TextLayout, TextLayoutInfo, TextSpan,
};
use markdown_core::{Block, BlockKind, Document, InlineStyle, RenderLine, RunKind};
use ropey::Rope;

use crate::highlight::{HighlightKind, Highlighter, SyntaxPalette};
use crate::{content_area_size, EditorCaret, EditorScroll, EditorStateComp};
use jim_pane::{PaneChrome, PaneKindMarker, PaneRect, PaneTag, MARGIN, TITLE_H};

// ---------- Tunables ----------

const BODY_SIZE: f32 = 17.0;
const CODE_SIZE: f32 = 15.0;
const LINE_SPACING: f32 = 1.45;
/// Horizontal indent per nesting level (blockquote / list).
const INDENT_PX: f32 = 24.0;
/// Extra left pad inside code blocks / blockquotes.
const BLOCK_PAD: f32 = 10.0;


fn heading_size(level: u8) -> f32 {
    match level {
        1 => 30.0,
        2 => 25.0,
        3 => 21.0,
        4 => 19.0,
        _ => 18.0,
    }
}

/// Row (line) height for a render line, given its dominant font size.
fn row_height_for(size: f32) -> f32 {
    (size * LINE_SPACING).round()
}

// ---------- Components / resources ----------

/// Present on editor panes whose file is markdown. `raw` toggles the
/// rendered/source view (Cmd+/).
#[derive(Component, Clone, Copy, Debug)]
pub struct MarkdownMode {
    pub enabled: bool,
    pub raw: bool,
}

/// Is the WYSIWYG render path active for this editor right now?
pub fn wysiwyg_active(md: Option<&MarkdownMode>) -> bool {
    md.map(|m| m.enabled && !m.raw).unwrap_or(false)
}

/// Cached parse of the document, rebuilt on doc change.
#[derive(Component)]
pub struct MdDoc(Document);

/// Render-state bookkeeping so we only rebuild span entities when
/// something visible changes.
#[derive(Component, Default)]
pub struct MdState {
    /// Block index currently revealed (caret inside), or -1. Compared
    /// each frame so we only rebuild span entities when the revealed
    /// block actually changes.
    active: i64,
}

/// Pool of render-line entities, in document render-line order.
#[derive(Component, Default)]
pub struct MdLines(Vec<Entity>);

/// Per-render-line entity: which editor + the rendered runs, so readback
/// can map glyphs (span_index, byte_index) back to document char offsets.
#[derive(Component)]
pub struct MdLineRef {
    editor: Entity,
    /// One entry per *rendered* span (in child order). `(src_start,
    /// rendered_text)`. Rendered text maps 1:1 by char to source chars.
    spans: Vec<(usize, String)>,
    /// Dominant row height for this line.
    row_height: f32,
    /// Left indent applied to this line.
    x_offset: f32,
    /// Source char range of the line (excl. trailing newline).
    src: std::ops::Range<usize>,
    /// Block this line belongs to (kind + index + reveal state) so the
    /// decoration pass can draw backgrounds / bars / bullets / rules.
    kind: BlockKind,
    block: usize,
    active: bool,
    /// True for hidden code-fence lines — collapse them to zero height so
    /// a code block has no blank gap (but keep blank lines *inside* code).
    collapse: bool,
}

/// Marker for selection-highlight sprites in markdown mode.
#[derive(Component)]
pub struct MdSel(Entity);

/// Marker for block-decoration sprites (code-block backgrounds,
/// blockquote bars, list bullets, thematic-break rules). Rebuilt every
/// frame in `markdown_position`.
#[derive(Component)]
pub struct MdDecor(Entity);

/// Geometry read back from Bevy's text layout. Drives caret / selection
/// / click. Rebuilt every frame in `PostUpdate`.
#[derive(Component, Default)]
pub struct MdLayout {
    pub lines: Vec<MdLineGeom>,
    pub total_height: f32,
}

pub struct MdLineGeom {
    pub src: std::ops::Range<usize>,
    /// Top of the line, as a content-top-down distance (y grows down).
    pub top: f32,
    pub height: f32,
    pub row_height: f32,
    pub x_offset: f32,
    pub glyphs: Vec<MdGlyph>,
    pub kind: BlockKind,
    pub block: usize,
    pub active: bool,
}

pub struct MdGlyph {
    pub src_char: usize,
    pub src_len: usize,
    /// Horizontal extent in content-local space (already includes the
    /// line's x_offset).
    pub left: f32,
    pub right: f32,
    /// Visual row within the line (0-based; >0 for wrapped lines).
    pub row: usize,
}

/// Bundled font handles for markdown rendering.
#[derive(Resource, Clone)]
pub struct MarkdownFonts {
    /// Upright variable font (regular + bold via weight).
    pub body: Handle<Font>,
    /// Italic variable font (italic + bold-italic via weight).
    pub italic: Handle<Font>,
    /// Monospace, for code.
    pub mono: Handle<Font>,
}

const INTER_VF: &[u8] = include_bytes!("../assets/fonts/Inter-VF.ttf");
const INTER_ITALIC_VF: &[u8] = include_bytes!("../assets/fonts/Inter-Italic-VF.ttf");

/// A reusable tree-sitter highlighter for fenced code blocks (Rust). One
/// instance, reparsed per code block during a render rebuild.
#[derive(Resource)]
pub struct MarkdownCodeHl(pub Highlighter);

pub fn setup_markdown_fonts(
    mut commands: Commands,
    mut fonts: ResMut<Assets<Font>>,
    editor_font: Option<Res<crate::EditorFont>>,
) {
    commands.insert_resource(MarkdownCodeHl(Highlighter::new()));
    let body = fonts.add(Font::from_bytes(INTER_VF.to_vec()));
    let italic =
        fonts.add(Font::from_bytes(INTER_ITALIC_VF.to_vec()));
    // Reuse the editor's mono font handle when present; else load ours.
    let mono = match editor_font {
        Some(f) => f.0.clone(),
        None => fonts.add(
            Font::from_bytes(crate::EMBEDDED_FONT.to_vec()),
        ),
    };
    commands.insert_resource(MarkdownFonts { body, italic, mono });
}

// ---------- Style resolution ----------

struct ResolvedStyle {
    font: Handle<Font>,
    weight: FontWeight,
    size: f32,
    color: Color,
}

struct MdColors {
    body: Color,
    marker: Color,
    code: Color,
    link: Color,
    heading: Color,
}

impl Default for MdColors {
    /// Sensible dark-theme fallback when no [`jim_style::Theme`] resource
    /// is present (e.g. a host without `StylePlugin`).
    fn default() -> Self {
        Self {
            body: Color::srgb(0.90, 0.91, 0.93),
            marker: Color::srgb(0.48, 0.52, 0.58),
            code: Color::srgb(0.65, 0.87, 0.60),
            link: Color::srgb(0.55, 0.78, 1.0),
            heading: Color::srgb(0.96, 0.97, 0.99),
        }
    }
}

impl MdColors {
    fn from_theme(theme: &jim_style::Theme) -> Self {
        let c = |id| Color::LinearRgba(theme.color(id));
        Self {
            body: c(jim_style::tokens::FG),
            marker: c(jim_style::tokens::FG_MUTED),
            code: c(jim_style::tokens::SYNTAX_STRING),
            link: c(jim_style::tokens::ACCENT),
            heading: c(jim_style::tokens::FG),
        }
    }
}

/// Resolve a run's font/weight/size/color from its kind + block context.
fn resolve_style(
    kind: RunKind,
    block: BlockKind,
    fonts: &MarkdownFonts,
    colors: &MdColors,
) -> ResolvedStyle {
    // Base size from block.
    let base_size = match block {
        BlockKind::Heading(l) => heading_size(l),
        BlockKind::CodeBlock => CODE_SIZE,
        _ => BODY_SIZE,
    };

    if kind.is_marker() {
        // Markers render in the body/mono face, dim, at the block size.
        let mono = matches!(block, BlockKind::CodeBlock);
        return ResolvedStyle {
            font: (if mono { fonts.mono.clone() } else { fonts.body.clone() }).into(),
            weight: if matches!(block, BlockKind::Heading(_)) {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            size: base_size,
            color: colors.marker,
        };
    }

    let s: InlineStyle = kind.style();
    let italic = s.italic;
    let mono = s.code || matches!(block, BlockKind::CodeBlock);
    let bold = s.bold || matches!(block, BlockKind::Heading(_));

    let font = if mono {
        fonts.mono.clone()
    } else if italic {
        fonts.italic.clone()
    } else {
        fonts.body.clone()
    };
    let weight = if bold { FontWeight::BOLD } else { FontWeight::NORMAL };
    let size = if mono { CODE_SIZE.max(base_size.min(CODE_SIZE).max(CODE_SIZE)) } else { base_size };
    let size = if s.code && !matches!(block, BlockKind::CodeBlock) {
        CODE_SIZE
    } else {
        size
    };
    let color = if s.code {
        colors.code
    } else if s.link {
        colors.link
    } else if matches!(block, BlockKind::Heading(_)) {
        colors.heading
    } else if matches!(block, BlockKind::BlockQuote) {
        colors.marker
    } else {
        colors.body
    };
    ResolvedStyle { font, weight, size, color }
}

/// Dominant font size for a render line, from its block (used for row
/// height & caret height; lines are visually single-size).
fn line_size(block: BlockKind) -> f32 {
    match block {
        BlockKind::Heading(l) => heading_size(l),
        BlockKind::CodeBlock => CODE_SIZE,
        _ => BODY_SIZE,
    }
}

// ---------- Mode detection ----------

/// Attach [`MarkdownMode`] to editor panes once their file path is known.
pub fn detect_markdown_mode(
    mut commands: Commands,
    q: Query<
        (Entity, &crate::EditorFilePath, &PaneKindMarker),
        (With<PaneTag>, Without<MarkdownMode>),
    >,
) {
    for (e, path, kind) in &q {
        if kind.0 != crate::PANE_KIND {
            continue;
        }
        let is_md = path
            .0
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case("md") || x.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false);
        commands.entity(e).insert(MarkdownMode {
            enabled: is_md,
            raw: false,
        });
    }
}

/// Marker: this editor has a keepalive sprite under its content_root.
#[derive(Component)]
pub struct MdKeepalive;

/// Bevy 0.19's `update_text2d_layout` only lays out `Text2d` for cameras
/// that currently have at least one visible `Sprite` entity (it builds
/// `target_scale_factors` from `camera_query.filter(visible Sprite)`).
///
/// A WYSIWYG markdown pane is pure `Text2d` — its only sprites are the
/// caret and (optional) selection / code-block / blockquote decorations.
/// When none of those are visible (caret blinked off or off-screen, no
/// selection, a doc with no code/quote blocks), the pane camera has NO
/// visible sprite, so the text never lays out: glyphs stay empty, every
/// render line collapses to the one-row fallback height, and wrapped
/// paragraphs stack on top of the lines below them. Worse, it self-
/// sustains: a collapsed layout can push the caret off-screen, hiding the
/// one sprite that would have re-armed layout.
///
/// Park one always-visible, fully-transparent sprite under each markdown
/// pane's content_root so its camera ALWAYS qualifies and the text lays
/// out every frame.
pub fn ensure_md_keepalive(
    mut commands: Commands,
    editors: Query<
        (Entity, &PaneChrome, &PaneKindMarker, Option<&MarkdownMode>),
        Without<MdKeepalive>,
    >,
) {
    if std::env::var("MD_NO_KEEPALIVE").is_ok() {
        return;
    }
    for (entity, chrome, kind, md) in &editors {
        if kind.0 != crate::PANE_KIND || !wysiwyg_active(md) {
            continue;
        }
        commands.spawn((
            MdKeepalive,
            ChildOf(chrome.content_root),
            Sprite {
                color: Color::NONE,
                // HUGE and anchored at the content origin so it spans the
                // entire scrollable range: no matter how far the pane is
                // scrolled, this transparent sprite still intersects the
                // camera frustum, keeping the camera in `update_text2d_layout`'s
                // set of "cameras with a visible Sprite". Without that, a pure-
                // text markdown pane's text never lays out and every wrapped
                // line collapses to one row (the overlap bug).
                custom_size: Some(Vec2::new(8000.0, 200_000.0)),
                ..default()
            },
            Anchor::TOP_LEFT,
            // Behind the text; transparent regardless.
            Transform::from_xyz(0.0, 0.0, -10.0),
        ));
        commands.entity(entity).insert(MdKeepalive);
    }
}

/// Toggleable debug overlay for markdown layout. `boxes` draws one
/// translucent rectangle per layout line at the position + height the
/// MODEL claims; `labels` adds a per-line index/height tag. Lets us put
/// "what we think is there" on screen next to "what is actually rendered".
#[derive(Resource, Default)]
pub struct MdDebugViz {
    pub boxes: bool,
    pub labels: bool,
    pub hide_md_text: bool,
}

/// Marker for a debug overlay entity (box or label) so we can clear them.
#[derive(Component)]
pub struct MdDebugItem;

/// Keyboard toggles for the markdown debug overlay — all are
/// `Ctrl+Alt+<key>` so macOS / the editor's text input never swallow them:
///   Ctrl+Alt+B — toggle per-line boxes (what the MODEL thinks)
///   Ctrl+Alt+L — toggle per-line index/height labels
///   Ctrl+Alt+H — hide/show the tracked markdown render-lines
pub fn markdown_debug_toggle(keys: Res<ButtonInput<KeyCode>>, mut viz: ResMut<MdDebugViz>) {
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if !(ctrl && alt) {
        return;
    }
    if keys.just_pressed(KeyCode::KeyB) {
        viz.boxes = !viz.boxes;
        eprintln!("[md-viz] boxes = {}", viz.boxes);
    }
    if keys.just_pressed(KeyCode::KeyL) {
        viz.labels = !viz.labels;
        eprintln!("[md-viz] labels = {}", viz.labels);
    }
    if keys.just_pressed(KeyCode::KeyH) {
        viz.hide_md_text = !viz.hide_md_text;
        eprintln!("[md-viz] hide_md_text = {}", viz.hide_md_text);
    }
}

/// F11: hide/show the tracked markdown render-line entities. If text is
/// STILL visible after hiding them, that text is coming from some OTHER
/// renderer (e.g. the monospace grid editor that should have been torn
/// down) — i.e. the duplicate, overlapping copy.
pub fn markdown_debug_hide_text(
    viz: Res<MdDebugViz>,
    mut lines: Query<&mut Visibility, With<MdLineRef>>,
) {
    let want = if viz.hide_md_text {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    };
    for mut v in &mut lines {
        if *v != want {
            *v = want;
        }
    }
}

/// Draw the debug overlay: a translucent box per layout line (alternating
/// red/blue so adjacent lines and any overlap are obvious), optionally with
/// an index/height label. If the boxes line up cleanly with the rendered
/// text, the layout MODEL matches the render. If the text overlaps while
/// the boxes are clean (or vice-versa), the mismatch is right there.
pub fn markdown_debug_boxes(
    mut commands: Commands,
    viz: Res<MdDebugViz>,
    fonts: Option<Res<MarkdownFonts>>,
    editors: Query<(&PaneChrome, &PaneRect, Option<&MarkdownMode>, &MdLayout), With<PaneTag>>,
    existing: Query<Entity, With<MdDebugItem>>,
) {
    for e in &existing {
        commands.entity(e).despawn();
    }
    if !viz.boxes && !viz.labels {
        return;
    }
    for (chrome, rect, md, layout) in &editors {
        if !wysiwyg_active(md) {
            continue;
        }
        let width = content_area_size(rect).x;
        for (i, g) in layout.lines.iter().enumerate() {
            if viz.boxes {
                let color = if i % 2 == 0 {
                    Color::srgba(1.0, 0.25, 0.25, 0.25)
                } else {
                    Color::srgba(0.25, 0.6, 1.0, 0.25)
                };
                let w = (width - g.x_offset).max(4.0);
                commands.spawn((
                    MdDebugItem,
                    ChildOf(chrome.content_root),
                    Sprite {
                        color,
                        custom_size: Some(Vec2::new(w, g.height.max(1.0))),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    // Above the text so the tint is visible.
                    Transform::from_xyz(g.x_offset, -g.top, 0.6),
                ));
            }
            if viz.labels {
                if let Some(fonts) = fonts.as_ref() {
                    commands.spawn((
                        MdDebugItem,
                        ChildOf(chrome.content_root),
                        Text2d::new(format!("{i}·h{:.0}·g{}", g.height, g.glyphs.len())),
                        TextFont {
                            font: fonts.mono.clone().into(),
                            font_size: FontSize::Px(11.0),
                            ..default()
                        },
                        TextColor(Color::srgb(1.0, 1.0, 0.3)),
                        Anchor::TOP_LEFT,
                        Transform::from_xyz(width - 90.0, -g.top, 0.7),
                    ));
                }
            }
        }
    }
}

// ---------- Render (Update) ----------

/// Rebuild render-line entities when the document, active block, width,
/// or mode changes. Despawns everything when WYSIWYG is inactive.
#[allow(clippy::too_many_arguments)]
pub fn markdown_render(
    mut commands: Commands,
    theme: Option<Res<jim_style::Theme>>,
    fonts: Option<Res<MarkdownFonts>>,
    palette: Option<Res<SyntaxPalette>>,
    mut code_hl: Option<ResMut<MarkdownCodeHl>>,
    mut editors: Query<
        (
            Entity,
            Ref<EditorStateComp>,
            Ref<PaneRect>,
            &PaneChrome,
            &PaneKindMarker,
            Option<&MarkdownMode>,
            Option<&mut MdDoc>,
            Option<&mut MdState>,
            Option<&mut MdLines>,
        ),
        With<PaneTag>,
    >,
) {
    let Some(fonts) = fonts else { return };
    let colors = theme
        .as_deref()
        .map(MdColors::from_theme)
        .unwrap_or_default();

    for (entity, state, rect, chrome, kind, md, mut doc, mut mdstate, mut lines) in &mut editors {
        if kind.0 != crate::PANE_KIND {
            continue;
        }
        if !wysiwyg_active(md) {
            // Tear down any md entities so the grid path is unobstructed.
            if let Some(lines) = lines.as_mut() {
                for e in lines.0.drain(..) {
                    commands.entity(e).despawn();
                }
            }
            continue;
        }

        // Reparse the document on change.
        let doc_changed = doc.is_none() || state.is_changed();
        if doc_changed {
            let parsed = markdown_core::parse(&state.0.doc.to_string());
            match doc.as_mut() {
                Some(d) => d.0 = parsed,
                None => {
                    commands.entity(entity).insert(MdDoc(parsed));
                    commands.entity(entity).insert(MdState::default());
                    commands.entity(entity).insert(MdLines::default());
                    commands.entity(entity).insert(MdLayout::default());
                    // Re-run next frame once components exist.
                    continue;
                }
            }
        }
        let (Some(doc), Some(mdstate), Some(lines)) =
            (doc.as_mut(), mdstate.as_mut(), lines.as_mut())
        else {
            continue;
        };

        let caret = state.0.selection.primary_range().head;
        let active = doc
            .0
            .blocks
            .iter()
            .position(|b| b.active_for(caret))
            .map(|i| i as i64)
            .unwrap_or(-1);

        // Rendering no longer depends on which block holds the caret
        // (markers are always hidden), so only doc / width / first-build
        // trigger a rebuild — caret movement is cheap and flicker-free.
        let need_rebuild = doc_changed || rect.is_changed() || lines.0.is_empty();
        if !need_rebuild {
            continue;
        }
        if std::env::var("JIM_MD_DBG").is_ok() {
            eprintln!(
                "[md-rebuild {entity:?}] doc_changed={doc_changed} rect_changed={} empty={} pool_before={}",
                rect.is_changed(),
                lines.0.is_empty(),
                lines.0.len(),
            );
        }
        mdstate.active = active;

        let content = content_area_size(&rect);
        rebuild_lines(
            &mut commands,
            entity,
            chrome.content_root,
            &doc.0,
            active,
            content.x,
            &fonts,
            &colors,
            palette.as_deref(),
            code_hl.as_deref_mut().map(|c| &mut c.0),
            lines,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn rebuild_lines(
    commands: &mut Commands,
    editor: Entity,
    content_root: Entity,
    doc: &Document,
    active: i64,
    content_width: f32,
    fonts: &MarkdownFonts,
    colors: &MdColors,
    palette: Option<&SyntaxPalette>,
    mut code_hl: Option<&mut Highlighter>,
    pool: &mut MdLines,
) {
    let mut idx = 0usize;
    for (bi, block) in doc.blocks.iter().enumerate() {
        let is_active = bi as i64 == active;
        let x_offset = block_indent(block);
        let lsize = line_size(block.kind);
        let rh = row_height_for(lsize);

        // Syntax-highlight a fenced code block: highlight the whole code
        // body once, then resolve per-content-line colored chunks.
        let code_chunks = if matches!(block.kind, BlockKind::CodeBlock) {
            highlight_code_block(block, code_hl.as_deref_mut())
        } else {
            None
        };

        for (li, line) in block.lines.iter().enumerate() {
            let entity = ensure_line_entity(commands, content_root, pool, idx, editor);
            if let Some(chunks) = code_chunks.as_ref().and_then(|c| c.get(li)).and_then(|c| c.as_ref())
            {
                build_code_line(
                    commands, entity, editor, bi, line, x_offset, rh, fonts, palette, colors,
                    chunks,
                );
            } else {
                build_line(
                    commands, entity, editor, block, bi, line, is_active, content_width, x_offset,
                    rh, fonts, colors,
                );
            }
            idx += 1;
        }
    }
    // Despawn surplus pooled entities.
    for e in pool.0.drain(idx..) {
        commands.entity(e).despawn();
    }
}

/// Render text for a kept list prefix: a real bullet for unordered
/// lists (replacing the `-`/`*`/`+`, preserving char count so the
/// source mapping stays 1:1), the literal `N.` for ordered lists.
fn bullet_text(kind: BlockKind, marker: &str) -> String {
    match kind {
        BlockKind::ListItem { ordered: false } => {
            let rest: String = marker.chars().skip(1).collect();
            format!("•{rest}")
        }
        _ => marker.to_string(),
    }
}

/// Left indent for a block.
fn block_indent(block: &Block) -> f32 {
    let base = block.indent as f32 * INDENT_PX;
    match block.kind {
        BlockKind::BlockQuote | BlockKind::CodeBlock => base + BLOCK_PAD,
        BlockKind::ListItem { .. } => base,
        _ => base,
    }
}

fn ensure_line_entity(
    commands: &mut Commands,
    content_root: Entity,
    pool: &mut MdLines,
    idx: usize,
    editor: Entity,
) -> Entity {
    if let Some(&e) = pool.0.get(idx) {
        return e;
    }
    let e = commands
        .spawn((
            ChildOf(content_root),
            Text2d::new(String::new()),
            TextLayout {
                justify: Justify::Left,
                linebreak: LineBreak::WordOrCharacter,
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(0.0, 0.0, 0.0),
            MdLineRef {
                editor,
                spans: Vec::new(),
                row_height: row_height_for(BODY_SIZE),
                x_offset: 0.0,
                src: 0..0,
                kind: BlockKind::Paragraph,
                block: 0,
                active: false,
                collapse: false,
            },
        ))
        .id();
    pool.0.push(e);
    e
}

#[allow(clippy::too_many_arguments)]
fn build_line(
    commands: &mut Commands,
    entity: Entity,
    editor: Entity,
    block: &Block,
    block_index: usize,
    line: &RenderLine,
    is_active: bool,
    content_width: f32,
    x_offset: f32,
    row_height: f32,
    fonts: &MarkdownFonts,
    colors: &MdColors,
) {
    // Clear existing span children.
    commands.entity(entity).despawn_related::<Children>();

    let wrap_w = (content_width - x_offset).max(40.0);
    commands
        .entity(entity)
        .insert(TextBounds::new_horizontal(wrap_w));

    // Only list items keep their leading marker — rendered as a real
    // bullet / number. Blockquotes hide the `>` (the bar decoration
    // stands in for it); every other marker is hidden Typora-style.
    let keep_prefix = matches!(block.kind, BlockKind::ListItem { .. });
    let mut spans: Vec<(usize, String)> = Vec::new();
    for (ri, run) in line.runs.iter().enumerate() {
        let is_prefix = ri == 0 && keep_prefix;
        if run.kind.is_marker() && !is_prefix {
            continue;
        }
        if run.text.is_empty() {
            continue;
        }
        let mut rs = resolve_style(run.kind, block.kind, fonts, colors);
        // Substitute a real bullet for an unordered list dash/star/plus
        // (same char count → 1:1 source mapping preserved), and give the
        // marker a readable color.
        let rendered = if is_prefix {
            rs.color = colors.body;
            bullet_text(block.kind, &run.text)
        } else {
            run.text.clone()
        };
        commands.entity(entity).with_child((
            TextSpan::new(rendered.clone()),
            TextFont {
                font: (rs.font).into(),
                font_size: FontSize::Px(rs.size),
                weight: rs.weight,
                ..default()
            },
            LineHeight::Px(row_height),
            TextColor(rs.color),
        ));
        spans.push((run.src.start, rendered));
    }

    // Blank line: no spans. The entity may have been REUSED from a line that
    // previously held text — and Bevy's `detect_text_needs_rerender` only
    // re-lays-out a text block on `Changed<TextSpan>`/`Changed<Children>`.
    // Removing all children doesn't fire that, so the pipeline keeps the old
    // glyphs and the entity renders stale text at its new (shifted) position
    // — the "old line sticks around" duplication when a newline is inserted.
    // Explicitly clear the layout so a blank line renders nothing.
    if spans.is_empty() {
        commands.entity(entity).insert(TextLayoutInfo::default());
    }

    commands.entity(entity).insert(MdLineRef {
        editor,
        spans,
        row_height,
        x_offset,
        src: line.src.clone(),
        kind: block.kind,
        block: block_index,
        active: is_active,
        // A hidden code fence line (``` ): collapse so it leaves no gap.
        collapse: matches!(block.kind, BlockKind::CodeBlock)
            && line
                .runs
                .first()
                .map(|r| r.kind.is_marker())
                .unwrap_or(false),
    });
}

/// Map a fence language string to a highlighter language, or None if we
/// don't have a parser for it (falls back to plain mono code).
fn highlight_lang_supported(lang: Option<&str>) -> bool {
    matches!(
        lang.map(|l| l.trim().to_ascii_lowercase()).as_deref(),
        Some("rust") | Some("rs")
    )
}

/// Syntax-highlight a fenced code block. Returns a vec aligned to
/// `block.lines`: `Some(chunks)` for code content lines, `None` for
/// fence lines (or `None` overall when the language is unsupported or no
/// highlighter is available). Chunks are `(text, kind)`; text maps 1:1
/// by char to source chars, so the caret stays exact.
fn highlight_code_block(
    block: &Block,
    code_hl: Option<&mut Highlighter>,
) -> Option<Vec<Option<Vec<(String, HighlightKind)>>>> {
    let hl = code_hl?;
    if !highlight_lang_supported(block.lang.as_deref()) {
        return None;
    }
    // Content lines = those that aren't a fence marker line.
    let is_fence = |l: &RenderLine| {
        l.runs
            .first()
            .map(|r| r.kind.is_marker())
            .unwrap_or(false)
    };
    let content_text = |l: &RenderLine| -> String {
        l.runs.first().map(|r| r.text.clone()).unwrap_or_default()
    };

    // Join content lines into a single code body to parse.
    let content_lines: Vec<&RenderLine> = block.lines.iter().filter(|l| !is_fence(l)).collect();
    let code = content_lines
        .iter()
        .map(|l| content_text(l))
        .collect::<Vec<_>>()
        .join("\n");
    let rope = Rope::from_str(&code);
    hl.maybe_reparse(&rope);

    // Per content line (in rope order), resolve chunks.
    let mut content_chunks: Vec<Vec<(String, HighlightKind)>> = Vec::new();
    for (i, l) in content_lines.iter().enumerate() {
        let text = content_text(l);
        content_chunks.push(hl.line_chunks(&rope, i, 0, &text));
    }

    // Re-align to block.lines order (None for fence lines).
    let mut out = Vec::with_capacity(block.lines.len());
    let mut ci = 0;
    for l in &block.lines {
        if is_fence(l) {
            out.push(None);
        } else {
            out.push(Some(std::mem::take(&mut content_chunks[ci])));
            ci += 1;
        }
    }
    Some(out)
}

/// Build a code-content render line from highlighted `(text, kind)`
/// chunks. Chunks render in the mono font at the code size, colored by
/// the syntax palette; each maps to consecutive source chars from the
/// line start.
#[allow(clippy::too_many_arguments)]
fn build_code_line(
    commands: &mut Commands,
    entity: Entity,
    editor: Entity,
    block_index: usize,
    line: &RenderLine,
    x_offset: f32,
    row_height: f32,
    fonts: &MarkdownFonts,
    palette: Option<&SyntaxPalette>,
    colors: &MdColors,
    chunks: &[(String, HighlightKind)],
) {
    commands.entity(entity).despawn_related::<Children>();
    commands
        .entity(entity)
        .insert(TextBounds::UNBOUNDED); // code doesn't soft-wrap

    let mut spans: Vec<(usize, String)> = Vec::new();
    let mut src = line.src.start;
    for (text, kind) in chunks {
        if text.is_empty() {
            continue;
        }
        let color = palette
            .map(|p| p.color_for(*kind))
            .unwrap_or(colors.code);
        commands.entity(entity).with_child((
            TextSpan::new(text.clone()),
            TextFont {
                font: (fonts.mono.clone()).into(),
                font_size: FontSize::Px(CODE_SIZE),
                ..default()
            },
            LineHeight::Px(row_height),
            TextColor(color),
        ));
        spans.push((src, text.clone()));
        src += text.chars().count();
    }

    // See build_line: clear stale glyphs when a reused entity has no spans.
    if spans.is_empty() {
        commands.entity(entity).insert(TextLayoutInfo::default());
    }

    commands.entity(entity).insert(MdLineRef {
        editor,
        spans,
        row_height,
        x_offset,
        src: line.src.clone(),
        kind: BlockKind::CodeBlock,
        block: block_index,
        active: false,
        collapse: false,
    });
}

// ---------- Readback (PostUpdate) ----------

/// Read Bevy's computed glyph layout back into [`MdLayout`] for each
/// markdown editor. Runs after `Text2dUpdateSystems`.
pub fn markdown_readback(
    mut editors: Query<(Entity, Option<&MarkdownMode>, &MdLines, &mut MdLayout), With<PaneTag>>,
    lines_q: Query<(&MdLineRef, &TextLayoutInfo)>,
) {
    for (_editor, md, pool, mut layout) in &mut editors {
        if !wysiwyg_active(md) {
            layout.lines.clear();
            layout.total_height = 0.0;
            continue;
        }
        let mut geoms: Vec<MdLineGeom> = Vec::with_capacity(pool.0.len());
        let mut top = 0.0f32;
        // True if a line that HAS text (non-empty spans) produced no glyphs
        // this frame — i.e. Bevy didn't lay the text out (the pane camera had
        // no qualifying sprite, so `update_text2d_layout` skipped it). On such
        // a frame every wrapped paragraph would collapse to the one-row
        // fallback height, so we must NOT commit this layout.
        let mut incomplete = false;
        for &line_e in &pool.0 {
            let Ok((lref, info)) = lines_q.get(line_e) else {
                continue;
            };
            let rh = lref.row_height;
            // Glyph positions are in PHYSICAL pixels (Bevy scales them by the
            // window scale factor; only `info.size` is divided back to
            // logical). Our transforms are logical, so divide here.
            let sf = if info.scale_factor > 0.0 {
                info.scale_factor
            } else {
                1.0
            };
            // Bevy 0.19 / parley: `PositionedGlyph` no longer carries per-glyph
            // byte offsets or size — only a `section_index`, `line_index`, a
            // center `position`, and the atlas `rect`. We rebuild source char
            // ranges by counting glyphs in logical order within each rendered
            // span (section), and derive horizontal extents from neighbouring
            // glyph centers on the same row. This assumes one glyph per source
            // char, which holds for the editor's LTR mono/Inter text; ligature
            // clusters can drift the caret by a fraction of a cell.
            struct G {
                row: usize,
                x: f32,
                half: f32,
                section: usize,
            }
            if std::env::var("JIM_MD_RAW").is_ok() && lref.src.end - lref.src.start > 50 {
                eprintln!(
                    "[md-raw] src={:?} raw_glyphs={} size=({:.0},{:.0}) sf={:.2} spans={} bounds_w?",
                    (lref.src.start, lref.src.end),
                    info.glyphs.len(),
                    info.size.x,
                    info.size.y,
                    info.scale_factor,
                    lref.spans.len(),
                );
            }
            let mut gs: Vec<G> = info
                .glyphs
                .iter()
                // section 0 is the (empty) root section; spans are 1..
                .filter(|g| g.section_index != 0)
                .map(|g| G {
                    row: g.line_index,
                    x: g.position.x / sf,
                    half: (g.atlas_info.rect.width() * 0.5 / sf).max(0.5),
                    section: g.section_index,
                })
                .collect();
            // Sort by row then x: logical order for LTR text.
            gs.sort_by(|a, b| a.row.cmp(&b.row).then(a.x.total_cmp(&b.x)));

            // Per-span running char counter -> source char offsets.
            let mut span_char: Vec<usize> = vec![0; lref.spans.len()];
            let mut glyphs: Vec<MdGlyph> = Vec::with_capacity(gs.len());
            let n = gs.len();
            for i in 0..n {
                let g = &gs[i];
                let prev_same = i > 0 && gs[i - 1].row == g.row;
                let next_same = i + 1 < n && gs[i + 1].row == g.row;
                // Cell boundaries = midpoints between glyph centers; fall back
                // to the glyph's own half-width at the row ends.
                let left = if prev_same {
                    (gs[i - 1].x + g.x) * 0.5
                } else {
                    g.x - g.half
                };
                let right = if next_same {
                    (g.x + gs[i + 1].x) * 0.5
                } else {
                    g.x + g.half
                };
                let k = g.section - 1;
                let (Some((src_start, _text)), Some(cnt)) =
                    (lref.spans.get(k), span_char.get_mut(k))
                else {
                    continue;
                };
                let char_index = *cnt;
                *cnt += 1;
                glyphs.push(MdGlyph {
                    src_char: src_start + char_index,
                    src_len: 1,
                    left: lref.x_offset + left,
                    right: lref.x_offset + right,
                    row: g.row,
                });
            }
            glyphs.sort_by(|a, b| a.row.cmp(&b.row).then(a.left.total_cmp(&b.left)));
            // Height: prefer Bevy's measured size; fall back to one row.
            // Code-fence lines render to nothing (markers hidden) — collapse
            // them to zero so a code block has no blank gap above/below its
            // content. Blank lines and rules keep a row of height.
            if !lref.spans.is_empty() && glyphs.is_empty() {
                incomplete = true;
            }
            let height = if !glyphs.is_empty() {
                if info.size.y > 0.5 { info.size.y } else { rh }
            } else if lref.collapse {
                0.0
            } else {
                rh
            };
            geoms.push(MdLineGeom {
                src: lref.src.clone(),
                top,
                height,
                row_height: rh,
                x_offset: lref.x_offset,
                glyphs,
                kind: lref.kind,
                block: lref.block,
                active: lref.active,
            });
            top += height;
        }
        // Text didn't lay out this frame (e.g. the pane just became visible
        // and the camera hasn't acquired a qualifying sprite yet). Committing
        // now would collapse every wrapped line to one row and overlap the
        // lines below. Keep the last good layout; a subsequent frame that
        // lays out fully will replace it. Only fall through on first build
        // (no prior layout to preserve).
        // Only safe to keep the old layout if it still lines up 1:1 with the
        // current pool — otherwise `markdown_position` would map entities onto
        // stale slots (a paragraph drawn in a blank line's position). If the
        // pool changed (doc edited), commit the new geometry even if a frame's
        // layout is briefly incomplete; it self-corrects next frame.
        if incomplete && layout.lines.len() == pool.0.len() {
            continue;
        }
        layout.total_height = top;
        layout.lines = geoms;
    }
}

// ---------- Position (Update) ----------

/// Position render-line transforms and the caret from the *fresh*
/// [`MdLayout`]. Runs in `PostUpdate` (after text layout, before
/// transform propagation) — it only MUTATES existing entities, never
/// spawns, so it's safe to run after the per-pane `RenderLayers`
/// propagation.
pub fn markdown_position(
    editors: Query<
        (
            Entity,
            &EditorStateComp,
            &PaneRect,
            &EditorScroll,
            Option<&MarkdownMode>,
            &MdLines,
            &MdLayout,
        ),
        With<PaneTag>,
    >,
    mut tf_q: Query<&mut Transform>,
    mut caret_q: Query<(Entity, &EditorCaret, &mut Sprite, &mut Visibility)>,
    all_refs: Query<(Entity, &MdLineRef)>,
    grid_lines: Query<(), With<crate::LineRender>>,
    mut dbg_last: Local<std::collections::HashMap<Entity, String>>,
    mut dbg_frame: Local<u32>,
) {
    *dbg_frame += 1;
    // Force a reprint of the steady state every ~1.5s even when nothing
    // changed, so we can read the picture the user is actually staring at.
    if *dbg_frame % 90 == 0 {
        dbg_last.clear();
    }
    if std::env::var("JIM_MD_DBG").is_ok() && *dbg_frame % 90 == 0 {
        // GLOBAL orphan sweep: MdLineRef entities whose editor is no longer a
        // live wysiwyg editor in this query. Such entities keep rendering at
        // their last positions — a second, untracked copy of the doc.
        let live_editors: std::collections::HashSet<Entity> =
            editors.iter().map(|t| t.0).collect();
        let mut orphan_editors: std::collections::HashMap<Entity, usize> =
            std::collections::HashMap::new();
        for (_e, r) in &all_refs {
            if !live_editors.contains(&r.editor) {
                *orphan_editors.entry(r.editor).or_default() += 1;
            }
        }
        let total_refs = all_refs.iter().count();
        let grid_count = grid_lines.iter().count();
        eprintln!(
            "[md-global] total_MdLineRef_entities={total_refs} live_editors={} GHOST_editors={:?} GRID_LineRender_entities={grid_count}",
            live_editors.len(),
            orphan_editors,
        );
    }
    for (editor, state, rect, scroll, md, pool, layout) in &editors {
        if !wysiwyg_active(md) {
            continue;
        }
        // Position each render line at its top.
        for (i, &line_e) in pool.0.iter().enumerate() {
            let Some(g) = layout.lines.get(i) else { continue };
            if let Ok(mut t) = tf_q.get_mut(line_e) {
                t.translation.x = g.x_offset;
                t.translation.y = -g.top;
                t.translation.z = 0.0;
            }
        }
        if std::env::var("JIM_MD_DBG").is_ok() {
            // How many entities carry an MdLineRef pointing at THIS editor?
            // (rendered lines, whether or not they're in the live pool.)
            let live: std::collections::HashSet<Entity> = pool.0.iter().copied().collect();
            let mut total_refs = 0usize;
            let mut orphans = 0usize;
            for (e, r) in &all_refs {
                if r.editor != editor {
                    continue;
                }
                total_refs += 1;
                if !live.contains(&e) {
                    orphans += 1;
                }
            }
            // Detect duplicate source ranges inside the live layout (the same
            // doc line rendered by more than one pool entity).
            let mut seen: std::collections::HashMap<(usize, usize), usize> =
                std::collections::HashMap::new();
            for g in &layout.lines {
                *seen.entry((g.src.start, g.src.end)).or_default() += 1;
            }
            let dup_src = seen.values().filter(|&&c| c > 1).count();
            let report = format!(
                "pool={} layout={} refs={} ORPHANS={} dup_src_ranges={} total_h={:.0}",
                pool.0.len(),
                layout.lines.len(),
                total_refs,
                orphans,
                dup_src,
                layout.total_height,
            );
            // Map each pool ENTITY to the src range its MdLineRef currently
            // holds (what it will actually render), to compare against the
            // layout slot it's being positioned into.
            let ent_src: std::collections::HashMap<Entity, (usize, usize)> = all_refs
                .iter()
                .map(|(e, r)| (e, (r.src.start, r.src.end)))
                .collect();
            let mut mismatches = 0usize;
            for (i, &line_e) in pool.0.iter().enumerate() {
                if let (Some(es), Some(g)) = (ent_src.get(&line_e), layout.lines.get(i)) {
                    if *es != (g.src.start, g.src.end) {
                        mismatches += 1;
                    }
                }
            }
            let report = format!("{report} POOL_LAYOUT_MISMATCH={mismatches}");
            if dbg_last.get(&editor) != Some(&report) {
                eprintln!("[md-pos {editor:?}] {report}");
                let doc_str = state.0.doc.to_string();
                for (i, &line_e) in pool.0.iter().enumerate().take(24) {
                    let Some(g) = layout.lines.get(i) else { continue };
                    let max_row = g.glyphs.iter().map(|gl| gl.row + 1).max().unwrap_or(0);
                    let ent = ent_src.get(&line_e).copied().unwrap_or((0, 0));
                    let flag = if ent != (g.src.start, g.src.end) { " <<MISMATCH" } else { "" };
                    let txt: String = doc_str
                        .get(g.src.start..g.src.end)
                        .unwrap_or("<oob>")
                        .chars()
                        .take(24)
                        .collect();
                    eprintln!(
                        "    [{i:>3}] top={:>6.1} h={:>5.1} rows={max_row} g={:>3} layout_src={:?} entity_src={:?} {:?}{flag}",
                        g.top,
                        g.height,
                        g.glyphs.len(),
                        (g.src.start, g.src.end),
                        ent,
                        txt,
                    );
                }
                dbg_last.insert(editor, report);
            }
        }

        let content = content_area_size(rect);
        let head = state.0.selection.primary_range().head;

        if let Some((cx, cy, ch)) = caret_pos(layout, head) {
            for (caret_e, parent, mut sprite, mut vis) in &mut caret_q {
                if parent.0 != editor {
                    continue;
                }
                let visible = cy >= scroll.y - ch && cy <= scroll.y + content.y;
                if let Ok(mut t) = tf_q.get_mut(caret_e) {
                    t.translation.x = cx;
                    t.translation.y = -cy;
                    t.translation.z = 1.0;
                }
                if let Some(size) = sprite.custom_size.as_mut() {
                    size.y = ch;
                } else {
                    sprite.custom_size = Some(Vec2::new(2.0, ch));
                }
                *vis = if visible {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                };
            }
        }
    }
}

/// Spawn selection-highlight and block-decoration sprites. Runs in
/// `Update` (NOT PostUpdate): these are freshly spawned every frame, and
/// per-pane `RenderLayers` are propagated in PostUpdate — sprites spawned
/// after that propagation would render on the wrong layer (i.e. not show
/// on the pane at all). Reads the prior frame's [`MdLayout`], which
/// matches what's currently on screen (same as the click path).
pub fn markdown_decorations(
    mut commands: Commands,
    theme: Option<Res<jim_style::Theme>>,
    editors: Query<
        (
            Entity,
            &EditorStateComp,
            &PaneChrome,
            &PaneRect,
            Option<&MarkdownMode>,
            &MdLayout,
        ),
        With<PaneTag>,
    >,
    existing_sel: Query<(Entity, &MdSel)>,
    existing_decor: Query<(Entity, &MdDecor)>,
) {
    for (e, _) in &existing_sel {
        commands.entity(e).despawn();
    }
    for (e, _) in &existing_decor {
        commands.entity(e).despawn();
    }
    let deco = DecorColors::from_theme(theme.as_deref());

    for (editor, state, chrome, rect, md, layout) in &editors {
        if !wysiwyg_active(md) {
            continue;
        }
        let content = content_area_size(rect);
        let range = state.0.selection.primary_range();

        if range.from() != range.to() {
            let sel_color = theme
                .as_deref()
                .map(|t| Color::LinearRgba(t.color(jim_style::tokens::SELECTION)))
                .unwrap_or(Color::srgba(0.30, 0.45, 0.70, 0.45));
            let rects = selection_rects(layout, range.from(), range.to());
            if std::env::var("JIM_MD_SEL_DEBUG").is_ok() {
                eprintln!(
                    "[md-sel] range={}..{} lines={} rects={}",
                    range.from(),
                    range.to(),
                    layout.lines.len(),
                    rects.len()
                );
                for (i, l) in layout.lines.iter().enumerate() {
                    eprintln!(
                        "  line{i} src={:?} top={:.1} h={:.1} glyphs={}",
                        l.src,
                        l.top,
                        l.height,
                        l.glyphs.len()
                    );
                }
                for r in &rects {
                    eprintln!("  rect x={:.1} y={:.1} w={:.1} h={:.1}", r.x, r.y, r.w, r.h);
                }
            }
            for rect_geom in rects {
                commands.spawn((
                    MdSel(editor),
                    ChildOf(chrome.content_root),
                    Sprite {
                        color: sel_color,
                        custom_size: Some(Vec2::new(rect_geom.w, rect_geom.h)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(rect_geom.x, -rect_geom.y, 0.5),
                ));
            }
        }

        draw_decorations(&mut commands, editor, chrome.content_root, layout, content.x, &deco);
    }
}

/// Colors for block decorations.
struct DecorColors {
    code_bg: Color,
    quote_bar: Color,
    rule: Color,
}

impl DecorColors {
    fn from_theme(theme: Option<&jim_style::Theme>) -> Self {
        let muted = theme
            .map(|t| Color::LinearRgba(t.color(jim_style::tokens::FG_MUTED)))
            .unwrap_or(Color::srgb(0.48, 0.52, 0.58));
        Self {
            // A distinct near-opaque panel so a code block reads as one.
            code_bg: Color::srgba(0.13, 0.15, 0.19, 0.92),
            quote_bar: muted,
            rule: muted,
        }
    }
}

/// Draw code-block backgrounds, blockquote bars, and thematic-break
/// rules by grouping the layout's render lines per source block.
fn draw_decorations(
    commands: &mut Commands,
    editor: Entity,
    content_root: Entity,
    layout: &MdLayout,
    content_width: f32,
    deco: &DecorColors,
) {
    let mut i = 0;
    while i < layout.lines.len() {
        let block = layout.lines[i].block;
        let kind = layout.lines[i].kind;
        let mut j = i;
        while j < layout.lines.len() && layout.lines[j].block == block {
            j += 1;
        }
        let first = &layout.lines[i];
        let last = &layout.lines[j - 1];
        let top = first.top;
        let bottom = last.top + last.height;
        let x_off = first.x_offset;

        match kind {
            BlockKind::CodeBlock => {
                let x = (x_off - BLOCK_PAD).max(0.0);
                let w = (content_width - x).max(8.0);
                let h = (bottom - top) + 6.0;
                commands.spawn((
                    MdDecor(editor),
                    ChildOf(content_root),
                    Sprite {
                        color: deco.code_bg,
                        custom_size: Some(Vec2::new(w, h)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(x, -(top - 3.0), -0.4),
                ));
            }
            BlockKind::BlockQuote => {
                let x = (x_off - BLOCK_PAD).max(0.0);
                commands.spawn((
                    MdDecor(editor),
                    ChildOf(content_root),
                    Sprite {
                        color: deco.quote_bar,
                        custom_size: Some(Vec2::new(4.0, bottom - top)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(x, -top, -0.2),
                ));
            }
            BlockKind::ThematicBreak => {
                let y = top + (bottom - top) * 0.5;
                let w = (content_width - x_off - 8.0).max(8.0);
                commands.spawn((
                    MdDecor(editor),
                    ChildOf(content_root),
                    Sprite {
                        color: deco.rule,
                        custom_size: Some(Vec2::new(w, 2.0)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(x_off, -y, 0.0),
                ));
            }
            _ => {}
        }
        i = j;
    }
}

struct SelGeom {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn selection_rects(layout: &MdLayout, lo: usize, hi: usize) -> Vec<SelGeom> {
    let mut out = Vec::new();
    for line in &layout.lines {
        if line.src.end < lo || line.src.start > hi {
            continue;
        }
        // Group selected glyphs per visual row into one rect.
        let mut row_extents: std::collections::BTreeMap<usize, (f32, f32)> =
            std::collections::BTreeMap::new();
        for g in &line.glyphs {
            if g.src_char + g.src_len <= lo || g.src_char >= hi {
                continue;
            }
            let e = row_extents.entry(g.row).or_insert((g.left, g.right));
            e.0 = e.0.min(g.left);
            e.1 = e.1.max(g.right);
        }
        for (row, (l, r)) in row_extents {
            out.push(SelGeom {
                x: l,
                y: line.top + row as f32 * line.row_height,
                w: (r - l).max(2.0),
                h: line.row_height,
            });
        }
    }
    out
}

// ---------- Geometry helpers (caret / click / vertical) ----------

/// Content-local `(x, top_down_y, height)` for the caret at char offset
/// `off`, or None if it can't be placed.
pub fn caret_pos(layout: &MdLayout, off: usize) -> Option<(f32, f32, f32)> {
    let line = line_for_offset(layout, off)?;
    let rh = line.row_height;
    if line.glyphs.is_empty() {
        return Some((line.x_offset, line.top, rh));
    }
    // Glyph containing the offset.
    for g in &line.glyphs {
        if off >= g.src_char && off < g.src_char + g.src_len {
            let frac = (off - g.src_char) as f32 / g.src_len.max(1) as f32;
            let x = g.left + (g.right - g.left) * frac;
            return Some((x, line.top + g.row as f32 * rh, rh));
        }
    }
    // Offset at a glyph start (boundary) — first glyph with src_char >= off.
    if let Some(g) = line.glyphs.iter().find(|g| g.src_char >= off) {
        return Some((g.left, line.top + g.row as f32 * rh, rh));
    }
    // Past the end — right edge of the last glyph.
    let g = line.glyphs.last().unwrap();
    Some((g.right, line.top + g.row as f32 * rh, rh))
}

fn line_for_offset(layout: &MdLayout, off: usize) -> Option<&MdLineGeom> {
    layout
        .lines
        .iter()
        .find(|l| off >= l.src.start && off <= l.src.end)
        .or_else(|| layout.lines.last())
}

/// Char offset nearest a content-local point `(px, top_down_y)`.
pub fn offset_at_point(layout: &MdLayout, px: f32, dy: f32) -> Option<usize> {
    if layout.lines.is_empty() {
        return None;
    }
    // Find the line whose vertical band contains dy (clamp to ends).
    let mut chosen = 0usize;
    for (i, l) in layout.lines.iter().enumerate() {
        if dy >= l.top {
            chosen = i;
        }
    }
    let line = &layout.lines[chosen];
    let row = (((dy - line.top) / line.row_height).floor().max(0.0)) as usize;
    Some(offset_in_row(line, row, px))
}

/// Char offset on a given visual row of a line, nearest x `px`.
fn offset_in_row(line: &MdLineGeom, row: usize, px: f32) -> usize {
    let row_glyphs: Vec<&MdGlyph> = line.glyphs.iter().filter(|g| g.row == row).collect();
    if row_glyphs.is_empty() {
        // Empty row — clamp to line bounds.
        return line.src.start;
    }
    for g in &row_glyphs {
        let mid = (g.left + g.right) * 0.5;
        if px < mid {
            return g.src_char;
        }
    }
    let last = row_glyphs.last().unwrap();
    (last.src_char + last.src_len).min(line.src.end)
}

/// Vertical caret motion by `delta` visual rows, preserving column x.
/// Returns the new char offset, or None if already at the doc edge.
pub fn vertical_move(layout: &MdLayout, off: usize, delta: i32) -> Option<usize> {
    // Build a flat list of (line_index, row) and find the caret's.
    let (cx, _cy, _ch) = caret_pos(layout, off)?;
    let line_idx = layout
        .lines
        .iter()
        .position(|l| off >= l.src.start && off <= l.src.end)?;
    let cur_row = {
        let line = &layout.lines[line_idx];
        line.glyphs
            .iter()
            .find(|g| off >= g.src_char && off < g.src_char + g.src_len)
            .map(|g| g.row)
            .unwrap_or(0)
    };

    // Walk to the target visual row across line boundaries.
    let mut li = line_idx as i64;
    let mut row = cur_row as i64 + delta as i64;
    loop {
        if li < 0 || li as usize >= layout.lines.len() {
            return None;
        }
        let rows = visual_rows(&layout.lines[li as usize]);
        if row < 0 {
            li -= 1;
            if li < 0 {
                return None;
            }
            row = visual_rows(&layout.lines[li as usize]) as i64 - 1;
            continue;
        }
        if row as usize >= rows {
            li += 1;
            if li as usize >= layout.lines.len() {
                return None;
            }
            row = 0;
            continue;
        }
        break;
    }
    let line = &layout.lines[li as usize];
    Some(offset_in_row(line, row as usize, cx))
}

fn visual_rows(line: &MdLineGeom) -> usize {
    line.glyphs.iter().map(|g| g.row + 1).max().unwrap_or(1)
}

/// Max vertical scroll for a markdown editor.
pub fn max_scroll(layout: &MdLayout, content_height: f32) -> f32 {
    (layout.total_height - content_height).max(0.0)
}

/// Convert a pane-local press point to a content-top-down `(px, dy)`
/// pair, accounting for scroll. Mirrors the grid path's convention.
pub fn local_to_content(local: Vec2, scroll: &EditorScroll) -> (f32, f32) {
    (local.x, local.y + scroll.y)
}

// Re-export for lib.rs convenience.
pub const TITLE_H_EXPORT: f32 = TITLE_H;
pub const MARGIN_EXPORT: f32 = MARGIN;

#[cfg(test)]
mod tests {
    use super::*;

    fn line(src: std::ops::Range<usize>, top: f32, chars: usize) -> MdLineGeom {
        // One glyph per source char, 10px wide, single row.
        let glyphs = (0..chars)
            .map(|i| MdGlyph {
                src_char: src.start + i,
                src_len: 1,
                left: i as f32 * 10.0,
                right: i as f32 * 10.0 + 10.0,
                row: 0,
            })
            .collect();
        MdLineGeom {
            src,
            top,
            height: 20.0,
            row_height: 20.0,
            x_offset: 0.0,
            glyphs,
            kind: BlockKind::Paragraph,
            block: 0,
            active: false,
        }
    }

    #[test]
    fn selection_spans_all_lines() {
        // Three lines "abc" at offsets 0..3, 4..7, 8..11 (newline between).
        let layout = MdLayout {
            lines: vec![
                line(0..3, 0.0, 3),
                line(4..7, 20.0, 3),
                line(8..11, 40.0, 3),
            ],
            total_height: 60.0,
        };
        // Select from middle of line 0 to middle of line 2.
        let rects = selection_rects(&layout, 1, 9);
        // Expect one rect per line (3).
        assert_eq!(rects.len(), 3, "rects: {:?}", rects.iter().map(|r| (r.x, r.y, r.w)).collect::<Vec<_>>());
        // Each rect should sit at its line's top.
        let ys: Vec<f32> = rects.iter().map(|r| r.y).collect();
        assert!(ys.contains(&0.0) && ys.contains(&20.0) && ys.contains(&40.0), "ys={ys:?}");
    }

    #[test]
    fn selection_spans_wrapped_rows() {
        // One render line "abcdef" (src 0..6) wrapped into two visual rows
        // of three glyphs each.
        let mut glyphs = Vec::new();
        for i in 0..6 {
            let row = i / 3;
            let col = i % 3;
            glyphs.push(MdGlyph {
                src_char: i,
                src_len: 1,
                left: col as f32 * 10.0,
                right: col as f32 * 10.0 + 10.0,
                row,
            });
        }
        let layout = MdLayout {
            lines: vec![MdLineGeom {
                src: 0..6,
                top: 0.0,
                height: 40.0,
                row_height: 20.0,
                x_offset: 0.0,
                glyphs,
                kind: BlockKind::Paragraph,
                block: 0,
                active: false,
            }],
            total_height: 40.0,
        };
        // Select the whole line — expect a rect per visual row (2).
        let rects = selection_rects(&layout, 0, 6);
        assert_eq!(rects.len(), 2, "rects={rects:?}");
    }
}

impl std::fmt::Debug for SelGeom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SelGeom{{x:{},y:{},w:{},h:{}}}", self.x, self.y, self.w, self.h)
    }
}
