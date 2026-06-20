# Bevy 0.18 → 0.19 migration

Worktree `bevy-0.19` (branch `bevy-0.19`). Whole workspace builds (debug +
release) and the `jim` app boots on 0.19 (Metal renderer, canvas/panes/cube,
Glaze shaders, widget auto-discovery all verified via an isolated smoke run).

## What changed

### Versions / deps
- Every `bevy = "0.18"` → `"0.19"` (11 crates).
- `jim-style`: `naga = "27"` → `"29"`. 0.19's wgpu ships **naga 29**; keeping
  our own naga 27 left two naga versions in the tree, and feature-unification
  dropped codespan-reporting's `termcolor`, breaking naga 27's error path
  (`String: WriteColor`). Aligning to 29 removes the duplicate.
- `jim-widget`: added taffy feature **`taffy_tree`** (0.9.2 split `TaffyTree`
  out of default features) and `use taffy::TaffyTree` (no longer in the
  prelude).
- **`system_font_discovery`** enabled on every text-rendering crate — this is
  0.19's `parley/system` and the replacement for cosmic-text's
  `load_system_fonts()`. Without it, `FontSource::Monospace`/`SansSerif`
  generic families log "Text may not render" and parley does no system
  fallback.

### Text / font API (the bulk — parley replaced cosmic-text)
- `TextFont.font`: `Handle<Font>` → `FontSource` (`From<Handle<Font>>`, so
  `handle.into()`; reflexive on real-Handle fields).
- `TextFont.font_size`: `f32` → `FontSize` (`FontSize::Px(..)`).
- `Font::try_from_bytes(..)->Result` → `Font::from_bytes(..)->Font`
  (**infallible** now; parley parses lazily). The old parse-failure guards in
  the font registry / fallback are gone; bad-font screening relies on the
  cmap-coverage pass + `is_safe_fallback_font`.
- `TextLayout::new_with_no_wrap()` → `no_wrap()`, `new_with_justify` →
  `justify`. `JustifyText` → `Justify`.
- `EventReader`/`EventWriter` → `MessageReader`/`MessageWriter`.
- `Assets::get_mut` returns an `AssetMut` guard → `if let Some(mut x)` /
  `let Some(mut x)` bindings (mutation through it triggers `Modified` only on
  real change).
- `CosmicFontSystem` removed → `FontCx` (wraps `parley::FontContext`). The
  editor's `load_fallback_fonts` startup pass is deleted; system fallback is
  now automatic (see `system_font_discovery`).

### markdown.rs glyph readback (`crates/jim-editor/src/markdown.rs`)
0.19's `PositionedGlyph` dropped `byte_index` / `byte_length` / `size` and
renamed `span_index` → `section_index`. The WYSIWYG caret/click mapping was
rebuilt: source-char ranges are reconstructed by counting glyphs in logical
order within each rendered span, and horizontal extents come from neighbouring
glyph **centers** on a row (`position` is the glyph quad center =
`atlas.size/2 + pen + offset`), with the atlas rect giving a real width at row
ends. **Known limitation:** assumes one glyph per source char — correct for
the editor's LTR mono/Inter text, but ligature clusters can drift the caret by
a fraction of a cell. Worth runtime calibration; if it drifts, the next step is
to drive it off parley cluster boundaries instead.

## Runtime notes / things to watch
- **file_watcher panic under `/tmp`:** 0.19's `bevy_asset` file_watcher
  panics on `strip_prefix` when the watched abs-path doesn't start with root.
  On macOS `/tmp`→`/private/tmp` symlinking triggers it *only* when `HOME` is
  under `/tmp` (i.e. the isolated smoke test). Under the real `HOME`
  (`/Users/...`, no symlink) it does not fire. Latent fragility if `~/.jim`
  ever lives on a symlinked path.
- Editor system-font fallback for exotic glyphs now flows through
  parley/fontique (via `system_font_discovery`) rather than the old explicit
  cosmic-text load. The widget CoreText cascade (`jim-widget/system_font.rs`)
  is unaffected — it loads concrete font bytes itself.

## Feature-widget exploration (0.19)

Shipped one working demo of the flagship feature; the rest are ranked
candidates.

- **DONE — native `EditableText`** (`crates/jim-widget/src/bin/editable_text_demo.rs`,
  `cargo run --bin editable_text_demo`). 0.19's `EditableText` is a headless
  parley `PlainEditor` (Unicode-correct cursor motion, grapheme delete, word
  motion, selection, IME/clipboard hooks) that produces layout but renders
  nothing itself — the host draws it. The demo feeds `TextEdit`s from raw
  `KeyboardInput` and mirrors `editor.text()` into a `Text2d`. This is the
  smallest faithful surface for a real jim integration: jim already owns
  keyboard routing (`compute_keyboard_owner`) and already consumes
  `TextLayoutInfo` (markdown readback), so a native-editing pane = own the
  routing → queue `TextEdit`s → render the layout. Highest-value follow-up:
  back the funct widget `Input`/`TextArea` elements with `EditableText`.
- **Post-processing pane effects** (`bevy_post_process` feature): vignette +
  lens distortion components on a camera. Fits the Glaze styling ethos; a
  per-pane "focus dim" or CRT-ish terminal look. Self-contained (add feature,
  attach component).
- **Text gizmos** (`Gizmos::text` / `text_2d`): world-space debug labels —
  cheap win for the profiler/trace/churn overlays.
- **`bevy_feathers` / `bevy_ui_widgets`** (stabilized): dropdowns, number
  inputs, scrollbars, list view. Could back parts of the retained-UI widget
  vocabulary, but overlaps a lot with what `jim-widget` already does.
- **`bsn!` scene macro**: ergonomic spawning; nice-to-have, not a widget.
