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
    /// System fonts loaded on demand (macOS CoreText cascade) to cover
    /// codepoints absent from every bundled family. Consulted in order
    /// after `symbols`. Populated by the widget text-fallback system as it
    /// encounters glyphs nothing bundled can draw — so widgets render any
    /// Unicode the OS has a font for, instead of tofu.
    system: Vec<Handle<Font>>,
    /// Bundled COLOR emoji font (Twemoji, COLRv0). Color glyphs render via
    /// cosmic-text's `SwashContent::Color` path. We bundle a COLR font on
    /// purpose: macOS's own Apple Color Emoji is `sbix` (bitmap) in a `.ttc`,
    /// which crashes Bevy's rasterizer — COLR scales cleanly.
    emoji: Handle<Font>,
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

    /// Resolve `ch` to the actual `(glyph, font)` to draw. Like
    /// [`font_for_char`], but when NEITHER the base family nor the broad
    /// `symbols` font has the codepoint, it substitutes the Unicode
    /// replacement character `�` (U+FFFD, which DejaVu Sans has) so an
    /// unsupported glyph renders a VISIBLE box instead of silently
    /// vanishing or showing inconsistent tofu. That's the guarantee: text
    /// never disappears just because a font lacks a glyph.
    fn resolve_glyph(&self, base: &Handle<Font>, ch: char) -> (char, Handle<Font>) {
        if self.covers(base, ch) {
            return (ch, base.clone());
        }
        if self.symbols != Handle::default() && self.covers(&self.symbols, ch) {
            return (ch, self.symbols.clone());
        }
        // Bundled COLOR emoji font (covers the emoji codepoints).
        if self.emoji != Handle::default() && self.covers(&self.emoji, ch) {
            return (ch, self.emoji.clone());
        }
        // On-demand system fonts (CoreText cascade) loaded by the widget text
        // path. The OS indexes every installed font's coverage, so this is what
        // makes arbitrary Unicode actually render.
        for h in &self.system {
            if self.covers(h, ch) {
                return (ch, h.clone());
            }
        }
        if self.symbols != Handle::default() && self.covers(&self.symbols, '\u{FFFD}') {
            return ('\u{FFFD}', self.symbols.clone());
        }
        (ch, base.clone())
    }

    /// Does ANY currently-registered font (the requested `base`, the broad
    /// `symbols` fallback, or a loaded system font) cover `ch`? Drives the
    /// widget text path's decision to lazily load a system font.
    pub fn has_glyph(&self, base: &Handle<Font>, ch: char) -> bool {
        self.covers(base, ch)
            || (self.symbols != Handle::default() && self.covers(&self.symbols, ch))
            || (self.emoji != Handle::default() && self.covers(&self.emoji, ch))
            || self.system.iter().any(|h| self.covers(h, ch))
    }

    /// Register a system fallback font (loaded on demand) with its coverage so
    /// `resolve_glyph`/`split_runs` route matching codepoints to it.
    pub fn register_system_font(&mut self, handle: Handle<Font>, coverage: Arc<HashSet<u32>>) {
        self.coverage.insert(handle.clone(), coverage);
        self.system.push(handle);
    }

    /// Split `text` into maximal runs that share one font, applying
    /// per-glyph fallback. Returns `[(run_text, font)]`. The overwhelmingly
    /// common case — every glyph covered by `base` — yields a single run,
    /// so callers can cheaply special-case `len == 1` (no rich text).
    pub fn split_runs(&self, base: &Handle<Font>, text: &str) -> Vec<(String, Handle<Font>)> {
        let mut runs: Vec<(String, Handle<Font>)> = Vec::new();
        for ch in text.chars() {
            let (glyph, font) = self.resolve_glyph(base, ch);
            match runs.last_mut() {
                Some((s, f)) if *f == font => s.push(glyph),
                _ => runs.push((glyph.to_string(), font)),
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
    // COLOR emoji (Twemoji, COLRv0). Per-glyph fallback for emoji codepoints.
    ("emoji", include_bytes!("../assets/fonts/TwemojiMozilla.ttf")),
];

/// The family name whose handle becomes the per-glyph symbol fallback.
const FALLBACK_FAMILY: &str = "symbols";
/// The family name whose handle becomes the per-glyph emoji fallback.
const EMOJI_FAMILY: &str = "emoji";

/// Is `bytes` a font we can SAFELY hand to Bevy's `Text2d` as a system
/// fallback? Bevy's rasterizer (cosmic-text + swash) PANICS — taking the whole
/// app down — on glyphs it can't scale, namely:
///   * color/bitmap (emoji) fonts: `sbix` / `COLR` / `CBDT` / `CBLC` / `SVG `;
///   * TrueType Collections (`.ttc`): `Font::try_from_bytes` and our cmap
///     coverage can pick different faces, so a glyph id we route here may not
///     exist in the face cosmic-text loaded → "failed to get scaled glyph
///     image" panic. (Apple ships CJK *and* emoji as `.ttc`.)
/// We only accept a single-face font with real outlines (`glyf` or `CFF`).
/// Anything else falls through to the `�` replacement — visible, never a crash.
pub fn is_safe_fallback_font(bytes: &[u8]) -> bool {
    // A `.ttc` collection has several faces; check EVERY one. If any face is a
    // color/bitmap font, reject the whole file (that's how Apple Color Emoji
    // ships). Non-color collections (Apple's CJK fonts) are fine — cosmic-text
    // loads them consistently, so CJK renders.
    let n = ttf_parser::fonts_in_collection(bytes).unwrap_or(1).max(1);
    let mut any_outline = false;
    for i in 0..n {
        let Ok(face) = ttf_parser::Face::parse(bytes, i) else {
            return false;
        };
        let raw = face.raw_face();
        let has = |t: &[u8; 4]| raw.table(ttf_parser::Tag::from_bytes(t)).is_some();
        if has(b"sbix") || has(b"COLR") || has(b"CBDT") || has(b"CBLC") || has(b"SVG ") {
            return false;
        }
        if has(b"glyf") || has(b"CFF ") || has(b"CFF2") {
            any_outline = true;
        }
    }
    any_outline
}

/// Parse a font's cmap into the set of Unicode codepoints it can render.
pub fn coverage_of(bytes: &[u8]) -> HashSet<u32> {
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
        if *name == EMOJI_FAMILY {
            registry.emoji = handle.clone();
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
