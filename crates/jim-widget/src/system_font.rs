//! System-font cascade for widget text.
//!
//! Bevy's `Text2d` draws each run in exactly one font and does no fallback,
//! and our four bundled families (JetBrains Mono / Inter / Crimson Pro /
//! DejaVu Sans) don't cover every codepoint. Rather than maintain a list of
//! "safe" glyphs forever, we ask the OS — exactly like real terminals and like
//! `jim-terminal`'s atlas do — "which installed font has this codepoint?",
//! load that font's bytes, and register it with the [`jim_style::FontRegistry`]
//! so the per-glyph splitter routes the glyph to it. Net effect: widget text
//! renders ANY Unicode the system has a font for.
//!
//! On non-macOS this is a stub (`None`); add fontconfig / DirectWrite paths
//! when those platforms ship.

use std::collections::HashMap;
use std::path::PathBuf;

/// A system-font lookup hit. `newly_loaded` is true exactly once per unique
/// font *file* over a session — the first codepoint that pulls a given font
/// off disk. Callers use it to register the font with Bevy's `Assets<Font>`
/// at most once: re-`add`ing the same bytes mints a fresh multi-MB `Font`
/// asset every time, and since a font's recorded coverage (face-0 cmap only)
/// can fail to include a glyph that actually lives in another face of a
/// `.ttc` collection, the "already registered" guard would never trip — so
/// without this flag every repaint of such a glyph leaked another font. That
/// was a 15GB-and-climbing leak.
#[derive(Clone, Copy)]
pub struct SystemFontHit {
    pub bytes: &'static [u8],
    pub newly_loaded: bool,
}

/// Per-codepoint → system-font-bytes resolver. Held as a `NonSend` resource
/// because the CoreText handle isn't `Send`. Caches by codepoint and by font
/// path so each unique glyph costs at most one OS query and each font loads
/// once (leaked to `'static`, matching the terminal atlas — a fixed, tiny set
/// of system fonts over a session).
pub struct SystemFontSource {
    char_cache: HashMap<char, Option<&'static [u8]>>,
    path_cache: HashMap<PathBuf, &'static [u8]>,
    #[cfg(target_os = "macos")]
    cascade_base: Option<core_text::font::CTFont>,
}

impl Default for SystemFontSource {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemFontSource {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        let cascade_base = core_text::font::new_from_name("Menlo", 14.0).ok();
        Self {
            char_cache: HashMap::new(),
            path_cache: HashMap::new(),
            #[cfg(target_os = "macos")]
            cascade_base,
        }
    }

    /// A system font that covers `ch`, or `None` if the OS has none (or we're
    /// not on macOS). Cached per codepoint; a cached hit is never
    /// `newly_loaded` (the file was pulled off disk on its first codepoint).
    pub fn bytes_for(&mut self, ch: char) -> Option<SystemFontHit> {
        if let Some(&cached) = self.char_cache.get(&ch) {
            return cached.map(|bytes| SystemFontHit { bytes, newly_loaded: false });
        }
        let (result, newly_loaded) = self.lookup(ch);
        self.char_cache.insert(ch, result);
        result.map(|bytes| SystemFontHit { bytes, newly_loaded })
    }

    /// Returns `(bytes, newly_loaded)`. `newly_loaded` is true only when this
    /// call read the font file for the first time this session.
    #[cfg(target_os = "macos")]
    fn lookup(&mut self, ch: char) -> (Option<&'static [u8]>, bool) {
        use core_foundation::base::{CFRange, TCFType};
        use core_foundation::string::{CFString, CFStringRef};
        use core_text::font::{CTFont, CTFontRef};

        // CTFontCreateForString isn't bound by the core-text crate; declare it.
        unsafe extern "C" {
            fn CTFontCreateForString(
                currentFont: CTFontRef,
                string: CFStringRef,
                range: CFRange,
            ) -> CTFontRef;
        }

        let Some(base) = self.cascade_base.as_ref() else {
            return (None, false);
        };
        let s_buf = ch.to_string();
        let cfs = CFString::new(&s_buf);
        let utf16_len = s_buf.encode_utf16().count() as isize;
        let range = CFRange { location: 0, length: utf16_len };

        let fallback = unsafe {
            let r = CTFontCreateForString(
                base.as_concrete_TypeRef(),
                cfs.as_concrete_TypeRef(),
                range,
            );
            if r.is_null() {
                return (None, false);
            }
            CTFont::wrap_under_create_rule(r)
        };

        let Some(url) = fallback.url() else {
            return (None, false);
        };
        let Some(path) = url.to_path() else {
            return (None, false);
        };
        if let Some(&bytes) = self.path_cache.get(&path) {
            return (Some(bytes), false);
        }
        let Ok(bytes) = std::fs::read(&path) else {
            return (None, false);
        };
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        self.path_cache.insert(path, leaked);
        (Some(leaked), true)
    }

    #[cfg(not(target_os = "macos"))]
    fn lookup(&mut self, _ch: char) -> (Option<&'static [u8]>, bool) {
        (None, false)
    }
}
