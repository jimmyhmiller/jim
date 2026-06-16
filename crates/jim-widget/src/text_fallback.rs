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

use bevy::prelude::*;
use bevy::text::TextSpan;
use jim_style::FontRegistry;

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
    reg: Option<Res<FontRegistry>>,
    q: Query<
        (Entity, &Text2d, &TextFont, &TextColor, Option<&Children>),
        (Added<Text2d>, Without<CanvasManagedText>),
    >,
) {
    let Some(reg) = reg else {
        return;
    };
    for (entity, text, font, color, children) in &q {
        // Leave anything that's already rich text (has child spans) alone.
        if children.is_some() {
            continue;
        }
        let runs = reg.split_runs(&font.font, &text.0);
        if runs.is_empty() {
            continue;
        }
        // Nothing to do ONLY when the whole string already draws in the base
        // font. A single run whose font differs from the base — e.g. a label
        // that is entirely symbols like "↻" — still has to be re-inserted, or
        // it keeps the base font that can't draw it and renders as tofu. (This
        // was the bug: `runs.len() <= 1` skipped that all-fallback case.)
        if runs.len() == 1 && runs[0].1 == font.font {
            continue;
        }
        let size = font.font_size;
        let col = color.0;
        // Root keeps the first run; re-inserting Text2d is a *change*, not
        // an add, so this never re-triggers the `Added` filter.
        commands.entity(entity).insert((
            Text2d::new(runs[0].0.clone()),
            TextFont {
                font: runs[0].1.clone(),
                font_size: size,
                ..default()
            },
        ));
        for (s, f) in runs.iter().skip(1) {
            commands.spawn((
                ChildOf(entity),
                TextSpan::new(s.clone()),
                TextFont {
                    font: f.clone(),
                    font_size: size,
                    ..default()
                },
                TextColor(col),
            ));
        }
    }
}
