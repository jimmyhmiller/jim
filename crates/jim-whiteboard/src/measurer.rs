//! Text measurement for the whiteboard editor.
//!
//! The headless `whiteboard-core` never owns fonts, so it asks an injected
//! [`TextMeasurer`] for line widths/heights to auto-size text boxes. We render
//! text with a monospace chrome font (the same family the panes use), so a
//! per-family advance ratio is an accurate enough estimate for box sizing —
//! exact glyph layout happens later in Bevy's `Text2d`.

use whiteboard_core::text::{FontFamily, FontSpec, TextMeasurer, TextMetrics};

/// Deterministic measurer tuned to the fonts we actually render with.
#[derive(Debug, Clone, Copy)]
pub struct WbMeasurer;

impl WbMeasurer {
    fn advance_ratio(family: &FontFamily) -> f64 {
        match family {
            // Cascadia/monospace cell is ~0.6em wide.
            FontFamily::Code => 0.6,
            // Hand-drawn / sans are narrower on average.
            FontFamily::HandDrawn | FontFamily::Normal | FontFamily::Custom(_) => 0.55,
        }
    }
}

impl TextMeasurer for WbMeasurer {
    fn measure(&self, text: &str, font: &FontSpec) -> TextMetrics {
        let chars = text.chars().count() as f64;
        TextMetrics {
            width: chars * font.size * Self::advance_ratio(&font.family),
            ascent: font.size * 0.8,
            descent: font.size * 0.2,
        }
    }
}
