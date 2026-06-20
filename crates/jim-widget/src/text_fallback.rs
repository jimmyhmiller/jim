//! Per-glyph font fallback for `Text2d`.
//!
//! Bevy renders a `Text2d` with a single pinned `TextFont` handle and
//! does NO glyph fallback: any codepoint missing from that font silently
//! renders as nothing. Our bundled families (JetBrains Mono / Inter /
//! Crimson Pro) don't cover much of the geometric-shapes / arrows / math
//! blocks, so symbols like `⇄ ▦ ▮` just vanished.
//!
//! This module watches freshly-spawned `Text2d` entities and, when a
//! string contains glyphs the chosen font can't draw, splits it into
//! child `bevy::text::TextSpan`s so the missing glyphs come from the
//! broad-coverage fallback font (DejaVu Sans, registered as `"symbols"`)
//! while the rest stays in the original family. The common case — every
//! glyph covered — is a single run and costs nothing.
//!
//! Note: render.rs has its OWN `crate::TextSpan` (a selectable-text
//! span), unrelated to Bevy's `TextSpan` used here — hence this lives in
//! its own module with explicit imports.

use std::sync::Arc;

use bevy::prelude::*;
use bevy::text::TextSpan;
use jim_style::FontRegistry;

use crate::system_font::SystemFontSource;

/// Marks canvas text that's managed by the in-place reconcile diff
/// (`script_widget::diff_render`). This global splitter skips these — not
/// because canvas text lacks fallback, but because the reconcile does its
/// OWN per-glyph fallback inline (it owns the child spans, so they can't
/// be orphaned by the per-frame root rewrite the way this splitter's
/// would be). Net result: canvas labels render every codepoint just like
/// flow text — authors can use any glyph, no "covered glyphs only" rule.
#[derive(Component)]
pub struct CanvasManagedText;

/// Split any newly-spawned `Text2d` whose font lacks some glyph into
/// fallback runs. Runs in `PostUpdate` before text layout so the spans
/// exist the same frame the text is laid out.
pub fn apply_text_fallback(
    mut commands: Commands,
    reg: Option<ResMut<FontRegistry>>,
    mut fonts: ResMut<Assets<Font>>,
    mut sysfont: NonSendMut<SystemFontSource>,
    q: Query<
        (Entity, &Text2d, &TextFont, &TextColor, Option<&Children>),
        (Added<Text2d>, Without<CanvasManagedText>),
    >,
) {
    let Some(mut reg) = reg else {
        return;
    };
    for (entity, text, font, color, children) in &q {
        // Leave anything that's already rich text (has child spans) alone.
        if children.is_some() {
            continue;
        }
        // Bevy 0.19: TextFont.font is a `FontSource`. The fallback registry is
        // keyed by concrete `Handle<Font>`; widget text always uses one, so a
        // generic-family source just bypasses the per-glyph cascade.
        let FontSource::Handle(base_font) = &font.font else {
            continue;
        };
        // For any codepoint no registered font can draw, ask the OS for a font
        // that has it and load it. Once a system font is registered its whole
        // coverage is known, so a single lookup serves every glyph it covers —
        // `has_glyph` dedups subsequent chars. This is what lets widget text
        // render arbitrary Unicode instead of tofu.
        for ch in text.0.chars() {
            if reg.has_glyph(base_font, ch) {
                continue;
            }
            if let Some(hit) = sysfont.bytes_for(ch) {
                // Register each system font with Bevy at most ONCE per file
                // (the first codepoint that loads it). A font's recorded
                // coverage is its face-0 cmap, which can omit a glyph that
                // lives in another face of a `.ttc` — so `has_glyph` above
                // would never start returning true for such a glyph, and
                // without this guard every repaint re-`add`ed the same font,
                // leaking a fresh multi-MB asset each frame (the 15GB leak).
                if !hit.newly_loaded {
                    continue;
                }
                // Only plain single-face outline fonts are safe — color/bitmap
                // (emoji) fonts and `.ttc` collections make Bevy's rasterizer
                // PANIC. Anything else falls through to the `�` replacement
                // instead of crashing the app.
                if !jim_style::fonts::is_safe_fallback_font(hit.bytes) {
                    continue;
                }
                // Bevy 0.19: Font::from_bytes is infallible; the safe-font
                // guard above already screened out color/.ttc fonts.
                let f = Font::from_bytes(hit.bytes.to_vec());
                let cov = Arc::new(jim_style::fonts::coverage_of(hit.bytes));
                let handle = fonts.add(f);
                reg.register_system_font(handle, cov);
            }
        }
        let runs = reg.split_runs(base_font, &text.0);
        if runs.is_empty() {
            continue;
        }
        // Nothing to do ONLY when the whole string already draws in the base
        // font. A single run whose font differs from the base — e.g. a label
        // that is entirely symbols like "↻" — still has to be re-inserted, or
        // it keeps the base font that can't draw it and renders as tofu. (This
        // was the bug: `runs.len() <= 1` skipped that all-fallback case.)
        if runs.len() == 1 && *base_font == runs[0].1 {
            continue;
        }
        let size = font.font_size;
        let col = color.0;
        // Root keeps the first run; re-inserting Text2d is a *change*, not
        // an add, so this never re-triggers the `Added` filter.
        commands.entity(entity).insert((
            Text2d::new(runs[0].0.clone()),
            TextFont {
                font: (runs[0].1.clone()).into(),
                font_size: size,
                ..default()
            },
        ));
        for (s, f) in runs.iter().skip(1) {
            commands.spawn((
                ChildOf(entity),
                TextSpan::new(s.clone()),
                TextFont {
                    font: (f.clone()).into(),
                    font_size: size,
                    ..default()
                },
                TextColor(col),
            ));
        }
    }
}
