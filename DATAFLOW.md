# Dataflow — pipeable data widgets

> Status: **design**. Nothing here is built yet. This document is the
> contract we agree on before writing code. It is deliberately concrete
> so that two people (or one person and a funct script) can implement
> independent pieces and have them compose.

## 1. What we're building

A **reactive dataflow graph** of small widgets that pipe data to each
other:

```
[source] ──▶ [transform] ──▶ [transform] ──▶ [view]
                                 └────────▶ [view]      (fan-out)
```

- **Source** — produces data (hits an HTTP endpoint, reads a file, runs
  a query). Has a refresh policy.
- **Transform** — data → data (parse, normalize, filter, group, split,
  join). Pure; recomputes when its inputs change.
- **View** — data → pixels (line / bar / table / stat / heatmap …).

Every node is the **same widget kind** under the hood, differentiated by
an `op` + `config`. Adding a capability means adding an op, not a new
subsystem. That single decision is what keeps the system extensible:
"many widget types, many operations, many ways of viewing" all reduce to
"more ops over one data contract on one transport."

### Non-goals (for now)

- Not a general visual-programming environment. The graph is the means,
  analytics dashboards are the end.
- Not a streaming/event-per-row engine. We pass **whole datasets** on
  each tick (snapshots), not row deltas. (Revisit only if a dataset
  gets large enough to hurt — see §11.)

---

## 2. The keystone: the dataset envelope

This is the single most important decision in the whole design. **If
every node speaks one data shape, everything composes for free.** This
is the same lesson behind Grafana "data frames" and Vega-Lite
"data + encoding": standardize the data, parameterize the view.

We standardize on a **tidy / long table**: a list of row records plus a
typed schema describing each column's *type* and *role*.

```jsonc
{
  "kind": "dataset",
  "schema": {
    "columns": [
      { "name": "date",  "type": "time",   "role": "dimension" },
      { "name": "path",  "type": "string", "role": "dimension" },
      { "name": "views", "type": "number", "role": "measure" }
    ]
  },
  "rows": [
    { "date": 1781000000000, "path": "/api/x", "views": 42 },
    { "date": 1781086400000, "path": "/api/x", "views": 51 }
  ],
  "meta": {
    "source": "analytics/timeline",
    "params": { "days": 30, "bucket": "day" },
    "fetched_at": 1781200000000,
    "row_count": 2
  }
}
```

### Column types

| type     | encoding                          | notes                          |
|----------|-----------------------------------|--------------------------------|
| `time`   | epoch-ms integer                  | always UTC ms; views localize  |
| `number` | f64                               | integers are numbers too       |
| `string` | utf-8                             | categories, labels             |
| `bool`   | true/false                        |                                |
| `null`   | absent or JSON null               | missing measure ≠ 0            |

### Column roles

- `dimension` — something you group/split/slice **by** (time, path,
  region, status).
- `measure` — something you aggregate (views, count, rate).

Roles are advisory but they let views auto-pick sensible defaults
(x = first time/dimension, y = first measure, series = second
dimension) so a view can render *any* dataset with zero config and be
refined later.

### Two sibling envelope kinds (same channel)

```jsonc
{ "kind": "scalar", "value": 51967, "label": "views",
  "delta": { "value": 1820, "pct": 3.6, "dir": "up" },
  "meta": { ... } }
```

```jsonc
{ "kind": "error", "message": "curl exit 6: could not resolve host",
  "stage": "source", "node": "src", "meta": { ... } }
```

Everything that flows on a pipe is one of `dataset | scalar | error`.
Errors **flow** (they don't get swallowed) so a view can render a red
banner instead of going silently blank — and per global rule, a node
that cannot do its job emits an `error` envelope, it never emits empty
`rows` that masquerade as "no data."

### Wide vs long

We commit to **long** (tidy) as the wire format. A series column (e.g.
`path`) distinguishes lines. `pivot`/`unpivot` ops convert when a view
or transform wants wide. Picking one canonical format avoids every node
having to handle both.

---

## 3. The pipe = the existing widget msgbus

We do **not** build new transport. Each edge is a **topic**; the
existing project-scoped bus carries envelopes:

- `emit_retained(topic, envelope)` to push downstream.
- `on_message(topic, payload, sender)` to receive.

**Why `emit_retained` and not `emit`:** retained = MQTT last-value-per-
topic. A view spawned (or hot-reloaded) *after* its source already ran
immediately receives the last dataset on init. That gives us free
per-node output caching and cold-start resilience with zero extra code.

### Topic naming convention

```
df/<dashboard>/<node-id>/out          # a node's output
df/<dashboard>/<node-id>/out/<key>    # a split node's per-partition outputs
df/<dashboard>/control/<verb>         # graph-wide control (pause, refresh)
```

Everything is project-scoped already, so two dashboards in different
projects can't cross-talk. Within a project, the `<dashboard>` segment
namespaces independent graphs.

An **edge** is just: "node B's `in` list contains topic
`df/dash/A/out`." No new edge object — edges are topic references.

---

## 4. Node model

Every node is one funct widget (or subprocess widget) with this shape:

```jsonc
{
  "id": "chart",                 // unique within the dashboard
  "op": "view.line",             // which behavior (registry key)
  "in": ["df/dash/norm/out"],    // input topics (0..n)
  "out": "df/dash/chart/out",    // output topic (views may omit)
  "config": { "x": "date", "y": "views", "series": "path" },
  "refresh": "on_input"          // see §6
}
```

Runtime behavior of the generic node:

1. On init, subscribe to every topic in `in`. Retained backlog arrives
   immediately.
2. On each `on_message`, store the latest envelope per input topic.
3. Run the op's `process(inputs, config) -> envelope` (transforms/
   sources) or `render(inputs, config, w, h) -> Element` (views),
   honoring the refresh policy (§6).
4. Transforms/sources `emit_retained(out, result)`. Views call
   `request_render()`.

A node with `in: []` is a **source**; a node with no `out` is a **view**;
everything else is a **transform**. There is no enum — the wiring
defines the category.

---

## 5. Op catalog

Ops are grouped by category. Each is a small funct script registered by
key (§9). v0 ships the **bold** ones; the rest are the roadmap.

### Sources (`source.*`)

- **`source.http`** — fetch a URL with params + bearer auth, emit the
  raw body (string) or parsed JSON. v0 implementation: `proc_spawn
  ("curl", [...])`, token from `host_env("JIMMY_API_KEY")`. v1: a native
  `http_get` host fn (no per-tick process spawn — see §10).
  - config: `{ url, method, query, headers, body, auth_env }`
- `source.file` — read/tail a local file.
- `source.proc` — run an arbitrary command, capture stdout.

Analytics endpoints are **presets** over `source.http`, e.g.
`{ op: "source.http", config: { preset: "analytics/timeline",
days: 30, bucket: "day" } }`.

### Transforms (`xf.*`) — all `dataset -> dataset`

- **`xf.adapt`** — map a source's idiosyncratic JSON into the standard
  dataset. Each analytics endpoint has a *different* shape (`by-day` is
  `{by_day:[{date,views}]}`; `timeline` is `{series:[{path,points:
  [...]}]}`; `top-paths` is `{top:[{value,views}]}`). `xf.adapt` holds
  one adapter per known shape (keyed by `meta.source`) plus a generic
  "pluck path → rows" config for unknown JSON.
- **`xf.select`** — keep/rename/reorder columns.
- **`xf.filter`** — keep rows matching a predicate.
- **`xf.sort`** — order rows by column(s).
- **`xf.limit`** — top/bottom N.
- `xf.derive` — add a computed column (`rate = a / b`).
- `xf.groupby` — group by dimension(s) + aggregate measures
  (sum/avg/count/min/max).
- `xf.bucket` — resample a `time` column (day→week→month).
- `xf.pivot` / `xf.unpivot` — long↔wide.
- `xf.join` — combine two datasets on a key.
- **`xf.split`** — **the fan-out primitive** (§7).

### Views (`view.*`) — `dataset -> Element`

- **`view.table`** — uses existing `Element::Table` directly. Zero new
  rendering. First view to ship.
- **`view.stat`** — big number + delta arrow from a `scalar` envelope.
  Uses `Text`/`Badge`. Also new-render-free.
- **`view.bar`** — bars via `Element::Bar` or stacked `Frame` rects.
  No path primitive needed → second view to ship.
- `view.line` / `view.area` — need a real polyline (see §8).
- `view.stacked-area`, `view.scatter`, `view.heatmap`,
  `view.calendar`, `view.pie`, `view.sparkline` — later.

### The encoding contract (how views stay generic)

A view never hard-codes columns. It takes an **encoding** that maps
dataset columns onto visual channels:

```jsonc
{ "x": "date", "y": "views", "series": "path",
  "agg": "sum", "color": "auto", "stack": false }
```

Unspecified channels fall back to role defaults (§2). This is the whole
reason one `view.line` can chart *any* dataset — "many ways of viewing
the data" is just different encodings over the same envelope.

---

## 6. Scheduling & reactivity

Honors the project rule **widgets must be event-driven, not
tick-polled**. The graph is *pull at the edges, push in the middle*.

Per-node `refresh` policy:

| policy           | who uses it      | behavior                                   |
|------------------|------------------|--------------------------------------------|
| `on_input`       | transforms, views (default) | recompute only when an input envelope arrives |
| `interval(<dur>)`| sources          | opt-in timer via `set_animating` cadence; re-fetch + emit; downstream reacts |
| `manual`         | any              | recompute only on a refresh control message / button |
| `on_demand`      | sources          | fetch once on init, then never until manual |

So the **only** thing that polls is a source you explicitly told to.
Everything downstream is pure push — a source emits, subscribers wake
(the bus already wakes workers immediately, no per-frame scan).

Additional controls:

- **Debounce/throttle** per node (`min_interval_ms`) so a chatty source
  can't thrash an expensive view.
- **Graph control topic** `df/<dash>/control/*`: `pause`, `resume`,
  `refresh` (all), `refresh:<node-id>` (one). A toolbar widget or
  `jimctl msg emit` can drive these.
- Each node tracks `meta.fetched_at` / last-run time so views can show a
  "updated 3s ago" stamp and a staleness tint.

Interval sources don't free-run hot: reuse the 30 Hz `on_frame` opt-in
only to *check a deadline*, or (better, v1) a coarse host timer so a
"every 5 min" source isn't holding a 30 Hz tick. v0 can use a cheap
deadline check inside an existing low-rate tick.

---

## 7. Splitting & fan-out — "view it many ways"

Two mechanisms, both already supported by the bus:

**Fan-out (one → many subscribers).** Multiple nodes list the same
topic in their `in`. One `source.http → xf.adapt` can feed a table, a
bar chart, and a stat card simultaneously — just three nodes subscribing
to `df/dash/norm/out`. Retained delivery means each one lights up the
moment it spawns.

**Split / partition (one dataset → many datasets).** `xf.split`
partitions rows by a key column and emits each partition to a
**sub-topic**:

```jsonc
{ "id": "by_region", "op": "xf.split",
  "in": ["df/dash/norm/out"],
  "config": { "key": "region" },
  "out": "df/dash/by_region/out" }
// emits df/dash/by_region/out/us-east, /us-west, /eu, ...
```

Downstream views subscribe to a specific partition, or a "small
multiples" view subscribes to the parent and lays out one mini-chart per
partition. This is the literal "split the data and view it in many ways"
ask: split once, render each slice independently.

---

## 8. Drawing layer — **both** shader and canvas

We build **two** complementary draw paths, per the decision. They share
nothing but the dataset; pick per-view by which trade-off you want.

### 8a. Path/Canvas element (CPU-tessellated, exact, interactive)

The existing `Element::Canvas` has `Sprite`/`Rect`/`Text` items but **no
polyline** — diagonal strokes today require rotated rects, which is
unworkable for a 200-point line. So we **extend `CanvasItem`** with real
vector primitives:

```rust
enum CanvasItem {
    Sprite { .. }, Rect { .. }, Text { .. },   // existing
    Polyline { id, points: Vec<[f32;2]>, color, width, closed, z },
    Path     { id, segments: Vec<PathSeg>, fill, stroke, width, z },
    //         segments: MoveTo|LineTo|QuadTo|CubicTo  (bezier)
}
```

- Rendered by tessellating into triangles on the Bevy side (a thin
  stroke-to-mesh + fill-to-mesh step; `lyon`-style or a hand-rolled
  miter stroker). One mesh per `Polyline`/`Path`, diffed by `id`.
- This is the **interactive, pixel-exact** path: line/area charts,
  axes, gridlines, tooltips that hit-test against points, crosshairs.
- Views compute screen coords in script (we provide scale helpers —
  `linear_scale`, `time_scale`, `nice_ticks`) and emit `Polyline`/`Path`
  + `Text` labels into a `Canvas`. No GPU data plumbing needed.
- This is the workhorse for most charts and the first of the two to land
  because it needs no new uniform pipeline.

### 8b. Glaze data-shader plotter (GPU, smooth, "alive")

Glaze shaders today get only **frame-global** uniforms (`time`, `size`,
`mouse`, interaction state) — **there is no way to feed a data series
into a shader.** That gap is the real work for shader charts. We close
it by adding a **data channel** to the Glaze material:

- New `GlazeLayer::DataPlot { body, data_ref }` (or extend the existing
  `Shader` layer) where the host uploads the node's series as GPU data:
  - small series → a uniform array (`data: array<vec2<f32>, N>` +
    `data_len`), or
  - any size → a **1×N data texture** the shader samples (robust, no
    fixed cap). Recommended default.
- New canonical uniforms alongside `GlazeUniforms`: `data_len`,
  `data_min`, `data_max`, `x_min`, `x_max` so the WGSL can normalize.
- The script declares `{ op: "view.line", config: { renderer: "glaze",
  shader: "<glaze-fn>" } }`; the host packs `rows[].{x,y}` into the
  data texture each time the dataset changes (not per frame — only on
  data change, then the shader animates for free off `time`).

This path is for **continuous / animated / dense** visuals: glowing
area gradients, smooth resampled curves at any zoom, heatmaps, density
plots, sparkline shimmer. It trades pixel-exact hit-testing for GPU
smoothness and motion.

### Choosing between them

| want…                              | use            |
|------------------------------------|----------------|
| exact points, tooltips, hit-test   | 8a Canvas/Path |
| crisp axes, gridlines, labels      | 8a Canvas/Path |
| dense series, smooth at any zoom   | 8b Glaze       |
| animated/"alive" gradients, glow   | 8b Glaze       |
| heatmap / density field            | 8b Glaze       |

A single chart can combine them: Glaze area fill underneath (8b) + a
Canvas polyline stroke and axis labels on top (8a). The dataset feeds
both.

---

## 9. Authoring & extensibility

### Declarative dashboard file (phase 0)

A dashboard is a JSON/`.ft` file under `~/.jim/dashboards/<name>`.
Spawning it materializes the nodes as widgets and auto-wires their
topics. Diffable, hot-reloadable (same watcher as widgets), hand-
writable.

```jsonc
{
  "dashboard": "analytics",
  "nodes": [
    { "id": "src",   "op": "source.http",
      "config": { "preset": "analytics/timeline", "days": 30 },
      "refresh": "interval(5m)" },
    { "id": "norm",  "op": "xf.adapt",  "in": ["df/analytics/src/out"] },
    { "id": "chart", "op": "view.line", "in": ["df/analytics/norm/out"],
      "config": { "x": "date", "y": "views", "series": "path" } },
    { "id": "table", "op": "view.table","in": ["df/analytics/norm/out"] }
  ]
}
```

(`df/analytics/...` topics derive mechanically from `dashboard` + `id`;
authors can write `"in": ["src"]` shorthand and the loader expands it.)

### Visual node editor (phase 1)

A canvas pane where you drag nodes and draw edges; it reads/writes the
same file. The pane infra (drag/resize/hit-test/z-order/close) already
exists — the **new** bit is rendering edges *between* panes and a port
hit-target. Edges drawn with the §8a Polyline primitive (dogfooding).

### Extensibility contract (the op registry)

Mirror the existing `PaneRegistry`/`PaneKindSpec` pattern. One
`OpRegistry` maps op key → spec:

```rust
struct OpSpec {
    key: &'static str,            // "xf.groupby"
    category: OpCategory,         // Source | Transform | View
    in_arity: Arity,             // Exactly(1) | Range(0,N)
    config_schema: Value,         // JSON-schema-ish, drives the editor form
    // impl is a funct script implementing process()/render()
}
```

New op = drop a funct script implementing the known interface into a
watched dir (`~/.jim/dataflow/ops/<key>.ft`), hot-reloaded. New view =
a script implementing `render(inputs, config, w, h) -> Element`. No
recompile for script-level ops; only new *draw primitives* (§8) or the
native `http_get` need Rust.

This is what makes "many operations, many widget types" cheap: the
contract is fixed (`process`/`render` over the envelope), the catalog is
open.

---

## 10. What exists vs. what's new

| Need                         | Status                                            |
|------------------------------|---------------------------------------------------|
| Pipe transport               | ✅ `msgbus` (`emit_retained` / `on_message`)      |
| Source fetch                 | ✅ `proc_spawn("curl")` today                      |
| Native HTTP host fn          | 🆕 `http_get` (v1; avoids per-tick process spawn) |
| Node runtime + hot reload    | ✅ funct script widgets, `~/.jim/widgets/` watcher |
| Event-driven recompute       | ✅ handlers + opt-in `set_animating`               |
| Table / stat / bar views     | ✅ `Element::Table`, `Bar`, `Frame`, `Badge`       |
| **Dataset envelope**         | 🆕 a convention (cheap; discipline, not code)      |
| **Polyline/Path canvas item**| 🆕 extend `CanvasItem` + tessellation (§8a)        |
| **Glaze data channel**       | 🆕 data texture/uniforms into shader (§8b)         |
| Op registry                  | 🆕 model on `PaneRegistry` (§9)                    |
| Declarative dashboard loader | 🆕 file → spawned wired nodes                      |
| Visual node editor           | 🆕 phase 1; edges via §8a                          |

---

## 11. Build phases

**Phase 0 — generic primitives. ✅ SHIPPED.**
The system is **two generic primitive kinds wired by params + a shared
bus topic** — NOT a file per endpoint. Per-instance config arrives via
the funct global **`params`** (host.ft `extern let params`), plumbed
`ScriptWidgetConfig.params` → `funct_worker_main`
(`vm.set_global("params", Value::from_json(..))`) → IPC `spawn_widget`
`params` field. That is what makes a `.ft` a reusable primitive.

The primitives (in `crates/jim-widget/widgets/`):

- `http.ft` — the only source. Params `{ url, out, curl_cfg, interval }`.
  GETs any URL, publishes `{kind:"raw", body}` (retained) on its `out`
  topic. Auth optional via a `-K` curl-config path (token never in a
  script).
- `df.ft` — the keystone library: the `dataset` envelope + generic
  `extract(body, rows_path, x, y)` (flat) and `extract_series(body,
  series_path, label, points, x, y)` (nested multi-series), dispatched by
  `dsify(payload, params)`. No per-endpoint adapter functions.
- Generic chart views, each params `{ in, rows_path, x, y }` (or
  `series_path/label/points` for multi-series): `df_view_table.ft`,
  `df_view_bar.ft` (horizontal), `df_view_vbars.ft` (vertical),
  `df_view_stat.ft` (big number + Δ), `df_view_line.ft` (single line),
  `df_view_heatmap.ft` (grid), `df_view_multiline.ft` (one line per
  series + legend). All canvas charts handle `on_hover` → highlight +
  value tooltip.

**The connector** is just a shared topic: `http.out == chart.in`.
Swapping data = swapping params, never touching a widget. `scripts/
df-dashboard.sh [Project] [by-day|blog|top|timeline|combined|quakes|
github]` is a thin param-spec launcher: a preset IS the params.

**Generic over ANY API.** `extract`/`extract_series` take DOTTED paths
(`dig(e, "properties.mag")`), so nested JSON works, not just flat. Proven
on totally different public APIs with the same widgets: USGS earthquakes
(`…/all_day.geojson`, nested) and GitHub contributors (top-level array).
Public presets set `curl_cfg:""` (no auth); analytics presets pass the
bearer curl-config.

**Theme-aware + reactive.** Charts pull every color from the active
theme via `theme_get` tokens (`df.ft`: `col_text/col_muted/col_axis/
col_accent/col_panel/col_err/col_warn`; `series_color` from the theme's
`syntax_*` scale; `ramp` = accent at value-scaled alpha). On a theme
switch (`set_active_style(name)` — see `set_theme.ft`), the host
re-renders every widget so charts recolor live: `jim-widget` lib.rs
`rerender_widgets_on_theme_change` uses a short frame countdown to
outlast the theme-snapshot sync (a single-frame force left some charts
stale). Verified amber→forest.

**Unicode / font fallback (general fix).** Bevy renders `Text2d` with a
single pinned font and does NO glyph fallback, so any codepoint missing
from the family (JetBrains Mono / Inter / Crimson Pro) silently vanished
(`⇄ ▦ ▮` etc.). Fixed by bundling DejaVu Sans (`"symbols"` family),
parsing each font's cmap into a coverage set (`jim-style/fonts.rs`,
`FontRegistry::split_runs`), and a `PostUpdate` system
(`jim-widget/text_fallback.rs`) that splits any flow `Text2d` whose font
lacks a glyph into child `TextSpan`s drawn from the fallback. Canvas text
is diffed in place (`script_widget::diff_render`), so the global
`PostUpdate` splitter skips it (marked `CanvasManagedText` — splitting
there fights the diff and duplicates); instead **the reconcile runs
`split_runs` itself and owns the resulting `TextSpan` children** (tracked
in `sprite_entities` under composite keys so the stale-cleanup reaps them
when the run count shrinks). Net: canvas labels render every codepoint
just like flow text — **no "covered glyphs only" rule for canvas**.

**Build pipes from the shell.** `jimctl widget` now takes `--params
JSON` (+ `--pos x,y`, `--size w,h`), so a generic primitive is fully
configurable from the CLI — no launcher needed:
```
jimctl widget -k script_widget -p Recursion --pos 40,60 \
  --params '{"url":"https://api…","out":"feed","curl_cfg":""}' http.ft
jimctl widget -k script_widget -p Recursion --pos 40,230 \
  --params '{"in":"feed","rows_path":"items","x":"name","y":"count"}' df_view_bar.ft
```
That's a complete source→chart pipe (connected by the `feed` topic).
`df-dashboard.sh` is just a convenience wrapper over these.

**Theme reactivity caveat (fixed).** Charts re-render on the
`jim_style::ThemeChanged` *message* (the canonical signal the chrome
uses) — NOT `Res<Theme>::is_changed()`, which the style-picker path
doesn't trip for a widget-side query. `rerender_widgets_on_theme_change`
reads `MessageReader<ThemeChanged>` + a frame countdown.

**Dev IPC.** `{"action":"activate_project","project":"…"}` brings a
project into view (dev panes go to **Recursion**, never the user's active
project). `close_project_panes` now takes an optional `titles: [..]`
filter (and `jimctl close --title T`) so a SINGLE pane can be removed
(e.g. a duplicate) instead of all of a kind. `df-dashboard.sh
[Project] combined` spawns the blog graphs + one timeline together.

Per-instance **params injection is done** (the `ScriptWidgetConfig.params`
→ funct `params` plumbing above), so nodes are configured at spawn, not
hardcoded. A declarative dashboard *file* (vs. the imperative
`df-dashboard.sh`) and a visual node-and-edge editor are the natural next
steps; both just emit `spawn_widget` calls with `params`.

**funct authoring notes (learned building Phase 0):**
- `type` is a reserved keyword → the column data-type field is `ctype`.
- Top-level `let NAME = …` with an UPPERCASE identifier is parsed as a
  constructor *pattern* and faults ("let pattern did not match"). Use
  lowercase for constants.
- No trailing comma in function-call argument lists (record/list
  literals allow them).
- `import { a, b } from "df"` gives unqualified names; `import "df"`
  gives `df.a`. Module root at runtime is `~/.jim/widgets/` (flat), so
  shared modules like `df.ft` must live directly there.
- Pre-flight any script offline with `funct run <file.ft>` (installable
  via `cargo install funct`) — it compiles + resolves imports without
  the GUI. Pure logic is testable the same way.
- **funct `/` is INTEGER division for Int/Int** (`740/1971 == 0`). JSON
  numbers parse to Int, so any ratio (`v / max`, time-scale
  `(x-min)/span`) silently truncates to 0 for everything below the max —
  the "flat lines + one bump" bug. Always make a divisor Float:
  `col_max`/`col_min` return `mx * 1.0`. Reach for this first when a chart
  looks collapsed.
- Time series position on a real scale by a numeric x (epoch-ms
  `bucket_start_ms`), NOT a per-bucket index or the `date` string (which
  repeats across hourly buckets). `df.ms_to_date(ms)` formats axis labels.
- A canvas widget must stay canvas across *all* states — if it returns a
  flow `vstack` for "waiting"/"error" then a `canvas` once data arrives,
  the flow entities strand under the chart. Render banners as canvas too.
- Widget snapshots are STATE-ONLY (the `state` atom's data), not the whole
  VM. On spawn the worker re-evaluates the *current* source + modules, then
  rehydrates the data — so hot-reloaded code (incl. shared `df.ft`) takes
  effect across restarts. Editing an imported module hot-swaps into every
  importer via `ReloadModule`→`vm.reload_module`. (Snapshotting execution
  instead would restore stale baked-in code and silently break hot-reload.)
- Canvas text now gets per-glyph font fallback too (the reconcile splits
  runs inline — see the Unicode note above), so canvas labels can use ANY
  glyph, same as flow text (`Text`/`Button`/`Badge`). The old "covered
  glyphs only" rule is retired. A line chart can be drawn without a Path
  primitive: one
  rotated Canvas `rect` per segment (`atan2`/`sqrt` are in the prelude).

**Phase 1 — vectors.** Add §8a `Polyline`/`Path` `CanvasItem` +
tessellation + scale/tick helpers → `view.line` / `view.area`. Add
`xf.groupby` / `xf.bucket` / `xf.split`.

**Phase 2 — GPU.** Add §8b Glaze data channel → shader-rendered
line/area/heatmap. Add `xf.join` / `xf.pivot`.

**Phase 3 — editor.** Native `http_get`, op registry surfaced as a
palette, visual node-and-edge editor writing the dashboard file.

### Open questions to settle before Phase 1

1. **Tessellation dependency** — pull in `lyon` for stroke/fill, or
   hand-roll a miter stroker? (Affects build weight.)
2. **Big datasets** — at what row count do we stop passing whole
   snapshots on the bus and add a shared-store handle? (Measure first;
   don't pre-optimize.)
3. **Glaze data upload** — uniform array (capped, simple) vs data
   texture (uncapped, default). Leaning texture.
4. **Adapter ownership** — do per-endpoint adapters live in `xf.adapt`'s
   script, or does each `source.http` preset also declare its adapter so
   `xf.adapt` stays generic? (Leaning: preset declares its shape.)
