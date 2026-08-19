# Present — markdown decks that can reveal live dashboards

> Architecture + implementation. **Stages 1–6 are built.**

The ask:

1. **Markdown is the source** for a slide deck.
2. **Glaze styles the slides** — our style language, not inline `Style` maps.
3. **Some slides reveal a live project dashboard** — a real terminal, a real
   `repo_hub`, a real diff pane — because the point is *demoing*.
4. **Fullscreen** presentation.

## The model

**A deck is just a widget.** `deck.ft` is one funct widget pane. It reads
the markdown, splits it into slides, renders the current one, and handles
next/prev. Nothing about it is special to the pane system.

**A demo slide names a PROJECT, and that project's whole canvas shows up
live inside the slide.** A slide says `project: Recursion` and Recursion's
actual panes — real terminals with real shells, widgets with their real
data — appear in a region of the slide, laid out as they are on that
project's canvas. Nothing is spawned, nothing re-fetches; the panes keep
running while they're shown.

The unit is the **project**, not a hand-assembled set of panes. You
already arrange your work per project; a demo slide should just point at
one.

> A finer-grained `dashboard: <group>` also exists (named pane groups,
> stage 4) for revealing a hand-picked subset. It works, but it is not the
> primary path — naming a project is. See `pane_groups` vs
> `project_region`.

That is the whole design. Everything below is what it costs.

---

## Part 1 — What already exists (the ~70%)

| Capability | Where | State |
|---|---|---|
| Markdown → `Element` tree, in funct | `jim-widget/widgets/markdown.ft` | Works. Headings, wrapped paragraphs, fenced code, quotes, nested lists, pipe tables, HRs. **Strips inline emphasis** (gap C). Fork it as the deck's front end. |
| Markdown → styled runs, in Rust | `crates/markdown-core` | Proper `Block`/`RenderLine`/`Run` + `InlineStyle`, exact source-offset coverage. **Not reachable from funct.** |
| Element vocabulary | `jim-widget/src/protocol.rs` | `Text` (size/weight/family/wrap), `Frame`, `Vstack`/`Hstack` (Taffy flex), `Canvas` (nested, absolute-positioned sprites/rects/text), `Table`, `Bar`, `Scroll`, `Editor` portal. |
| Glaze compiler | `crates/glaze` | Tokens (3-tier), `fn` defs, variant params, `:state` plans, `when` breakpoints, `Transition`/`Easing`, **and shader layers → WGSL** (`shader.rs`; the "Stage 3 unimplemented" note in `eval.rs` is stale). `Program::resolve(name, variant, states) -> CompiledStyle`. |
| Glaze → renderer | `jim-widget/src/glaze_style.rs`, `glaze_material.rs` | `to_style(&CompiledStyle) -> protocol::Style` + per-slot resolvers. `Style::glaze_layers` carries compiled WGSL over the wire; `render.rs` instantiates the materials. |
| Pane visibility gating | `jim-app/src/projects.rs::sync_visibility` | One system decides every pane's `Visibility` from `(project, canvas level)`. **The single seam a named group hooks into.** |
| Show/hide tween | `jim-app/src/expose.rs` | `PaneVisualScale` (scale, don't resize — a terminal keeps its layout and never re-wraps), ease-out cubic with stagger, input suppression, pan/zoom lock, persistence guard. The model for a dashboard reveal animation. |
| Multi-pane layout | `jim-pane/src/dock.rs` | Docks slave member `PaneRect`s to a split tree; members stay independent panes. `DockTemplate` presets + `create_dock` + `jimctl dock`. **This is how a dashboard gets a deliberate layout.** |
| Widget → app commands | `git.ft` exports `jimctl(args)` / `spawn_widget(...)` via `proc_spawn`; or `nc -U ~/.jim/socket` (`style_lab.ft`) | Works, but a subprocess per command — too slow for a slide advance (gap B). |
| Widget bus, GUI is a client | `jim-widget/src/msgbus.rs`, `crates/jim-bus` | `pump_widget_messages` runs per frame with every message in hand. Retained topics. **This is the deck→app channel.** |
| Parameterized widget spawn | `jimctl widget --params JSON`, `IpcRequest::SpawnWidget { params }` | One `.ft`, many instances via the funct global `params`. |
| Bundled fonts | `jim-style/src/fonts.rs` | `mono` JetBrains Mono, `sans` Inter VF, `serif` Crimson Pro VF, plus macOS CoreText cascade. |

## Part 2 — What's missing

### A. Named pane groups — the core new primitive

Nothing today can say "these panes are the `repo-health` dashboard; show
them." Pane visibility has exactly one input: `(project, canvas level)`,
decided in `sync_visibility` (`projects.rs:1991`). The drawer
(`drawer.rs`) is spawn-on-demand, not show/hide. Docks group panes for
*layout*, not visibility.

Proposed, and it is small:

```rust
/// Membership in a named, independently-revealable group of panes.
/// Absent = always visible (normal pane).
#[derive(Component)] pub struct PaneGroup(pub String);

/// Groups currently revealed. Empty = only ungrouped panes show.
#[derive(Resource, Default)] pub struct VisibleGroups(pub HashSet<String>);
```

`sync_visibility` gains one clause: a pane with `PaneGroup(g)` shows only
when `g ∈ VisibleGroups`. Persistence already round-trips arbitrary pane
config through `PaneSnapshot`, so group membership survives restarts and
a deck's dashboards are set up once.

Why not reuse nested canvases (a dashboard = a named child canvas, reveal
= `CanvasNav::descend`)? It's free — the machinery exists and tile panes
already carry names (`canvas_pane.rs:1097`). But descending hides
*everything* on the parent level, including the deck pane itself, and it
hijacks the breadcrumb/Cmd+Up nav the user still wants during a talk.
`PaneGroup` is orthogonal to canvas level, which is what we actually
want: **the deck stays put; a group appears over it.**

Assigning membership: `jimctl group add --name repo-health --title "…"`,
or a `group:` field in the pane's snapshot config, or drag-select on
canvas. The CLI form is enough for v1.

### B. Deck → app command channel

The deck must flip `VisibleGroups` on every advance. The two existing
routes both fork a process per command (`proc_spawn("jimctl", …)`, or
`nc -U ~/.jim/socket`), which is wrong for something bound to an arrow
key during a live demo.

The right seam already exists: the GUI is a bus client and
`pump_widget_messages` sees every message each frame. Add an **app-side
subscriber** so the app itself can act on topics rather than only fanning
them out to widgets:

```funct
emit_retained("deck.groups", { show: ["repo-health"] })
```

Retained matters — it means the deck's current state survives a GUI
restart mid-talk, and a dashboard pane spawned late learns the state.
Latency is one frame, no process spawn.

This subscriber seam is reusable well beyond decks (any widget driving
app-level state), so it's worth building properly rather than as a deck
special case.

### C. No inline rich text

`Element::Text` is a single uniform run. That is why `markdown.ft`
*strips* `**bold**` / `*italic*` / `` `code` `` — its own comment: "the
Element model has no mixed-style runs inside a wrapped paragraph." For
prose slides that's disqualifying.

Fix: **`Element::RichText { runs: [...] }`** with run-level line breaking
in `layout.rs`/`render.rs`, plus a **`md_parse(text)` host fn** over
`markdown-core` so the deck stops reimplementing CommonMark in funct and
inherits the already-correct inline scanner. Both benefit every widget.

### D. Glaze is unreachable from funct

Requirement 2's blocker. Glaze is a **Rust-only** API today —
`glaze::parse` → `Program::resolve` → `glaze_style::to_style`; the
showcase (`bin/glaze_ui.rs`) embeds its sheet as a Rust `const`. The
funct host surface (`funct_widget.rs::register_host_surface`) registers
~60 natives and none touch Glaze.

The bridge is well-shaped:

```funct
glaze_load(src_or_path)          // compile a sheet, held host-side per widget
glaze(name, variant_record)      // -> a Style record, spliced into `style:`
glaze_slot(name, slot, variant)  // -> per-slot style (tabs/bar/table/…)
```

`to_style` already returns `protocol::Style`, which serializes to exactly
the record shape the renderer decodes — so this is a serde hop plus a
`Program` slot per widget. Shader layers ride along in `glaze_layers`
and `render.rs` already handles them. Add `.glz` hot reload (recompile →
re-render, no restart).

### E. Fullscreen

`rg -i fullscreen` over the workspace returns only shader comments;
`WindowMode` is never touched. Needed:

- Set `Window::mode` to `BorderlessFullscreen(MonitorSelection::Current)`.
- **Hazard:** `main.rs` carries a hard-won workaround for the macOS
  lid-close cascade — `Monitor` is the `linked_spawn` target of the
  window's `OnMonitor` relationship, so a despawned monitor takes the
  window down with it; `exit_condition: DontExit` +
  `respawn_primary_window_on_loss` is the mitigation. Fullscreen
  *changes monitor association*, so test against display sleep,
  external-monitor unplug, and Space switches before shipping.
- Chrome hiding is a separate job: sidebar (`projects.rs`), breadcrumb
  (`canvas_pane.rs::render_breadcrumb`), debug bar, drawer — all gated on
  one `Presenting` resource.

### F. Sizing the deck pane — resize, don't zoom

Non-obvious and it shapes the deck's authoring model. Canvas zoom sets
`content_root.scale` (`canvas.rs`), a **transform** scale over glyphs
already rasterized at their base size. Zooming a 1280×720 slide up to a
2560×1440 display gives blurry text.

So presentation must **resize the deck pane to the display** and let the
widget re-lay-out at native size. Consequence: **slides must be authored
resolution-relative, not in fixed px** — Glaze `when` breakpoints and `%`
widths, or the deck derives a scale factor from pane height and multiplies
every `size:` itself. Worth deciding at format-design time, because
retrofitting it means rewriting every deck.

### G. Presenter keys don't reach anything

`script_widget.rs::forward_keys_to_workers` forwards exactly six keys to
a focused widget: `ArrowLeft/Right/Up/Down`, `Home`, `End`. No Space, no
PageUp/PageDown, no Escape — i.e. none of what a presentation remote
sends.

Widen that set, **and** put deck navigation on app-level `Action`s +
`KeyChord`s (`actions.rs`). Otherwise advancing breaks the instant you
click into the terminal you're demoing — which on a dashboard slide is
the entire point.

### H. No image element

No `Element::Image`. Only `Style::background_image` (top-left anchored,
**stretched to fill** — wrong aspect for photos) and `CanvasItem::Sprite`
(explicit `x/y/w/h`). Need `Element::Image { path, fit: contain|cover|fill }`
sized by flex. Small — `WidgetImageCache` and the decode path in
`render.rs` already exist.

### I. Presenter view / second display — defer

The app assumes one primary window and actively respawns it if lost.
Bevy multi-window plus that respawn logic is its own project. Ship
speaker notes as an overlay on the presenting display first.

---

## Part 3 — How it fits together

```
                     ┌─────────────────────────────────────┐
   deck.md  ──────►  │  deck.ft  (one widget pane)         │
   talk.glz ──────►  │   parse → slides[]                  │
                     │   render slide[i] via glaze()       │
                     │   on_key / action → i±1             │
                     └──────────────┬──────────────────────┘
                                    │ emit_retained("deck.groups", {show:[…]})
                                    ▼
                     ┌─────────────────────────────────────┐
                     │  present.rs (app-side subscriber)   │
                     │   VisibleGroups = {…}               │
                     └──────────────┬──────────────────────┘
                                    ▼
                     ┌─────────────────────────────────────┐
                     │  sync_visibility  (one new clause)  │
                     │   PaneGroup("repo-health") → shown  │
                     └─────────────────────────────────────┘
                        repo_hub · diff · terminal  (already alive)
```

On a dashboard slide the deck pane steps aside — either hidden outright,
or shrunk to a corner strip carrying the slide title and progress. Both
are just `PaneRect` writes, tweened with the `expose.rs` easing.

### Deck format strawman

```markdown
---
style: decks/talk.glz
---

# Jim

A canvas of live panes.

::: notes
Open cold — don't explain the architecture yet.
:::

---
dashboard: repo-health
chrome: corner        # deck shrinks to a corner strip instead of hiding
---

# Where the work is
```

Front matter names the Glaze sheet. `---` splits slides. `dashboard:`
names a `PaneGroup`. Per-slide front-matter keys map onto Glaze variant
params, so the sheet stays the single source of visual truth.

---

## Part 4 — Staged plan

Each stage stands alone.

1. ✅ **Glaze from funct** — `glaze_load` / `glaze_load_file` / `glaze` /
   `glaze_at` / `glaze_slot` / `glaze_token` / `glaze_styles` /
   `glaze_tokens`, plus `.glz` hot reload (the sheet path lives in
   `WorkerSlots` so `poll_watcher` reloads the widget when it's saved).
   Shader layers came through free — `to_style` already lowers
   `Layer::Shader`. *Every widget benefits, decks or not.*
2. ✅ **`Element::RichText` + `md_parse` + `Element::Image`.** Runs wrap as
   one block via Bevy `TextSpan` children; `tokenize_runs` closes words on
   whitespace rather than run boundaries so `un**bold**ed` stays one word.
   `md_parse` exposes `markdown_core`. `Image` fits contain/cover/fill,
   cropping in texture space.
3. ✅ **`deck.ft` + `deck.glz`** — front matter, `---` separators,
   `<!-- k: v -->` directives, `::: notes`, arrow/click nav, a progress
   footer, and `deck.slide` published retained (including on start).
   Type scales from a 1280×720 reference to the pane.
4. ✅ **`PaneGroup` + `VisibleGroups` + `jimctl group`.** A grouped pane is
   hidden until its group is revealed; revealing is a `Visibility` write,
   not a spawn. Membership persists in `PaneSnapshot.group`. The deck
   publishes the generic topic `pane.groups` `{show:[…]}` and the app
   consumes it — the app never learns what a deck is.
   **The app-side bus subscriber turned out to already exist**:
   `jim_widget::BusMessageObserved` is emitted for every delivered bus
   message, so `pane_groups::apply_bus_group_messages` just reads it.
   (Caveat that shaped the design: the *retained backlog* replayed to a
   late joiner does NOT surface as `BusMessageObserved` — which is why a
   publisher must announce its state on start, as `deck.ft` does.)
4b. ✅ **`project_region.rs` — a whole project inside a slide.** The real
   answer to "show me a project", and the primary demo path. A slide's
   `project:` directive publishes `pane.project {name, inset}`; the app
   fits that project's canvas bounding box into a region of the
   publishing pane and draws its panes there, live.

   Mechanism, both parts already existed: `PaneVisualScale` scales a pane
   about its top-left **without touching `PaneRect::size`** (so a terminal
   shrinks visually instead of re-wrapping) and is already folded into
   both `position_panes` and the per-pane camera; `PaneScreenAnchored`
   makes `PaneRect::pos` window pixels, so a guest pane can be placed in
   screen space without inheriting the host project's pan/zoom. Exposé
   uses the same scale-don't-resize trick.

   The region is anchored to the *publishing* pane by decoding the bus
   `sender` id (`rw<entity-bits>`) back to an entity — a widget has no
   idea where it sits on screen, so `inset` is expressed as fractions of
   its own pane.

   The later view-tree work completed that refactor: input resolves through
   the deepest `View`, then filters pane targets by that view's project.
   Clicking, dragging, scrolling, focusing and typing in a slide therefore
   operate on the real guest panes.

5. **`present.rs` — BUILT** — fullscreen, chrome hiding, deck-pane-fills-display,
   global key chords, Esc. Test the monitor-cascade hazard hard.
6. **Polish — BUILT FOR V1** — named groups and docks provide dashboard
   layouts. Speaker-notes UI and incremental build reveals remain optional
   follow-up features rather than blockers for presenting.

Stage 4 is where the ambition actually lands, and it's the one with no
existing precedent to copy.

### What shipped in 1–4

| | |
|---|---|
| `crates/jim-widget/src/glaze_host.rs` | the Glaze⇄funct bridge |
| `glaze::Program::token` / `token_names` | typography from the sheet (styles are box-only) |
| `protocol::{RichText, TextRun, Image, ImageFit}` | + `layout.rs` run tokenizer, `render.rs` `TextSpan` path and `fit_image` |
| `funct_widget::md_parse_to_json` | `markdown_core` → widget-shaped blocks |
| `widgets/{deck,glaze_demo,md_demo}.ft` + `{deck,glaze_demo}.glz` | the deck and two demos |
| `cargo run -p glaze --example check -- x.glz` | sheet linter; exits 1 on failure |
| `jim_pane::PaneGroup` + `PaneSnapshot.group` | named, revealable pane groups (persisted) |
| `jim-app/src/pane_groups.rs` | `VisibleGroups` + the `pane.groups` bus consumer |
| `jimctl group assign\|clear\|show\|hide\|list` | wire and rehearse a deck's dashboards |
| `jim-app/src/live_views.rs` | interactive project/whole-app views via direct cameras |
| `docs/example-deck.md` | every format feature, including a `project:` slide |

76 tests in `jim_widget`, 39 in `jim_app`, 31 in `glaze`. The deck, both
demos, and their sheets are covered end-to-end (booted through
`import "host"` in a scratch module root, rendered, asserted on the real
`Element` tree). The reveal chain was additionally verified in the running
app: `jimctl group assign` → pane hides → `show` → pane visible, and a
deck booting on a `dashboard:` slide revealing the group by itself.

---

## Part 5 — Known traps (prior scars in this repo)

- **Widget churn frame spikes.** funct flow-widgets despawn/respawn their
  whole entity tree per re-render (`jim-pane/churn.rs` reports `+N/-N`).
  A fullscreen slide is a *large* tree. Re-render only on slide change;
  never `set_animating(true)` for a static slide.
- **`Blend` `Mesh2d` is invisible through per-pane cameras** (Bevy 0.19
  regression). Slide decoration meshes must use `AlphaMode2d::Opaque`.
- **`pow(neg, 2)` is NaN in WGSL** → silently gray panes. In Glaze
  `shader {}` bodies, square with `x*x`.
- **Idle CPU.** `push_chrome_time` was a ~60% idle sink until gated behind
  `ChromeAnimates`. Keep animated Glaze layers opt-in per slide.
- **Colour/`.ttc` fonts panic Bevy's rasterizer** — `is_safe_fallback_font`
  skips them, so emoji-heavy slides render `�`.
- **Blank widget = whole-frame deserialize reject.** One bad enum field
  (e.g. `align: "baseline"`) drops the entire frame. `grep` the log for
  `frame deserialize` first.
- **Never mass-close panes.** A dashboard teardown must target by group,
  never `jimctl close --project P` (that wiped Recursion once).
- **Restart via `./scripts/dev-restart.sh`** — never a bare binary launch.

---

## Open questions

1. **Who owns dashboard membership** — `jimctl group add` (explicit,
   scriptable), a `group:` key in the pane snapshot config (persisted,
   hand-edited), or drag-select on canvas (WYSIWYG, needs a serializer)?
2. **Where do dashboard panes live** — same canvas as the deck (hidden
   until named), or parked on a nested canvas and shown in place? The
   first is simpler; the second keeps a busy canvas tidy between talks.
3. **Deck chrome on a dashboard slide** — hide the deck entirely, or keep
   a corner strip with title/progress?
4. **Resolution-relative authoring** (question F) — Glaze `when`
   breakpoints, or a deck-computed scale factor applied to every size?
5. **`RichText` scope** — full run-level line breaking, or cheaper
   "styled segments that break only at segment boundaries"?
