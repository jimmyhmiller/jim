//! Measure widget text with the font engine that draws it.
//!
//! Widget layout used to size every text leaf as monospace cells
//! ([`crate::layout::GridMeasure`]). That is exact for code and wrong for
//! everything in the proportional UI font, and the error is invisible only
//! while nothing depends on it. Once each text wraps at its OWN box, a label
//! measured a hair narrower than it draws wraps onto a second line — and as
//! the pane resizes, the guessed and real widths trade places, so it
//! flickers between wrapped and unwrapped.
//!
//! [`ShapedMeasure`] asks Bevy's own `TextPipeline` instead: the same
//! `update_buffer` shaping pass `Text2d` is drawn from, with the same font,
//! size, line height and line-break mode the renderer gives the entity. It
//! is how `bevy_ui` measures text for Taffy, and it mirrors that measure
//! function, so a box and the glyphs in it cannot disagree.
//!
//! Shaping happens in PHYSICAL pixels at the window's scale factor — the
//! factor `Text2d` lays out at, since the pane camera renders to the window
//! — and results come back to logical pixels for layout. Line breaks at a
//! scale factor are not the same as line breaks at 1.0 scaled up (hinting
//! and rounding move glyph edges), so measuring at any other factor would
//! reintroduce a smaller version of the same disagreement.

use std::collections::HashMap;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::text::{
    ComputedTextBlock, Font, FontCx, FontSize, LayoutCx, LetterSpacing, LineBreak, LineHeight,
    TextBounds, TextFont, TextLayout, TextMeasureInfo, TextPipeline,
};
use bevy::window::PrimaryWindow;
use taffy::{AvailableSpace, NodeId, Size};

use crate::layout::{MeasureCtx, TextMeasure};

/// Bevy's text-shaping state, borrowed for one widget layout.
#[derive(SystemParam)]
pub struct TextShaper<'w, 's> {
    pipeline: ResMut<'w, TextPipeline>,
    font_cx: ResMut<'w, FontCx>,
    layout_cx: ResMut<'w, LayoutCx>,
    fonts: Res<'w, Assets<Font>>,
    window: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
}

impl TextShaper<'_, '_> {
    /// The factor `Text2d` lays pane text out at: the window's.
    pub fn scale_factor(&self) -> f32 {
        self.window.single().map_or(1.0, |w| w.scale_factor())
    }
}

/// One leaf's shaped text, kept across Taffy's repeated measure calls for
/// that node within a layout.
struct Shaped {
    info: TextMeasureInfo,
    block: ComputedTextBlock,
}

/// A [`TextMeasure`] backed by [`TextShaper`]. Build one per layout pass.
pub struct ShapedMeasure<'a, 'w, 's> {
    shaper: &'a mut TextShaper<'w, 's>,
    /// The font a leaf with this family is drawn in — the renderer's own
    /// resolution (`LayoutCtx::font_for`, else the default font).
    resolve: &'a dyn Fn(Option<&str>) -> Handle<Font>,
    scale: f32,
    shaped: HashMap<NodeId, Shaped>,
    /// Fonts not loaded yet. See [`Self::incomplete`].
    missing: bool,
}

impl<'a, 'w, 's> ShapedMeasure<'a, 'w, 's> {
    pub fn new(
        shaper: &'a mut TextShaper<'w, 's>,
        resolve: &'a dyn Fn(Option<&str>) -> Handle<Font>,
    ) -> Self {
        let scale = shaper.scale_factor();
        Self {
            shaper,
            resolve,
            scale,
            shaped: HashMap::new(),
            missing: false,
        }
    }

    /// Some leaf's font had not been loaded into the font system, so its
    /// size could not be known. The layout is wrong in that case and the
    /// caller must lay the widget out again once fonts arrive — there is no
    /// honest size to guess.
    pub fn incomplete(&self) -> bool {
        self.missing
    }

    fn shape(&mut self, ctx: &MeasureCtx) -> Option<Shaped> {
        // The same line-break mode the renderer gives this entity.
        let linebreak = if !ctx.wrap {
            LineBreak::NoWrap
        } else if ctx.break_words {
            LineBreak::WordOrCharacter
        } else {
            LineBreak::WordBoundary
        };
        let layout = TextLayout::linebreak(linebreak);

        // One section per run, exactly as the renderer spawns them: a rich
        // block's line height comes from its tallest run for every span, a
        // plain leaf is a single span at its own line height.
        let block_size = if ctx.runs.is_empty() {
            ctx.font_size
        } else {
            ctx.runs
                .iter()
                .map(|r| r.font_size)
                .fold(ctx.font_size, f32::max)
        };
        let line_h = LineHeight::Px(crate::render::line_height(block_size));
        let sections: Vec<(String, TextFont)> = if ctx.runs.is_empty() {
            vec![(ctx.value.clone(), self.font(ctx.family.as_deref(), ctx.font_size))]
        } else {
            ctx.runs
                .iter()
                .map(|r| (r.value.clone(), self.font(r.family.as_deref(), r.font_size)))
                .collect()
        };

        let mut block = ComputedTextBlock::default();
        let spans = sections.iter().enumerate().map(|(i, (text, font))| {
            (
                Entity::PLACEHOLDER,
                // Depth: the first section is the Text2d root, the rest are
                // its TextSpan children.
                usize::from(i > 0),
                text.as_str(),
                font,
                Color::WHITE,
                line_h,
                LetterSpacing::default(),
            )
        });
        let TextShaper {
            pipeline,
            font_cx,
            layout_cx,
            fonts,
            window,
        } = &mut *self.shaper;
        let viewport = window
            .single()
            .map_or(Vec2::splat(1000.0), |w| w.resolution.size());
        match pipeline.create_text_measure(
            Entity::PLACEHOLDER,
            fonts,
            spans,
            self.scale,
            &layout,
            &mut block,
            font_cx,
            layout_cx,
            viewport,
            // Widget fonts are all `FontSize::Px`; rem is never consulted.
            16.0,
        ) {
            Ok(info) => Some(Shaped { info, block }),
            Err(e) => {
                warn_once!("[widget] text measure unavailable ({e}); re-laying out once fonts load");
                None
            }
        }
    }

    fn font(&self, family: Option<&str>, size: f32) -> TextFont {
        TextFont {
            font: (self.resolve)(family).into(),
            font_size: FontSize::Px(size),
            ..default()
        }
    }
}

impl TextMeasure for ShapedMeasure<'_, '_, '_> {
    fn measure(
        &mut self,
        node: NodeId,
        ctx: &MeasureCtx,
        known: Size<Option<f32>>,
        available: Size<AvailableSpace>,
    ) -> Size<f32> {
        if let (Some(width), Some(height)) = (known.width, known.height) {
            return Size { width, height };
        }
        if !self.shaped.contains_key(&node) {
            match self.shape(ctx) {
                Some(shaped) => {
                    self.shaped.insert(node, shaped);
                }
                None => {
                    self.missing = true;
                    return Size::ZERO;
                }
            }
        }
        let scale = self.scale;
        let shaped = self.shaped.get_mut(&node).expect("shaped above");

        // `bevy_ui`'s text measure, in physical pixels: clamp the offered
        // width between min- and max-content, then break at it.
        let x = known.width.map(|w| w * scale).unwrap_or_else(|| match available.width {
            AvailableSpace::Definite(w) => (w * scale).max(shaped.info.min.x).min(shaped.info.max.x),
            AvailableSpace::MinContent => shaped.info.min.x,
            AvailableSpace::MaxContent => shaped.info.max.x,
        });
        let size = match known.height {
            Some(h) => Vec2::new(x, h * scale),
            None => match available.width {
                AvailableSpace::Definite(_) => shaped.info.compute_size(
                    TextBounds::new_horizontal(x),
                    &mut shaped.block,
                    &mut self.shaper.font_cx,
                ),
                AvailableSpace::MinContent => Vec2::new(x, shaped.info.min.y),
                AvailableSpace::MaxContent => Vec2::new(x, shaped.info.max.y),
            },
        };
        // Whole LOGICAL pixels, rounded up. Taffy rounds the final layout
        // (`round(x + w) - round(x)`), and a fractional width — 347 physical
        // px is 173.5 logical — can come back a pixel short depending on
        // where the box starts. Bevy then gets bounds narrower than the line
        // and pushes its last word down: `pub struct Subscriber {` drew its
        // `{` on a line of its own at every pane size. An integer width is
        // preserved exactly by that rounding, and never cuts into the text.
        let size = (size / scale).ceil();
        Size {
            width: size.x,
            height: size.y,
        }
    }
}
