# Views — the same live content, in several places at once

> Architecture + implementation. **Stages 1–6 are built.** The former
> texture projection has been removed; `live_views.rs` uses direct cameras.

## Framing: a view is a CAMERA, not a thumbnail

The single most important property, and the one I got wrong repeatedly:

> **A view shows the project at 1:1 by default.** It is a window onto the
> canvas showing whatever portion fits in its rect — not the whole project
> squeezed into it.

Fitting the whole project into a small rect is what produced an unreadable
0.12x miniature. A view has its own `pan` and a `zoom` that defaults to
**1.0**; making it bigger reveals *more of the project*, exactly like
resizing the window does. Scaling is available (it is just the view's
zoom) but it is never the default and never automatic.

This is what makes the rest coherent: several views of the same project
side by side, or in a grid, each looking at a different part of it at
native size, each independently pannable, all interactive, all the same
underlying panes.

## The goal

Show the **real, working thing** in more than one place at a time. Drag a
pane in a slide and it moves on the canvas, because it *is* the canvas.
Type in a terminal inside a slide and it's the same shell. Put the whole
application — sidebar, chrome and all — inside a slide. And let that
nest: the slideshow visible inside its own slide, cut off at a depth
limit.

## The reframe that makes it tractable

I built a texture and called it a projection. That was wrong twice over:
it isn't interactive, and it can't nest.

But the instinct that "we must copy the content" is also wrong. **The
content is already single-instance and already capable of being drawn many
times.** Bevy renders an entity once per camera that can see it; `cube.rs`
already draws every pane a second time, live, on a rotating prism.

What is single-instance is not the content. It is the **mapping from world
to screen**:

- `PaneViewport` is ONE global `(origin, pan, zoom)` that every hit-test,
  drag, dock and camera-sync system reads.
- Each pane has exactly ONE camera (`PaneCameraOf`), whose viewport clips
  its content to one screen rect.

So there is exactly one answer to "where is this pane on screen?" and one
answer to "what is under the cursor?". Make **those** plural and multiple
live views fall out — no copying, no textures, no snapshots.

> This is why the texture approach is a dead end rather than a stepping
> stone: it makes a *picture* plural while leaving the mapping singular.
> Every hard part (input, focus, nesting, chrome) is in the mapping.

## The model: a view tree

```rust
/// One place the world is drawn. View 0 is the window itself.
struct View {
    id: ViewId,
    parent: Option<ViewId>,
    /// Screen rect, in window px. Computed through the parent chain.
    rect: Rect,
    /// What this view frames: which project, and its pan/zoom.
    project: u64,
    canvas: CanvasViewState,   // pan + zoom, already exists per project
    /// Extra non-pane layers to include (sidebar, overlays) — see below.
    chrome: ChromeSet,
    depth: u8,
}
```

- **View 0** is today's behaviour exactly: rect = the window, project =
  the active project, canvas = that project's pan/zoom.
- A **slide view** is a child: rect = a sub-rect of the publishing pane,
  project = whatever the slide names.
- A view's `transform()` is the old `PaneViewport` — the same struct,
  now one per view instead of one global.

Nesting is the tree. A view inside a pane that is itself drawn by another
view composes by rect intersection down the chain. **Depth cap:** views
below `MAX_VIEW_DEPTH` (2 is plenty) are simply not instantiated — the
region draws empty. That is the whole recursion story; there's no cycle
detection to get wrong, because a bounded tree can't cycle.

## Rendering: a camera per (pane, view)

Per-pane cameras exist because content is laid out in content-local
coordinates and must be clipped to the pane. That requirement doesn't go
away, so the generalization is:

> today: one camera per pane
> then:  one camera per (pane, view) where the view can see that pane

Each such camera gets:

- `viewport` = the pane's rect projected through **that view's** transform,
  intersected with the view's rect (this is what clips a pane at the edge
  of a slide region)
- `order` = the view's order base + the pane's z, so views stack
  predictably
- `RenderLayers` = the pane's existing layer — unchanged

`cube.rs` already spawns exactly this shape of camera (`face_cam`,
`RenderLayers::from_layers(&[layer])`, `image_target`, ortho scale matched
to zoom) and it works, live, today. We're reusing a proven path, pointed
at a window viewport instead of a texture.

**Cost**: panes × views cameras. A demo with one extra view over a
20-pane project is 40 cameras; the prism already spawns one per pane
across every project. Views are opt-in and short-lived, so idle cost is
unchanged.

## Input: one funnel, routed by view

Every interaction today starts by turning a window point into a canvas
point with the single global viewport. That becomes:

```rust
/// The deepest view whose rect contains `p`, and `p` in its canvas space.
fn resolve(p: Vec2, views: &Views) -> Option<(ViewId, Vec2)>
```

Deepest-first, so a slide view over the canvas wins for points inside it.
Then everything downstream — hit-test, focus, drag, resize, dock, context
menu — keeps working **unchanged**, because it already operates in canvas
space after that conversion.

This is the load-bearing refactor: roughly ten `Res<PaneViewport>` readers
across `jim-pane` (`position_panes`, hit-tests, `sync_pane_cameras`,
`dock`) and `jim-app` (`lib`, `context_menu`, `whiteboard_bg`,
`pane_annotation`). Each becomes "resolve the view, then use its
transform". Mechanical, but it is the core interaction path, so it needs
care and tests rather than speed.

Consequences that are *features*, not problems:

- Dragging a pane in a slide moves the real pane; it moves in every view
  at once, because there is one `PaneRect`.
- Clicking a pane in a slide focuses it globally. Correct: it's the same
  pane.
- Typing goes to the focused pane regardless of which view you clicked in.

## Where a pane *is*, and the one real constraint

An entity has one `Transform`, so a pane occupies one position in world
space. Two views can therefore differ in **framing** (which part of the
world, at what scale, into which screen rect) but not in **layout** (they
can't show the same pane at different relative positions).

That is exactly what we want — a view is a window onto the canvas, not an
alternate arrangement — but it's worth stating, because it rules out
"slide shows a different layout of the same panes". If that's ever wanted,
it's a second mechanism (per-view layout overrides), not this one.

## Chrome, and "the whole application in a slide"

The sidebar, breadcrumb, status bar and overlays are drawn by global
cameras on reserved layers (`MENU_OVERLAY_LAYER` 32,
`WHITEBOARD_OVERLAY_LAYER` 31, `dynamic::OVERLAY_LAYER` 30 — the registry
is in `lib.rs`). They're already layer-isolated, which is most of the work.

**Checked, and it's easier than it looks.** The sidebar is *not* on a
reserved layer — it's on layer 0, drawn by the main camera, positioned in
**world** coordinates at the window's left edge (`world_left_edge =
-win_w * 0.5`, `sidebar_layout` in `projects.rs`). Its window coupling is
13 references, all of the form "give me the window rect".

That means a faithful whole-application view needs **no chrome refactor at
all**:

> **Whole-app view = frame the entire window's world rect, render layer 0
> plus every pane layer, into the view's screen rect.**

The sidebar shows up because it's *already there* in the world region
being framed. Same for the canvas background, the breadcrumb, the dust
shader. A view is just "which world rect, which layers, into which screen
rect" — and "the whole app" is the trivial case where the world rect is
the whole window.

So chrome stops being a separate stage. Laying chrome out per-view
(refactoring `sidebar_layout` to take a rect instead of reading the
window) is only needed if a view should show the sidebar at a *different*
size or position than the real one — which is not what we want. We want a
faithful miniature.

The consequence to design for: a whole-app view frames the window, and the
window contains the slide pane, so the view contains itself. That's the
recursion — wanted, and bounded by `MAX_VIEW_DEPTH`.

## Staging

Each stage is shippable and independently verifiable.

**1. Views exist, with exactly one view. — BUILT**
Introduce `View` / `Views`, make view 0 the window, and route every
`PaneViewport` reader through it. **Success = nothing changes**: no
behaviour difference, no perf difference. Land this alone, live on it for
a day. This is the risky refactor and it must be boring.

**2. A second view, panes only. — BUILT**
Debug command that opens a view of the active project in a corner rect.
Verify: content is live, panes are clickable *in the view*, dragging in
the view moves the pane in both. Nothing about decks yet.

**3. Slide views. — BUILT**
`view.open {project, inset}` from a widget; the deck's `project:`
directive publishes it. The region is anchored to the publishing pane's
rect (decode the bus `sender` id → entity, already working).

**4. Nesting + depth cap. — BUILT**
Views inside views; `MAX_VIEW_DEPTH`. Verify the slideshow-inside-itself
case renders two levels and then stops.

**5. Whole-app views. — BUILT** A view whose world rect is the whole window and
whose layer set includes layer 0 — sidebar, tabs, canvas background, the
lot. Per the finding above this is a *configuration* of stage 2, not new
machinery. Recursion (stage 4) is what makes it survive containing itself.

**6. Delete `project_region.rs`. — BUILT** The texture path goes away; nothing
should be left that fakes this.

## Risks, honestly

- **Stage 1 is a refactor of the code that makes the app feel right** —
  drag, resize, focus, dock, context menu. A subtle regression here is
  worse than no feature. Mitigation: view 0 must be provably identical
  (golden tests on the resolve funnel), and land it separately from
  anything user-visible.
- **Camera count** grows as panes × views. Fine at demo scale; needs a cap
  and a "views are opt-in" rule so idle cost never changes.
- **Text sharpness under scale.** A view at 0.3× scales rasterized glyphs,
  same as canvas zoom does today. It will look like zoomed-out canvas
  looks. If the demo needs crisp text in the slide, the answer is to frame
  a *smaller region* of the project at a higher scale, not to fit the whole
  canvas — worth designing into the slide directive (`project: X, zoom:
  0.8` or naming a focus pane).
- **Focus ambiguity.** One `FocusedPane` globally is right, but the
  highlight will appear in every view showing that pane. Probably correct;
  worth looking at before deciding.
- **Presenter keys vs. focus.** Once the app-in-a-slide is really
  interactive, clicking into it moves keyboard focus *inside* the view —
  and Right must still advance the deck. Deck navigation therefore has to
  be app-level `Action` + `KeyChord` that outranks the focused pane, not
  the widget's `on_key`. (This was already stage 5 of PRESENT.md; views
  make it mandatory rather than nice-to-have.) Decide the exit gesture
  too: Right advances off the slide, Esc leaves the view.

- **`PaneScreenAnchored`** panes (whiteboard toolbar) bypass the canvas
  transform by definition. In a view they should either be excluded or
  anchored to the view rect — decide in stage 2.

## What I'd need decided

1. ~~Framing default~~ — **decided: 1:1, zoom 1.0, its own pan.** A view
   is a camera. See "Framing" at the top.
2. **Chrome scope** — is sidebar-in-slide required for the demo, or is a
   panes-only view enough? It's the difference between stages 1–4 and
   1–5, and stage 5 is the least predictable.
3. **Sequencing** — land stage 1 (invisible refactor) on its own first, or
   push straight through to a visible slide view and accept a riskier
   single landing?
