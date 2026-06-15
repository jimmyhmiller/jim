//! Font registry — name → `Handle<Font>` resolution.
//!
//! Theme tokens like [`crate::tokens::FONT_FAMILY_HEADING`] hold a
//! family *name* (`"serif"`, `"sans"`, `"mono"`). At render time the
//! widget asks the registry for the handle.
//!
//! ## Adding a family
//!
//! Drop a `.ttf` or `.otf` into `crates/style-bevy/assets/fonts/` and
//! add a line to [`bundled_fonts`]. Today only JetBrains Mono is
//! bundled; `"serif"` and `"sans"` both fall back to it until you
//! drop in real files. Unknown names also fall back to mono — the
//! engine never crashes for a missing font, it just renders in mono.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bevy::prelude::*;

/// Maps a family name to a `Handle<Font>`. Populated at Startup.
#[derive(Resource, Default, Clone)]
pub struct FontRegistry {
    by_name: HashMap<String, Handle<Font>>,
    /// Fallback used when a *name* doesn't resolve. Always points at the
    /// bundled mono font.
    fallback: Handle<Font>,
    /// Per-handle set of covered Unicode codepoints, parsed from each
    /// font's cmap at load. Drives per-glyph font fallback so a glyph
    /// missing from the chosen family is still drawn (from `symbols`)
    /// instead of silently vanishing. `Arc` so cloning the registry
    /// (it's a `Resource` that callers clone into `LayoutCtx`) is cheap.
    coverage: HashMap<Handle<Font>, Arc<HashSet<u32>>>,
    /// Broad-coverage fallback font (DejaVu Sans) consulted per-glyph
    /// when the requested family lacks a codepoint.
    symbols: Handle<Font>,
}

impl FontRegistry {
    /// Look up a family by name. Returns the fallback (mono) when the
    /// name isn't registered — callers never see `None`.
    pub fn resolve(&self, name: &str) -> Handle<Font> {
        self.by_name
            .get(name)
            .cloned()
            .unwrap_or_else(|| self.fallback.clone())
    }

    /// Resolve one of the three role tokens. Equivalent to
    /// `resolve(theme.str_value(role))`.
    pub fn for_role(&self, theme: &crate::Theme, role: crate::TokenId) -> Handle<Font> {
        self.resolve(theme.str_value(role))
    }

    /// Raw bytes for a bundled family. Used by callers that need to
    /// measure glyph advances (skrifa, etc.) and can't go through the
    /// `Handle<Font>`. Returns `None` for unknown names.
    pub fn bytes(&self, name: &str) -> Option<&'static [u8]> {
        BUNDLED_FONTS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| *b)
    }

    /// All registered family names. Used by the theme editor's font
    /// picker. Sorted for stable display order.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_name.keys().cloned().collect();
        v.sort();
        v
    }

    /// Does font `handle` cover codepoint `ch`? Unknown handles (not
    /// bundled) are assumed to cover everything, so we never *hide* text
    /// just because we couldn't introspect its font.
    fn covers(&self, handle: &Handle<Font>, ch: char) -> bool {
        match self.coverage.get(handle) {
            Some(set) => set.contains(&(ch as u32)),
            None => true,
        }
    }

    /// Pick the font to draw `ch` in: the requested `base` if it covers
    /// the glyph, else the broad `symbols` fallback if *it* does, else
    /// `base` (let it render tofu rather than swap unexpectedly).
    pub fn font_for_char(&self, base: &Handle<Font>, ch: char) -> Handle<Font> {
        if self.covers(base, ch) {
            base.clone()
        } else if self.symbols != Handle::default() && self.covers(&self.symbols, ch) {
            self.symbols.clone()
        } else {
            base.clone()
        }
    }

    /// Split `text` into maximal runs that share one font, applying
    /// per-glyph fallback. Returns `[(run_text, font)]`. The overwhelmingly
    /// common case — every glyph covered by `base` — yields a single run,
    /// so callers can cheaply special-case `len == 1` (no rich text).
    pub fn split_runs(&self, base: &Handle<Font>, text: &str) -> Vec<(String, Handle<Font>)> {
        let mut runs: Vec<(String, Handle<Font>)> = Vec::new();
        for ch in text.chars() {
            let font = self.font_for_char(base, ch);
            match runs.last_mut() {
                Some((s, f)) if *f == font => s.push(ch),
                _ => runs.push((ch.to_string(), font)),
            }
        }
        runs
    }
}

/// Per-family bundled bytes. New families add a row here.
//
// The three bundled families correspond to the FONT_FAMILY_HEADING /
// BODY / MONO theme tokens. Variable-font files: weight + optical-size
// axes are baked in; the cosmic-text shaper picks a default instance
// for now. To add an additional family: drop a `.ttf` next to these
// and add a row with `include_bytes!`.
const BUNDLED_FONTS: &[(&str, &[u8])] = &[
    ("mono", include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")),
    ("sans", include_bytes!("../assets/fonts/Inter-VF.ttf")),
    ("serif", include_bytes!("../assets/fonts/CrimsonPro-VF.ttf")),
    // Broad-coverage glyph fallback (geometric shapes, arrows, math,
    // symbols) consulted per-glyph when the chosen family lacks a
    // codepoint. Also selectable as a family by name.
    ("symbols", include_bytes!("../assets/fonts/DejaVuSans.ttf")),
];

/// The family name whose handle becomes the per-glyph fallback font.
const FALLBACK_FAMILY: &str = "symbols";

/// Parse a font's cmap into the set of Unicode codepoints it can render.
fn coverage_of(bytes: &[u8]) -> HashSet<u32> {
    let mut set = HashSet::new();
    if let Ok(face) = ttf_parser::Face::parse(bytes, 0) {
        if let Some(cmap) = face.tables().cmap {
            for sub in cmap.subtables {
                if sub.is_unicode() {
                    sub.codepoints(|cp| {
                        set.insert(cp);
                    });
                }
            }
        }
    }
    set
}

pub struct FontRegistryPlugin;

impl Plugin for FontRegistryPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FontRegistry>()
            .add_systems(Startup, register_bundled_fonts);
    }
}

/// Idempotently populate the registry with bundled fonts. Safe to call
/// from multiple Startup systems — entries that already exist are kept,
/// new ones are appended. The first registered family becomes the
/// fallback if no fallback is set yet.
pub fn ensure_initialized(registry: &mut FontRegistry, fonts: &mut Assets<Font>) {
    let mut first: Option<Handle<Font>> = None;
    for (name, bytes) in BUNDLED_FONTS {
        if registry.by_name.contains_key(*name) {
            if first.is_none() {
                first = registry.by_name.get(*name).cloned();
            }
            continue;
        }
        let font = match Font::try_from_bytes(bytes.to_vec()) {
            Ok(f) => f,
            Err(e) => {
                warn!("[font-registry] bundled font {:?} failed to parse: {}", name, e);
                continue;
            }
        };
        let handle = fonts.add(font);
        if first.is_none() {
            first = Some(handle.clone());
        }
        // Record glyph coverage for per-glyph fallback, and remember the
        // dedicated fallback family's handle.
        registry
            .coverage
            .insert(handle.clone(), Arc::new(coverage_of(bytes)));
        if *name == FALLBACK_FAMILY {
            registry.symbols = handle.clone();
        }
        registry.by_name.insert((*name).to_string(), handle);
    }
    if let Some(h) = first {
        // Don't clobber an explicitly-set fallback if one already exists.
        if registry.fallback == Handle::default() {
            registry.fallback = h;
        }
    }
}

fn register_bundled_fonts(
    mut registry: ResMut<FontRegistry>,
    mut fonts: ResMut<Assets<Font>>,
) {
    ensure_initialized(&mut registry, &mut fonts);
}
