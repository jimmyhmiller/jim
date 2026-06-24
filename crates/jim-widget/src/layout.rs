//! Layout pass — turns an Element tree into computed (x, y, width,
//! height) per node via the [taffy](https://docs.rs/taffy) flex/grid
//! engine.
//!
//! Pipeline:
//!   1. [`build_tree`] walks the Element tree and constructs a parallel
//!      Taffy tree, mirroring the parent/child relationship and
//!      translating each Element's style into a Taffy [`taffy::Style`].
//!      Text leaves carry a [`MeasureCtx`] so the measure callback can
//!      size them later.
//!   2. [`compute`] calls Taffy's layout solver, providing a measure
//!      function that handles text wrapping for leaves.
//!   3. The renderer walks the Element tree alongside the Taffy tree
//!      and reads each node's computed `Layout` to spawn entities.
//!
//! ## Why Taffy
//!
//! Previously, layout was hand-rolled in `render.rs::measure` +
//! `render_stack`. That worked for trivial trees but couldn't:
//!   - distribute extra width among siblings (`flex-grow`)
//!   - equalize sibling heights (`align-items: stretch`)
//!   - constrain with min/max widths
//!   - wrap text at the available width
//!
//! Taffy gives us all of those declaratively. It's the same engine
//! `bevy_ui` uses, so the dependency is already in the workspace.

use bevy::math::Vec2;
use jim_pane::PaneFontMetrics;
use taffy::prelude::*;
// taffy 0.9.2's prelude no longer re-exports `TaffyTree` (it lives at the
// crate root), so import it explicitly.
use taffy::TaffyTree;

use crate::protocol::{Align, Border, ButtonKind, Edges, Element, Shadow, Style as PStyle, Weight};

/// Per-text-leaf context passed to Taffy's measure callback.
#[derive(Clone, Debug)]
pub struct MeasureCtx {
    pub value: String,
    pub font_size: f32,
    /// When false, measure as a single line (no word-wrap) — see
    /// `Element::Text.wrap`. Other leaves (button/badge/input) always
    /// pass true; only `Text` plumbs this through.
    pub wrap: bool,
}

/// Built layout: the Taffy tree + the root node + a parallel vector of
/// (Element-tree-position → NodeId) entries.
///
/// `nodes` is in pre-order so a renderer that walks the Element tree
/// in the same pre-order can pair each Element with its layout by
/// incrementing a counter.
pub struct LaidOut {
    pub taffy: TaffyTree<MeasureCtx>,
    pub root: NodeId,
}

impl LaidOut {
    pub fn layout(&self, id: NodeId) -> Layout {
        *self.taffy.layout(id).expect("missing layout for node")
    }
}

/// Build the Taffy tree mirroring `el`. Returns the root NodeId.
/// `metrics` is used to pre-size leaves whose height depends on wrapped
/// text (currently `TextArea`).
pub fn build_tree(el: &Element, metrics: &PaneFontMetrics) -> LaidOut {
    let mut taffy = TaffyTree::new();
    let root = build_node(&mut taffy, el, metrics);
    LaidOut { taffy, root }
}

fn build_node(
    taffy: &mut TaffyTree<MeasureCtx>,
    el: &Element,
    metrics: &PaneFontMetrics,
) -> NodeId {
    match el {
        Element::Vstack {
            gap,
            pad,
            children,
            style,
        } => {
            let st = stack_style(*gap, *pad, style.as_ref(), FlexDirection::Column);
            let kids: Vec<NodeId> = children
                .iter()
                .map(|c| build_node(taffy, c, metrics))
                .collect();
            taffy.new_with_children(st, &kids).unwrap()
        }
        Element::Hstack {
            gap,
            pad,
            align,
            children,
            style,
        } => {
            let mut st = stack_style(*gap, *pad, style.as_ref(), FlexDirection::Row);
            st.align_items = Some(align_to_taffy(*align));
            let kids: Vec<NodeId> = children
                .iter()
                .map(|c| build_node(taffy, c, metrics))
                .collect();
            taffy.new_with_children(st, &kids).unwrap()
        }
        Element::Frame {
            gap,
            pad,
            children,
            style,
        } => {
            let st = stack_style(*gap, *pad, style.as_ref(), FlexDirection::Column);
            let kids: Vec<NodeId> = children
                .iter()
                .map(|c| build_node(taffy, c, metrics))
                .collect();
            taffy.new_with_children(st, &kids).unwrap()
        }
        Element::Scroll { gap, pad, children } => {
            let st = stack_style(*gap, *pad, None, FlexDirection::Column);
            let kids: Vec<NodeId> = children
                .iter()
                .map(|c| build_node(taffy, c, metrics))
                .collect();
            taffy.new_with_children(st, &kids).unwrap()
        }
        Element::ListItem {
            gap,
            pad,
            children,
            style,
            ..
        } => {
            let st = stack_style(*gap, *pad, style.as_ref(), FlexDirection::Column);
            let kids: Vec<NodeId> = children
                .iter()
                .map(|c| build_node(taffy, c, metrics))
                .collect();
            taffy.new_with_children(st, &kids).unwrap()
        }
        Element::Text {
            value, size, wrap, ..
        } => {
            let font_size = size.unwrap_or(crate::render::DEFAULT_FONT_SIZE);
            let st = taffy::Style {
                ..taffy::Style::DEFAULT
            };
            taffy
                .new_leaf_with_context(
                    st,
                    MeasureCtx {
                        value: value.clone(),
                        font_size,
                        wrap: *wrap,
                    },
                )
                .unwrap()
        }
        Element::Divider => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::auto(),
                    height: Dimension::length(1.0),
                },
                min_size: Size {
                    width: Dimension::length(20.0),
                    height: Dimension::length(1.0),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        Element::Spacer { size } => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::length(*size),
                    height: Dimension::length(*size),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        Element::Badge { value, .. } => taffy
            .new_leaf_with_context(
                taffy::Style {
                    padding: Rect {
                        left: LengthPercentage::length(crate::render::BADGE_PAD_X),
                        right: LengthPercentage::length(crate::render::BADGE_PAD_X),
                        top: LengthPercentage::length(crate::render::BADGE_PAD_Y),
                        bottom: LengthPercentage::length(crate::render::BADGE_PAD_Y),
                    },
                    ..taffy::Style::DEFAULT
                },
                MeasureCtx {
                    value: value.clone(),
                    font_size: crate::render::BADGE_FONT_SIZE,
                    wrap: true,
                },
            )
            .unwrap(),
        Element::Button { label, .. } => taffy
            .new_leaf_with_context(
                taffy::Style {
                    padding: Rect {
                        left: LengthPercentage::length(crate::render::BUTTON_PAD_X),
                        right: LengthPercentage::length(crate::render::BUTTON_PAD_X),
                        top: LengthPercentage::length(crate::render::BUTTON_PAD_Y),
                        bottom: LengthPercentage::length(crate::render::BUTTON_PAD_Y),
                    },
                    ..taffy::Style::DEFAULT
                },
                MeasureCtx {
                    value: label.clone(),
                    font_size: crate::render::DEFAULT_FONT_SIZE,
                    wrap: true,
                },
            )
            .unwrap(),
        Element::Link { label, .. } => taffy
            .new_leaf_with_context(
                taffy::Style::DEFAULT,
                MeasureCtx {
                    value: label.clone(),
                    font_size: crate::render::DEFAULT_FONT_SIZE,
                    wrap: true,
                },
            )
            .unwrap(),
        Element::Tooltip { label, .. } => taffy
            .new_leaf_with_context(
                taffy::Style::DEFAULT,
                MeasureCtx {
                    value: label.clone(),
                    font_size: crate::render::DEFAULT_FONT_SIZE,
                    wrap: true,
                },
            )
            .unwrap(),
        Element::Bar { width, height, .. } | Element::Slider { width, height, .. } => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::length(*width),
                    height: Dimension::length(*height),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        Element::Stepper { .. } => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::length(crate::render::STEPPER_W),
                    height: Dimension::length(crate::render::STEPPER_H),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        Element::Select { width, .. } => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::length(*width),
                    height: Dimension::length(crate::render::SELECT_H),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        Element::Popover { width, .. } => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::length(*width),
                    height: Dimension::length(crate::render::SELECT_H),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        // Dialog + Toast have no in-pane footprint — they render on the overlay.
        Element::Dialog { .. } | Element::Toast { .. } => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::length(0.0),
                    height: Dimension::length(0.0),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        Element::Swatch { size, .. } | Element::SwatchButton { size, .. } => taffy
            .new_leaf(taffy::Style {
                size: Size {
                    width: Dimension::length(*size),
                    height: Dimension::length(*size),
                },
                ..taffy::Style::DEFAULT
            })
            .unwrap(),
        Element::Tabs { items, .. } => {
            // Tabs render as an hstack of tab cells; build a row of
            // text-shaped leaves so Taffy gives each one its own slot.
            let cell_kids: Vec<NodeId> = items
                .iter()
                .map(|t| {
                    taffy
                        .new_leaf_with_context(
                            taffy::Style {
                                padding: Rect {
                                    left: LengthPercentage::length(crate::render::TAB_PAD_X),
                                    right: LengthPercentage::length(crate::render::TAB_PAD_X),
                                    top: LengthPercentage::length(crate::render::TAB_PAD_Y),
                                    bottom: LengthPercentage::length(
                                        crate::render::TAB_PAD_Y + crate::render::TAB_INDICATOR_H,
                                    ),
                                },
                                ..taffy::Style::DEFAULT
                            },
                            MeasureCtx {
                                value: t.label.clone(),
                                font_size: crate::render::DEFAULT_FONT_SIZE,
                                wrap: true,
                            },
                        )
                        .unwrap()
                })
                .collect();
            taffy
                .new_with_children(
                    taffy::Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Row,
                        gap: Size {
                            width: LengthPercentage::length(crate::render::TAB_GAP),
                            height: LengthPercentage::length(0.0),
                        },
                        ..taffy::Style::DEFAULT
                    },
                    &cell_kids,
                )
                .unwrap()
        }
        Element::RadioGroup { options, .. } => {
            // A column of option rows. Each cell measures its label with a left
            // padding reserving room for the ring (drawn into that padding).
            let cells: Vec<NodeId> = options
                .iter()
                .map(|o| {
                    taffy
                        .new_leaf_with_context(
                            taffy::Style {
                                padding: Rect {
                                    left: LengthPercentage::length(
                                        crate::render::RADIO_RING + crate::render::RADIO_GAP,
                                    ),
                                    right: LengthPercentage::length(0.0),
                                    top: LengthPercentage::length(crate::render::RADIO_PAD_Y),
                                    bottom: LengthPercentage::length(crate::render::RADIO_PAD_Y),
                                },
                                ..taffy::Style::DEFAULT
                            },
                            MeasureCtx {
                                value: o.label.clone(),
                                font_size: crate::render::DEFAULT_FONT_SIZE,
                                wrap: true,
                            },
                        )
                        .unwrap()
                })
                .collect();
            taffy
                .new_with_children(
                    taffy::Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Column,
                        gap: Size {
                            width: LengthPercentage::length(0.0),
                            height: LengthPercentage::length(crate::render::RADIO_GROUP_GAP),
                        },
                        ..taffy::Style::DEFAULT
                    },
                    &cells,
                )
                .unwrap()
        }
        Element::Toggle { label, .. } => {
            // The toggle is its own bounding box: track + optional label.
            let track_w = crate::render::TOGGLE_TRACK_W;
            let track_h = crate::render::TOGGLE_TRACK_H;
            if label.is_empty() {
                taffy
                    .new_leaf(taffy::Style {
                        size: Size {
                            width: Dimension::length(track_w),
                            height: Dimension::length(track_h),
                        },
                        ..taffy::Style::DEFAULT
                    })
                    .unwrap()
            } else {
                let label_node = taffy
                    .new_leaf_with_context(
                        taffy::Style::DEFAULT,
                        MeasureCtx {
                            value: label.clone(),
                            font_size: crate::render::DEFAULT_FONT_SIZE,
                            wrap: true,
                        },
                    )
                    .unwrap();
                let track_node = taffy
                    .new_leaf(taffy::Style {
                        size: Size {
                            width: Dimension::length(track_w),
                            height: Dimension::length(track_h),
                        },
                        flex_shrink: 0.0,
                        ..taffy::Style::DEFAULT
                    })
                    .unwrap();
                taffy
                    .new_with_children(
                        taffy::Style {
                            display: Display::Flex,
                            flex_direction: FlexDirection::Row,
                            align_items: Some(AlignItems::Center),
                            gap: Size {
                                width: LengthPercentage::length(8.0),
                                height: LengthPercentage::length(0.0),
                            },
                            ..taffy::Style::DEFAULT
                        },
                        &[label_node, track_node],
                    )
                    .unwrap()
            }
        }
        Element::Checkbox { label, .. } => {
            // box + optional label, laid out as a centered row (like Toggle).
            let box_d = crate::render::CHECKBOX_SIZE;
            if label.is_empty() {
                taffy
                    .new_leaf(taffy::Style {
                        size: Size {
                            width: Dimension::length(box_d),
                            height: Dimension::length(box_d),
                        },
                        ..taffy::Style::DEFAULT
                    })
                    .unwrap()
            } else {
                let box_node = taffy
                    .new_leaf(taffy::Style {
                        size: Size {
                            width: Dimension::length(box_d),
                            height: Dimension::length(box_d),
                        },
                        flex_shrink: 0.0,
                        ..taffy::Style::DEFAULT
                    })
                    .unwrap();
                let label_node = taffy
                    .new_leaf_with_context(
                        taffy::Style::DEFAULT,
                        MeasureCtx {
                            value: label.clone(),
                            font_size: crate::render::DEFAULT_FONT_SIZE,
                            wrap: true,
                        },
                    )
                    .unwrap();
                taffy
                    .new_with_children(
                        taffy::Style {
                            display: Display::Flex,
                            flex_direction: FlexDirection::Row,
                            align_items: Some(AlignItems::Center),
                            gap: Size {
                                width: LengthPercentage::length(8.0),
                                height: LengthPercentage::length(0.0),
                            },
                            ..taffy::Style::DEFAULT
                        },
                        &[box_node, label_node],
                    )
                    .unwrap()
            }
        }
        Element::Input { width, style, .. } => {
            // The `width` field is the DEFAULT; `style` (flex_grow,
            // width/height incl. `"100%"`, min/max) overrides it so an
            // input can fill / grow within its pane like any stack does.
            let mut s = taffy::Style {
                size: Size {
                    width: Dimension::length(*width),
                    height: Dimension::length(crate::render::INPUT_HEIGHT),
                },
                ..taffy::Style::DEFAULT
            };
            apply_style_overrides(&mut s, style.as_ref());
            taffy.new_leaf(s).unwrap()
        }
        Element::Editor { style, .. } => {
            // A live editor portal. Default to a roomy box; `style`
            // (flex_grow, width/height incl. "100%", min/max) overrides so
            // it can fill its pane like any other leaf.
            let mut s = taffy::Style {
                size: Size {
                    width: Dimension::length(420.0),
                    height: Dimension::length(280.0),
                },
                ..taffy::Style::DEFAULT
            };
            apply_style_overrides(&mut s, style.as_ref());
            taffy.new_leaf(s).unwrap()
        }
        Element::TextArea {
            width,
            rows,
            value,
            style,
            ..
        } => {
            // Auto-grow: height fits the wrapped content, with `rows` as
            // the minimum. Wrapping here uses the same routine + width as
            // the renderer, so the box height matches the drawn text.
            // (While typing, this tracks the element's `value`, which a
            // widget refreshes from `on_input_change`.)
            let line_h = crate::render::line_height(crate::render::DEFAULT_FONT_SIZE);
            let avail = (*width - 2.0 * crate::render::INPUT_PAD_X).max(1.0);
            let chars: Vec<char> = value.chars().collect();
            let wrapped = crate::render::wrap_visual_lines(&chars, metrics, avail).len() as u32;
            let lines = wrapped.max((*rows).max(1));
            let height = lines as f32 * line_h + 2.0 * crate::render::TEXTAREA_PAD_Y;
            // `width`/`rows` are the DEFAULTS; `style` overrides them so a
            // query editor can `flex_grow` / `height: "100%"` to fill a
            // docked editor pane instead of staying a fixed rows-tall box.
            let mut s = taffy::Style {
                size: Size {
                    width: Dimension::length(*width),
                    height: Dimension::length(height),
                },
                ..taffy::Style::DEFAULT
            };
            apply_style_overrides(&mut s, style.as_ref());
            taffy.new_leaf(s).unwrap()
        }
        Element::Table { columns, rows, .. } => {
            use taffy::style::{
                GridTemplateComponent, MaxTrackSizingFunction, MinTrackSizingFunction,
            };
            // Max width an auto (unsized) column grows to before its text
            // wraps. Keeps a long cell from ballooning the whole table.
            const COL_CAP: f32 = 260.0;
            let ncols = columns.len().max(1);
            // One grid track per column. A fixed `width` becomes a rigid
            // track; otherwise the column fits its content, capped at
            // COL_CAP (longer text wraps). We avoid `fr` here because the
            // widget layout root is content-sized, so `fr` has no
            // definite width to distribute and one column would balloon.
            let mut tracks: Vec<GridTemplateComponent<_>> = columns
                .iter()
                .map(|c| {
                    let track = match c.width {
                        Some(w) => minmax(
                            MinTrackSizingFunction::length(w),
                            MaxTrackSizingFunction::length(w),
                        ),
                        // Grow to content, but cap at COL_CAP so a long
                        // cell wraps instead of stretching the table. A
                        // fixed-length max caps even when the grid has no
                        // definite outer width (unlike `fit-content`).
                        None => minmax(
                            MinTrackSizingFunction::auto(),
                            MaxTrackSizingFunction::length(COL_CAP),
                        ),
                    };
                    GridTemplateComponent::Single(track)
                })
                .collect();
            if tracks.is_empty() {
                tracks.push(GridTemplateComponent::Single(minmax(
                    MinTrackSizingFunction::auto(),
                    MaxTrackSizingFunction::length(COL_CAP),
                )));
            }
            let st = taffy::Style {
                display: Display::Grid,
                grid_template_columns: tracks,
                // Column gap so adjacent cells (esp. a wrapped cell next
                // to a right-aligned one) don't visually touch.
                gap: Size {
                    width: LengthPercentage::length(crate::render::TABLE_COL_GAP),
                    height: LengthPercentage::length(0.0),
                },
                ..taffy::Style::DEFAULT
            };
            // Cells in row-major order: header row first, then data rows.
            // Each is a measured text leaf so it wraps within its column
            // and the row track grows to the tallest cell.
            let mut cells: Vec<NodeId> = Vec::with_capacity(ncols * (rows.len() + 1));
            for c in columns {
                cells.push(table_cell_leaf(taffy, &c.header));
            }
            for row in rows {
                for ci in 0..ncols {
                    let txt = row.get(ci).map(|s| s.as_str()).unwrap_or("");
                    cells.push(table_cell_leaf(taffy, txt));
                }
            }
            taffy.new_with_children(st, &cells).unwrap()
        }
        // A nested Canvas is a sized leaf in the flex tree: it draws its
        // own children absolutely, so it has no intrinsic content size.
        // Apply the style overrides (width/height/flex_grow/min_*) so it
        // can claim space; with no style it stays zero-size (the old
        // behavior). A top-level Canvas never reaches here — it bypasses
        // the flow layout entirely.
        Element::Canvas { style, .. } => {
            let mut st = taffy::Style::DEFAULT;
            apply_style_overrides(&mut st, style.as_ref());
            taffy.new_leaf(st).unwrap()
        }
    }
}

/// One table cell: a measured text leaf with cell padding, so it wraps
/// to its column width and contributes its height to the row track.
fn table_cell_leaf(taffy: &mut TaffyTree<MeasureCtx>, text: &str) -> NodeId {
    taffy
        .new_leaf_with_context(
            taffy::Style {
                padding: Rect {
                    left: LengthPercentage::length(crate::render::TABLE_CELL_PAD_X),
                    right: LengthPercentage::length(crate::render::TABLE_CELL_PAD_X),
                    top: LengthPercentage::length(crate::render::TABLE_CELL_PAD_Y),
                    bottom: LengthPercentage::length(crate::render::TABLE_CELL_PAD_Y),
                },
                ..taffy::Style::DEFAULT
            },
            MeasureCtx {
                value: text.to_string(),
                font_size: crate::render::DEFAULT_FONT_SIZE,
                wrap: true,
            },
        )
        .unwrap()
}

/// Build a Taffy style for a flex stack (vstack / hstack / frame /
/// scroll / list-item).
fn stack_style(gap: f32, pad: f32, style: Option<&PStyle>, dir: FlexDirection) -> taffy::Style {
    let padding = effective_padding(style, pad);
    let margin = effective_margin(style);
    // A Glaze `direction` override (e.g. from a `when` breakpoint) can flip the
    // container between row and column independent of the Element variant.
    let dir = match style.and_then(|s| s.flex_direction.as_deref()) {
        Some("row") => FlexDirection::Row,
        Some("column") | Some("col") => FlexDirection::Column,
        _ => dir,
    };
    let mut s = taffy::Style {
        display: Display::Flex,
        flex_direction: dir,
        gap: Size {
            width: LengthPercentage::length(gap),
            height: LengthPercentage::length(gap),
        },
        padding: rect_from(padding),
        margin: rect_from_signed(margin),
        ..taffy::Style::DEFAULT
    };
    apply_style_overrides(&mut s, style);
    s
}

fn effective_padding(style: Option<&PStyle>, pad: f32) -> Edges {
    style
        .and_then(|s| s.padding.as_ref())
        .copied()
        .unwrap_or_else(|| Edges::all(pad))
}

fn effective_margin(style: Option<&PStyle>) -> Edges {
    style
        .and_then(|s| s.margin.as_ref())
        .copied()
        .unwrap_or_default()
}

fn rect_from(e: Edges) -> Rect<LengthPercentage> {
    Rect {
        left: LengthPercentage::length(e.left),
        right: LengthPercentage::length(e.right),
        top: LengthPercentage::length(e.top),
        bottom: LengthPercentage::length(e.bottom),
    }
}

fn rect_from_signed(e: Edges) -> Rect<LengthPercentageAuto> {
    Rect {
        left: LengthPercentageAuto::length(e.left),
        right: LengthPercentageAuto::length(e.right),
        top: LengthPercentageAuto::length(e.top),
        bottom: LengthPercentageAuto::length(e.bottom),
    }
}

/// Pull min/max/explicit size from the Style overrides.
fn apply_style_overrides(s: &mut taffy::Style, style: Option<&PStyle>) {
    let Some(style) = style else { return };
    if let Some(g) = style.flex_grow {
        s.flex_grow = g;
    }
    if let Some(sh) = style.flex_shrink {
        s.flex_shrink = sh;
    }
    if let Some(w) = style.width.as_deref().and_then(parse_dimension) {
        s.size.width = w;
    }
    if let Some(h) = style.height.as_deref().and_then(parse_dimension) {
        s.size.height = h;
    }
    if let Some(mw) = style.min_width.as_deref().and_then(parse_dimension) {
        s.min_size.width = mw;
    }
    if let Some(mh) = style.min_height.as_deref().and_then(parse_dimension) {
        s.min_size.height = mh;
    }
    if let Some(mw) = style.max_width.as_deref().and_then(parse_dimension) {
        s.max_size.width = mw;
    }
    if let Some(mh) = style.max_height.as_deref().and_then(parse_dimension) {
        s.max_size.height = mh;
    }
    if let Some(a) = style.align_self {
        s.align_self = Some(align_to_taffy(a));
    }
    // Soft-wrap children onto multiple rows/lines (e.g. a row of colored code
    // runs becoming a wrapped code line) instead of overflowing the main axis.
    if style.flex_wrap == Some(true) {
        s.flex_wrap = taffy::style::FlexWrap::Wrap;
    }
    // `clip: true` makes this a horizontal clip boundary. Telling Taffy the
    // box clips on the x-axis means overflowing children no longer inflate
    // its content size, so the box stays at its laid-out (e.g. stretched)
    // width — which is the width the renderer truncates descendant text to.
    if style.clip == Some(true) {
        s.overflow = taffy::geometry::Point {
            x: taffy::style::Overflow::Clip,
            y: taffy::style::Overflow::Visible,
        };
    }
}

/// Parse a Style width/height/min/max string.
/// - `"123"` or `"123.5"` → pixels
/// - `"50%"` → percent of parent
/// - `"auto"` → intrinsic
/// Returns `None` for unparseable values; the caller leaves the default.
fn parse_dimension(s: &str) -> Option<Dimension> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("auto") {
        return Some(Dimension::auto());
    }
    if let Some(rest) = t.strip_suffix('%') {
        return rest
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| Dimension::percent(n / 100.0));
    }
    t.parse::<f32>().ok().map(Dimension::length)
}

fn align_to_taffy(a: Align) -> AlignItems {
    match a {
        Align::Start => AlignItems::FlexStart,
        Align::Center => AlignItems::Center,
        Align::End => AlignItems::FlexEnd,
        Align::Stretch => AlignItems::Stretch,
    }
}

/// Compute layout for the tree rooted at `root` within the given
/// `(max_w, max_h)` viewport. `metrics` is used to size text leaves.
pub fn compute(laid: &mut LaidOut, max_w: f32, max_h: f32, metrics: &PaneFontMetrics) {
    let _prof = jim_pane::prof::sys_span_nested("taffy_layout");
    // Force the root to fill the available content width. Without this the root
    // (auto width) shrinks to its content, so `grow`/stretch children have no
    // free space to distribute and text leaves measure/wrap at a collapsed
    // width. Pinning the root to the content width makes flex layouts reflow as
    // the pane resizes, and text wrap at the true available width.
    if let Ok(style) = laid.taffy.style(laid.root) {
        let mut s = style.clone();
        s.size.width = Dimension::length(max_w);
        let _ = laid.taffy.set_style(laid.root, s);
    }
    let m = *metrics;
    laid.taffy
        .compute_layout_with_measure(
            laid.root,
            Size {
                width: AvailableSpace::Definite(max_w),
                height: AvailableSpace::Definite(max_h),
            },
            move |known, available, _node, context, _style| {
                let Some(ctx) = context else {
                    return Size::ZERO;
                };
                measure_text(ctx, known, available, &m)
            },
        )
        .expect("taffy compute_layout");
}

/// Measure callback for text + button + badge leaves. Wraps at the
/// available width when the text overflows; returns (width, height).
fn measure_text(
    ctx: &MeasureCtx,
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
    metrics: &PaneFontMetrics,
) -> Size<f32> {
    let line_h = crate::render::line_height(ctx.font_size);
    if let (Some(w), Some(h)) = (known.width, known.height) {
        return Size {
            width: w,
            height: h,
        };
    }
    let char_w = metrics.char_width(ctx.font_size);
    // Min-content width is the widest *unbreakable word*, NOT the whole line.
    // If a text leaf reported its full single-line width as its minimum, flex
    // containers could never shrink it — so content would always overflow the
    // pane and the layout wouldn't reflow on resize. With the longest word as
    // the floor, text shrinks and wraps, and containers can adapt to width.
    let longest_word = ctx
        .value
        .split_whitespace()
        .map(|w| w.chars().count() as f32 * char_w)
        .fold(0.0_f32, f32::max);
    let max_w = match available.width {
        AvailableSpace::Definite(w) => w,
        AvailableSpace::MinContent => longest_word.max(char_w),
        AvailableSpace::MaxContent => f32::INFINITY,
    };
    // Count visual lines and the widest line, honoring HARD `\n` breaks: each
    // newline-delimited segment is laid out independently and word-wrapped to
    // `max_w`. Without this, a multi-line value (e.g. a code block) was sized
    // as a single line and its text overflowed the box. A blank segment still
    // occupies one line, and a value with no `\n` behaves exactly as before.
    // `wrap: false` (code/diff rows) forces a single line: every segment is
    // measured at its full width regardless of the available space, so the
    // leaf's min- AND max-content width both equal the full line and no flex
    // container can squeeze it into a taller wrapped box.
    let wrappable = ctx.wrap && char_w > 0.0 && max_w > 0.0 && max_w.is_finite();
    let mut total_lines: u32 = 0;
    let mut max_line_w: f32 = 0.0;
    for segment in ctx.value.split('\n') {
        let seg_w = metrics.measure(segment, ctx.font_size);
        if !wrappable || seg_w <= max_w {
            max_line_w = max_line_w.max(seg_w);
            total_lines += 1;
            continue;
        }
        // Word-wrap this segment: accumulate words per line until the next
        // word would overflow, then start a new line.
        let mut lines: u32 = 1;
        let mut line_w: f32 = 0.0;
        let mut first_word = true;
        for word in segment.split_whitespace() {
            let w = word.chars().count() as f32 * char_w;
            let added = if first_word { w } else { char_w + w };
            if !first_word && line_w + added > max_w {
                max_line_w = max_line_w.max(line_w);
                lines += 1;
                line_w = w;
            } else {
                line_w += added;
                first_word = false;
            }
        }
        max_line_w = max_line_w.max(line_w);
        total_lines += lines;
    }
    let total_lines = total_lines.max(1);
    // Clamp to the available width only when we actually wrapped; a no-wrap
    // line reports its full width so it overflows (and clips) instead of
    // having its box shrunk under the text, which would re-introduce wrapping.
    let width = if wrappable && max_w.is_finite() {
        max_line_w.min(max_w)
    } else {
        max_line_w
    };
    Size {
        width,
        height: line_h * total_lines as f32,
    }
}

/// Convenience: absolute position of a node within the root's coord
/// frame. Taffy stores per-node `location` relative to the node's
/// parent; absolute position requires summing across the ancestor
/// chain. The renderer walks the tree top-down anyway, so it carries
/// the running origin itself — this helper exists for ad-hoc
/// queries (e.g. snapshot tools).
pub fn absolute_position(laid: &LaidOut, mut node: NodeId, root: NodeId) -> Vec2 {
    let mut total = Vec2::ZERO;
    while node != root {
        let layout = laid.layout(node);
        total.x += layout.location.x;
        total.y += layout.location.y;
        let Some(parent) = laid.taffy.parent(node) else {
            break;
        };
        node = parent;
    }
    let root_layout = laid.layout(root);
    total.x += root_layout.location.x;
    total.y += root_layout.location.y;
    total
}

// Compile-time checks that the Style overrides import compiles
// against the protocol types we expect.
#[allow(dead_code)]
fn _check_style_imports(_b: Border, _sh: Shadow, _w: Weight, _k: ButtonKind) {}

// ===========================================================================
// Clip / truncation
//
// Bevy's `Text2d` + `TextBounds` only governs WRAPPING, not clipping — a
// single-line (`no_wrap`) run ignores the bounds width and renders its full
// length (cut only by the per-pane camera at the pane edge). So to contain a
// long line inside an inner box (e.g. a diff row's tint), we can't clip pixels
// — we shorten the STRING to fit the box. The host font is monospace, so this
// is pixel-exact. `resolve_clipped_runs` is a pure mirror of `render_node`'s
// clip propagation so the behavior is unit-testable with no Bevy World.
// ===========================================================================

/// Longest char-boundary prefix of `text` whose rendered width at `font_size`
/// fits within `avail_w` px (monospace-exact via [`PaneFontMetrics`]).
/// `avail_w <= 0` → empty; non-positive char width → text unchanged.
pub fn truncate_to_width(
    text: &str,
    font_size: f32,
    avail_w: f32,
    metrics: &PaneFontMetrics,
) -> String {
    if avail_w <= 0.0 {
        return String::new();
    }
    let cw = metrics.char_width(font_size);
    if cw <= 0.0 {
        return text.to_string();
    }
    let max_chars = (avail_w / cw).floor() as usize;
    let n = text.chars().count();
    if n <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// One text run after clip resolution — what would actually be drawn.
#[derive(Debug, Clone)]
pub struct ClippedRun {
    /// Absolute top-left of the run in content-root space.
    pub origin: Vec2,
    /// Clip boundary inherited from the nearest `clip:true` ancestor (None if
    /// no clipping ancestor).
    pub clip_right: Option<f32>,
    /// The original (untruncated) string.
    pub full: String,
    /// The string after clip-truncation — this is what renders.
    pub rendered: String,
    pub font_size: f32,
}

impl ClippedRun {
    /// Right edge of the rendered (truncated) text, in content space.
    pub fn rendered_right(&self, metrics: &PaneFontMetrics) -> f32 {
        self.origin.x + metrics.measure(&self.rendered, self.font_size)
    }
}

fn el_children(el: &Element) -> &[Element] {
    match el {
        Element::Vstack { children, .. }
        | Element::Hstack { children, .. }
        | Element::Frame { children, .. }
        | Element::Scroll { children, .. }
        | Element::ListItem { children, .. } => children,
        _ => &[],
    }
}

fn el_clips(el: &Element) -> bool {
    let style = match el {
        Element::Vstack { style, .. }
        | Element::Hstack { style, .. }
        | Element::Frame { style, .. }
        | Element::ListItem { style, .. } => style.as_ref(),
        _ => None,
    };
    style.and_then(|s| s.clip) == Some(true)
}

fn walk_clips(
    laid: &LaidOut,
    node: NodeId,
    el: &Element,
    origin: Vec2,
    clip_right: Option<f32>,
    metrics: &PaneFontMetrics,
    out: &mut Vec<ClippedRun>,
) {
    let size = laid.layout(node).size;
    if let Element::Text { value, size: fs, .. } = el {
        let font_size = fs.unwrap_or(crate::render::DEFAULT_FONT_SIZE);
        let rendered = match clip_right {
            Some(cr) => truncate_to_width(value, font_size, (cr - origin.x).max(0.0), metrics),
            None => value.clone(),
        };
        out.push(ClippedRun {
            origin,
            clip_right,
            full: value.clone(),
            rendered,
            font_size,
        });
        return;
    }
    // Same rule as `render_node::recurse_children`: a clip container tightens
    // the right boundary to its own laid-out right edge.
    let child_clip = if el_clips(el) {
        let edge = origin.x + size.width;
        Some(clip_right.map_or(edge, |c| c.min(edge)))
    } else {
        clip_right
    };
    let kids = el_children(el);
    let cids = laid.taffy.children(node).unwrap_or_default();
    for (cid, child) in cids.iter().zip(kids.iter()) {
        let cl = laid.layout(*cid);
        let cpos = origin + Vec2::new(cl.location.x, cl.location.y);
        walk_clips(laid, *cid, child, cpos, child_clip, metrics, out);
    }
}

/// Lay out `el` in a `content_w × content_h` viewport and resolve every text
/// run's clip-truncation — a headless mirror of what `render_node` draws.
/// Used by tests to assert containment without a GPU/World.
pub fn resolve_clipped_runs(
    el: &Element,
    metrics: &PaneFontMetrics,
    content_w: f32,
    content_h: f32,
) -> Vec<ClippedRun> {
    let mut laid = build_tree(el, metrics);
    compute(&mut laid, content_w, content_h, metrics);
    let root_layout = laid.layout(laid.root);
    let root_origin = Vec2::new(root_layout.location.x, root_layout.location.y);
    let mut out = Vec::new();
    walk_clips(&laid, laid.root, el, root_origin, None, metrics, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Style;

    fn metrics() -> PaneFontMetrics {
        PaneFontMetrics {
            cell_width: 8.4,
            font_size: 14.0,
        }
    }

    fn pct_height(p: &str) -> Style {
        Style {
            height: Some(p.into()),
            ..Default::default()
        }
    }

    // ---- clip / truncation helpers ----
    fn text_run(s: &str, size: f32) -> Element {
        Element::Text {
            value: s.into(),
            color: None,
            size: Some(size),
            weight: None,
            family: None,
            selectable: true,
            wrap: false,
        }
    }
    /// A fixed-width clip frame.
    fn clip_frame(width_px: &str, children: Vec<Element>) -> Element {
        Element::Frame {
            gap: 0.0,
            pad: 0.0,
            style: Some(Style {
                width: Some(width_px.into()),
                clip: Some(true),
                ..Default::default()
            }),
            children,
        }
    }
    fn hstack(children: Vec<Element>) -> Element {
        Element::Hstack {
            gap: 0.0,
            pad: 0.0,
            align: Align::Start,
            style: None,
            children,
        }
    }

    #[test]
    fn truncate_to_width_boundaries() {
        let m = metrics(); // cell_width 8.4 @ 14px
        let cw = m.char_width(14.0);
        assert_eq!(truncate_to_width("hello", 14.0, 1000.0, &m), "hello", "fits → unchanged");
        assert_eq!(truncate_to_width("hello", 14.0, 0.0, &m), "", "zero width → empty");
        assert_eq!(truncate_to_width("hello", 14.0, -5.0, &m), "", "negative → empty");
        // exactly 3 chars of room
        assert_eq!(truncate_to_width("hello", 14.0, cw * 3.0 + 0.1, &m), "hel");
        // just under 3 → 2
        assert_eq!(truncate_to_width("hello", 14.0, cw * 3.0 - 0.1, &m), "he");
        // unicode: count by chars, never split a codepoint
        assert_eq!(truncate_to_width("héllo", 14.0, cw * 2.0 + 0.1, &m), "hé");
    }

    /// THE core property (the bug): a long single line in a clip box must render
    /// no wider than the box — at any box width — and stay on a char boundary.
    #[test]
    fn long_line_never_exceeds_clip_box() {
        let m = metrics();
        let long = "abcdefghij".repeat(40); // 400 chars
        for w in ["40", "100", "237", "512"] {
            let el = clip_frame(w, vec![text_run(&long, 14.0)]);
            let runs = resolve_clipped_runs(&el, &m, 1000.0, 600.0);
            assert_eq!(runs.len(), 1);
            let r = &runs[0];
            assert!(r.rendered.chars().count() < r.full.chars().count(), "w={w}: should truncate");
            let clip = r.clip_right.expect("inside clip box");
            assert!(
                r.rendered_right(&m) <= clip + 0.01,
                "w={w}: rendered right {} must not exceed clip {}",
                r.rendered_right(&m),
                clip
            );
        }
    }

    /// Multi-run (syntax-highlighted) line: every colored run shares the box's
    /// right edge — none may bleed past it, and runs starting past it vanish.
    #[test]
    fn multi_run_line_all_runs_contained() {
        let m = metrics();
        let el = clip_frame(
            "120",
            vec![hstack(vec![
                text_run(&"a".repeat(10), 14.0),
                text_run(&"b".repeat(10), 14.0),
                text_run(&"c".repeat(40), 14.0), // pushes well past the box
            ])],
        );
        let runs = resolve_clipped_runs(&el, &m, 1000.0, 600.0);
        assert_eq!(runs.len(), 3, "three runs");
        for r in &runs {
            let clip = r.clip_right.expect("inside clip box");
            assert!(
                r.rendered_right(&m) <= clip + 0.01,
                "run {:?} rendered right {} exceeds clip {}",
                r.full,
                r.rendered_right(&m),
                clip
            );
        }
    }

    /// Nested clip boxes: the tighter (inner) boundary wins.
    #[test]
    fn nested_clip_uses_tightest() {
        let m = metrics();
        let long = "z".repeat(200);
        let el = clip_frame("300", vec![clip_frame("80", vec![text_run(&long, 14.0)])]);
        let runs = resolve_clipped_runs(&el, &m, 1000.0, 600.0);
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        let clip = r.clip_right.unwrap();
        assert!((clip - 80.0).abs() < 0.5, "tightest clip should be 80, got {clip}");
        assert!(r.rendered_right(&m) <= clip + 0.01);
    }

    /// No clipping ancestor: text is left untouched (overflow handled elsewhere
    /// by the pane camera).
    #[test]
    fn no_clip_leaves_text_untouched() {
        let m = metrics();
        let long = "q".repeat(200);
        let el = Element::Frame {
            gap: 0.0,
            pad: 0.0,
            style: Some(Style {
                width: Some("100".into()),
                ..Default::default()
            }),
            children: vec![text_run(&long, 14.0)],
        };
        let runs = resolve_clipped_runs(&el, &m, 1000.0, 600.0);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].clip_right.is_none());
        assert_eq!(runs[0].rendered, runs[0].full, "no clip → unchanged");
    }

    /// Short text inside a clip box is never altered.
    #[test]
    fn short_text_in_clip_unchanged() {
        let m = metrics();
        let el = clip_frame("400", vec![text_run("short line", 14.0)]);
        let runs = resolve_clipped_runs(&el, &m, 1000.0, 600.0);
        assert_eq!(runs[0].rendered, "short line");
    }

    /// A root vstack with `height:"100%"` and a `flex_grow:1` child fills
    /// the available pane height — the whole point of passing the known
    /// pane height (content_size.y) instead of INFINITY. With INFINITY,
    /// the percentage and flex have nothing to distribute and the child
    /// stays content-tall.
    #[test]
    fn root_fills_pane_height_when_height_100pct() {
        let m = metrics();
        let pane_h = 600.0_f32;
        let el = Element::Vstack {
            gap: 0.0,
            pad: 0.0,
            style: Some(pct_height("100%")),
            children: vec![Element::Frame {
                gap: 0.0,
                pad: 0.0,
                style: Some(Style {
                    flex_grow: Some(1.0),
                    ..Default::default()
                }),
                children: vec![],
            }],
        };
        let mut laid = build_tree(&el, &m);
        compute(&mut laid, 400.0, pane_h, &m);

        let root = laid.layout(laid.root);
        assert!(
            (root.size.height - pane_h).abs() < 0.5,
            "root should fill pane height {pane_h}, got {}",
            root.size.height
        );
        let child = laid.taffy.children(laid.root).unwrap()[0];
        let cl = laid.layout(child);
        assert!(
            (cl.size.height - pane_h).abs() < 0.5,
            "flex_grow child should fill {pane_h}, got {}",
            cl.size.height
        );
    }

    /// The table render fix paints the panel/rows from the cells' CONTENT
    /// BOX (union of laid-out cell extents) rather than the node `size`.
    /// This guards the two properties that make that correct in every
    /// case: (1) the content box bounds every cell (so no column can ever
    /// paint outside the panel — the reported bug), and (2) when the grid
    /// tracks already fill the node, the content box equals the node, so
    /// the fix never shrinks a correctly-filled table.
    #[test]
    fn table_content_box_bounds_cells_and_matches_filled_node() {
        use crate::protocol::TableColumn;
        let m = metrics();
        let col = |h: &str, a| TableColumn {
            header: h.into(),
            width: None,
            align: a,
        };
        let table = Element::Table {
            columns: vec![col("name", Align::Start), col("age", Align::End)],
            rows: vec![
                vec!["Widget".into(), "30".into()],
                vec!["Gadget".into(), "25".into()],
            ],
            zebra: true,
            selectable: false,
            style: None,
        };
        let el = Element::Vstack {
            gap: 0.0,
            pad: 0.0,
            style: Some(Style {
                width: Some("400".into()),
                ..Default::default()
            }),
            children: vec![table],
        };
        let mut laid = build_tree(&el, &m);
        compute(&mut laid, 400.0, 600.0, &m);

        let table_node = laid.taffy.children(laid.root).unwrap()[0];
        let node_w = laid.layout(table_node).size.width;

        // Cells' content box, computed exactly like render_table_at.
        let cells = laid.taffy.children(table_node).unwrap();
        let mut content_x = 0.0_f32;
        for &c in &cells {
            let cl = laid.layout(c);
            let right = cl.location.x + cl.size.width;
            // (1) every cell is within the content box by construction.
            assert!(
                right <= content_x.max(right) + 0.01,
                "cell right edge {right} must be within content box"
            );
            content_x = content_x.max(right);
        }
        assert!(content_x > 0.0, "content box should be non-degenerate");
        // (2) non-destructive: when tracks fill the node, panel == cells.
        assert!(
            (content_x - node_w).abs() < 1.0,
            "content box ({content_x}) should match the filled node ({node_w}) \
             so the fix doesn't shrink a correct table"
        );
    }

    /// Default (no height set) still content-sizes, so taller-than-pane
    /// content grows past the pane and scrolls — passing the pane height
    /// as the available space must NOT clamp an auto-height root.
    #[test]
    fn root_without_height_still_content_sizes() {
        let m = metrics();
        // Three stacked fixed-height frames = 300px of content, in a
        // 100px pane. An auto-height root must report ~300, not 100.
        let frame = |h: f32| Element::Frame {
            gap: 0.0,
            pad: 0.0,
            style: Some(Style {
                height: Some(format!("{h}")),
                ..Default::default()
            }),
            children: vec![],
        };
        let el = Element::Vstack {
            gap: 0.0,
            pad: 0.0,
            style: None,
            children: vec![frame(100.0), frame(100.0), frame(100.0)],
        };
        let mut laid = build_tree(&el, &m);
        compute(&mut laid, 400.0, 100.0, &m);
        let root = laid.layout(laid.root);
        assert!(
            (root.size.height - 300.0).abs() < 0.5,
            "auto-height root should grow to content 300, got {}",
            root.size.height
        );
    }
}
