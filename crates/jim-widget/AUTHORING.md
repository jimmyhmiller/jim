# Authoring widgets

A *widget* is a pane that renders a retained UI tree (an `Element`) and
reacts to events. There are **two ways to host one**, and they share the
same `Element` vocabulary and the same set of interactions. They differ
only in where the code runs and how events are delivered:

| | In-process funct | Subprocess |
|---|---|---|
| Code | a `.ft` script in `~/.jim/widgets/` | any program speaking NDJSON on stdio |
| Runs on | a worker thread inside the app | its own OS process |
| Event delivery | calls into named script **handlers** (`on_click`, …) | one `HostEvent` JSON line per event on stdin |
| Frame delivery | script `render()` returns an `Element` | program writes a `frame` JSON line on stdout |
| Hot reload | yes (file watcher re-parses) | restart the process |
| Use it for | small, live-editable UI; the default | heavier logic, other languages, isolation |

Both paths produce the **exact same `Element` tree** (see
`src/protocol.rs` for the full catalog) and expose the **exact same
interactions**. The tables below line the two event models up so there's
no ambiguity about which interaction reaches your code as what.

---

## The event model

This is the part that has bitten people: **UI events and the Claude Code
event bus are different things.** A name like "on_event" sounds like it
means "any event", but the bus and the UI are separate channels. Keep
them straight:

- **UI interaction** — the user clicked a button, flipped a toggle,
  picked a tab, typed in an input. These come from *this widget's own
  rendered elements*.
- **Claude Code bus** — `pre_tool_use`, `user_prompt_submit`, `stop`,
  etc., mirrored from the central hook bus. *Every* widget sees *every*
  bus event; filter by `kind` and `payload.cwd`.
- **Widget↔widget bus** — control messages widgets send *each other*
  (`emit` / `on_message`). Scoped to one editor project. This is a
  THIRD, separate channel — not UI, not the Claude bus. See
  "[The widget↔widget bus](#the-widgetwidget-bus)" below.

### funct handlers ↔ subprocess `HostEvent`

| Interaction | funct handler | Subprocess `HostEvent` (`event` field) |
|---|---|---|
| Button / `ListItem` press | `on_click(x, y, shift, cmd, id)` | `click` `{id}` |
| Press on empty space | `on_click(x, y, shift, cmd, "")` | (n/a — no target) |
| `Toggle` flipped | `on_toggle(id, checked)` | `toggle` `{id, checked}` |
| `Tabs` selection | `on_tab_select(id, tab)` | `tab-select` `{id, tab}` |
| `Input` focus / blur | `on_input_focus(id, focused)` | `input-focus` `{id, focused}` |
| `Input` edited | `on_input_change(id, value)` | `input-change` `{id, value}` |
| `Input` Enter | `on_input_submit(id, value)` | `input-submit` `{id, value}` |
| drag / release | `on_drag(x, y)` / `on_release(x, y)` | (funct only) |
| hover (x=inf on leave) | `on_hover(x, y)` | (funct only) |
| nav key, no input focused | `on_key(key)` | (funct only) |
| pane resized | `on_resize(w, h)` | `resize` `{width, height}` |
| per frame, while animating | `on_frame(dt)` | `tick` `{dt}` |
| **Claude Code bus** | **`on_bus(kind, payload)`** | `claude-event` `{kind, payload}` |
| **Widget↔widget bus** | **`on_message(topic, payload, sender)`** | `message` `{topic, payload, sender}` |
| lifecycle: state setup | `on_init()` | `init` `{width, height, state}` |
| lifecycle: side effects | `on_start()` | runs every start (fresh/restart/hot-reload) AFTER state is rehydrated — put fetches, `proc_spawn`, `set_animating`, bus subscribes here |
| lifecycle: closing | (worker `Shutdown`) | `close` |

`checked`, `tab`, and `value` are all computed host-side and handed to
you ready to use — `checked` is already the *new* value, you don't invert
it; `value` is the full new string, not a delta.

> `on_bus` was historically named `on_event`. That name is still
> accepted as a fallback but is deprecated, precisely because it implied
> "all events" and led authors to expect UI events there. Use `on_bus`.

## The widget↔widget bus

Several small widgets can act as one app — an editor pane, a results
pane, a schema browser — by sending each other control messages. This is
a general publish/subscribe channel, **separate from the Claude bus**
(`on_bus`) and from UI events.

### Publish

```funct
emit("sql.run", #{ sql: state.query });   // native value — host serializes
emit("schema.changed");                    // payload-less signal
```

`emit(topic, payload)` is fire-and-forget. `payload` is any native funct
value (map `#{…}`, array, string, number, bool) — the **host** encodes
it, so you never touch JSON in a script. The message is broadcast to
every widget **in the same editor project** (panes in other projects
never see it).

### Receive

```funct
fn on_message(topic, payload, sender) {
    if sender == my_id() { return; }       // ignore echoes of our own emits
    if topic == "sql.run" {
        run(payload.sql);
    }
}
```

Delivery is **pushed** — the host wakes your worker and calls
`on_message` directly. You do **not** need `set_animating` / `on_frame`;
the bus is fully event-driven. `sender` is the publishing widget's id
(`"jimctl msg"` for the CLI); compare it to `my_id()` to skip your own
messages, or use it to address a targeted reply (e.g. a topic naming the
sender).

### Retained messages (late joiners)

A pane that opens *after* a message was sent would miss it. For state
that late joiners need — the current DB connection, the current query —
use `emit_retained`:

```funct
emit_retained("conn.state", #{ host: "localhost", ok: true });
```

The host keeps the **last** retained value per topic and redelivers it to
each widget once, on init. So a results pane opened later immediately
learns the current connection without asking anyone. Retain is in-memory
only (it does not survive an app restart).

### Subprocess widgets

Same model over NDJSON: publish with `WidgetMsg::Emit { topic, payload,
retain }` on stdout; receive `HostEvent::Message { topic, payload,
sender }` on stdin.

### From the shell — `jimctl msg`

```sh
jimctl msg emit --project datalog-db --topic sql.run --json '{"sql":"select 1"}'
jimctl msg emit --project datalog-db --topic conn.state --json '{"ok":true}' --retain
jimctl msg tail --project datalog-db        # follow the bus live (one JSON line each)
```

Handy for driving a widget from a `proc_spawn`ed child or verifying flow
without the GUI. `--project` takes a project name or `active`. Messages
from the CLI arrive with `sender = "jimctl msg"`.

### Suggested topic conventions

Dotted topic names keyed by concern, e.g. for a SQL IDE:

| topic | payload | direction |
|---|---|---|
| `sql.run` | `{sql}` | editor → results: execute |
| `sql.result` | `{columns, rows, error, ms}` | results → *: a query finished |
| `sql.set_editor` | `{sql}` | history → editor: load text |
| `schema.changed` | `{}` | after DDL → browser: refresh |
| `conn.state` | `{host, ok}` *(retained)* | conn → late joiners |

Keep payloads small — the bus carries **control signals** (~kilobytes),
not bulk data. Big result sets stay where they were produced (or go
through the datalog DB); the bus just says "ready".

### Single-line vs multi-line input

- **`Input`** is single-line. `Enter` fires `on_input_submit`.
- **`TextArea`** is multi-line: `Enter` inserts a newline and the box is
  `rows` lines tall (default 4). Submit (`on_input_submit`) is
  **Cmd/Ctrl+Enter** — the usual "run query" gesture. Arrows move the
  caret across lines; Home/End are line-aware. Hard newlines only (no
  soft wrap).

Both emit the *same* `on_input_change` / `on_input_submit` /
`on_input_focus` handlers (subprocess: `input-change` / `input-submit` /
`input-focus`), with `value` carrying the full string (newlines and
all). So a query box is just:

```funct
#{ type: "textarea", id: "query", value: state.q, rows: 6,
   placeholder: "SELECT …" }

fn on_input_change(id, value) { state.q = value; request_render(); }
fn on_input_submit(id, value) { run_query(value); }   // Cmd/Ctrl+Enter
```

### Tables

`Element::Table` renders a header row + data rows on a grid. Columns
size to their content (capped, then the cell text wraps) unless you give
an explicit `width`; set per-column `align` for right-aligned numbers.
`zebra` stripes alternate rows.

```funct
#{ type: "table", zebra: true,
   columns: [
     #{ header: "id",    width: 48.0, align: "end" },
     #{ header: "name" },
     #{ header: "role" },
     #{ header: "score", width: 70.0, align: "end" },
   ],
   rows: [
     ["1", "Ada Lovelace", "Engineer", "98"],
     ["2", "Alan Turing",  "Researcher", "91"],
   ] }
```

Cells are plain strings (row-major; a short row leaves later cells
empty). The table sizes to its content width rather than filling the
pane, so give wide columns an explicit `width` when you want a specific
layout.

Cells are drag-selectable by default (see "Selectable / copyable text"
below): the user drags across a cell's text to highlight a substring and
Cmd/Ctrl+C it — the "grab one value out of the results" workflow, without
a whole-table export. Pass `selectable: false` to disable.

```funct
#{ type: "table", zebra: true,
   columns: [ #{ header: "id", align: "end" }, #{ header: "email" } ],
   rows: [ ["42", "ada@example.com"] ] }   // drag across the email → Cmd+C
```

### Selectable / copyable text

Read-only text displays — `Element::Text`, `Element::Table` (per cell),
and `Element::Badge` — are **drag-selectable by default**. The user drags
across the rendered text to highlight a range (a translucent accent
band), then **Cmd/Ctrl+C copies the selected substring** to the system
clipboard. A plain click (no drag) clears the selection; only one
selection is active at a time. You don't have to do anything — a results
table or a value label is copyable out of the box.

Opt a specific element OUT with `selectable: false` — e.g. a label that's
part of a custom drag gesture you handle yourself:

```funct
#{ type: "text", value: "drag me", selectable: false }
```

Interactive elements are intentionally NOT selectable: `Button`, `Link`,
`Tabs`, `Toggle`, and `Input`/`TextArea`. A press there fires the
element's action (and inputs own their caret/selection), so auto-select
would fight those gestures. Canvas widgets (`Element::Canvas`) are
unaffected — they render outside this text path and keep their own drag
handling.

Selection is handled entirely host-side — no `on_click`/event
round-trip, and it doesn't blur a focused input.

Scope: selection covers a **single run** — one `Text`, one table cell, or
one badge. Dragging across multiple runs, or a rectangular/multi-row
table span, isn't modeled yet. Mapping is tuned for single-line values; a
wrapped/multi-line `Text` selects approximately.

### Focused-input ownership

While an `Input` or `TextArea` is focused, the **host owns** the live
edit buffer and the blinking caret (`WidgetInputFocus`). That means:

- Typing echoes instantly — you do **not** need to round-trip a frame to
  show keystrokes.
- You get `on_input_change` after each edit and `on_input_submit` on
  Enter; react to those (e.g. run a search, store the value in `state`).
- The element's `value` you render is only the *initial* / committed
  value; the host substitutes the live buffer while focused.
- Nav keys (arrows / Home / End) drive the caret and do **not** fire
  `on_key` while an input is focused.

---

## Writing a funct widget

Drop a `.ft` file in `~/.jim/widgets/`. The file watcher
re-parses on save. The top-level body runs **once per load** (initialize
`state`, define handler `fn`s). All handlers are optional — define only
what you need.

```funct
// counter.ft
if !("n" in state)    { state.n = 0; }
if !("dark" in state) { state.dark = false; }
if !("q" in state)    { state.q = ""; }

fn on_init() { request_render(); }

fn on_click(x, y, shift, cmd, id) {
    if id == "inc" { state.n += 1; }
    if id == "dec" { state.n -= 1; }
    request_render();
}

fn on_toggle(id, checked)     { if id == "dark" { state.dark = checked; } request_render(); }
fn on_input_change(id, value) { if id == "search" { state.q = value; } request_render(); }
fn on_input_submit(id, value) { if id == "search" { run_search(value); } }

fn on_bus(kind, payload) {
    // Claude Code bus — NOT UI events.
    if kind == "stop" { state.n = 0; request_render(); }
}

fn render(w, h) {
    #{ type: "vstack", gap: 8.0, pad: 12.0, children: [
        #{ type: "text", value: "count: " + state.n },
        #{ type: "hstack", gap: 4.0, children: [
            #{ type: "button", id: "dec", label: "-" },
            #{ type: "button", id: "inc", label: "+" },
        ]},
        #{ type: "toggle", id: "dark", label: "Dark", checked: state.dark },
        #{ type: "input",  id: "search", value: state.q, placeholder: "search…" },
    ]}
}
```

### `state` and persistence

`state` is a `Map` in scope, persisted across restarts and hot reloads
(round-tripped to JSON into the pane snapshot). Mutate it in place.

### Scheduling renders

funct widgets are **event-driven** — there is no per-frame poll by
default. After a handler mutates state, call `request_render()` to redraw
once. For continuous animation, call `set_animating(true)` to start
receiving `on_frame(dt)`; `set_animating(false)` to stop (idle widgets
cost zero CPU).

### Driving a subprocess (event-driven)

Don't use `set_animating` + `proc_read`-in-`on_frame` to drain a child —
that busy-polls and pins the app at 60fps for the whole run. Instead the
subprocess reader pushes to two handlers:

| handler | when |
| --- | --- |
| `on_proc_output(handle, line)` | once per stdout line |
| `on_proc_exit(handle, code)`   | once when the child exits (`code` = exit status, or -1 if unknown) |

```funct
fn run_query(sql) {
    state.rows = [];
    state.proc = proc_spawn("datalog", ["--host", state.host, "query", sql]);
}
fn on_proc_output(handle, line) {
    if handle == state.proc { state.rows.push(line); }   // accumulate; no render yet
}
fn on_proc_exit(handle, code) {
    if handle == state.proc { state.done = true; request_render(); }  // render once, at the end
}
```

The worker wakes on each line (no polling); the app stays **reactive**
and only repaints when a handler calls `request_render()`. No
`set_animating` for I/O. `proc_read` / `proc_alive` still exist for
explicit polling / back-compat.

### Rich text and Markdown

`Element::Text` is a **single uniform run**. To style part of a wrapped
sentence — bold three words mid-paragraph — use `richtext`, which lays
its runs out as ONE wrapped block:

```funct
{ kind: "richtext", size: 14.0, color: "#cfd2d8", runs: [
    { value: "A paragraph with " },
    { value: "bold", weight: "bold" },
    { value: " and " },
    { value: "code", family: "mono", color: "#e2c08d" },
    { value: " that all wrap together." },
]}
```

Each run may set `size` / `weight` / `italic` / `color` / `family`;
anything it omits falls back to the element's defaults, then the theme.
The block's line height comes from its tallest run. An hstack of `Text`s
is *not* a substitute — each child wraps independently, so emphasis
mid-sentence breaks the line.

Don't hand-parse Markdown. **`md_parse(text)`** runs `markdown_core` —
the same parser the WYSIWYG editor uses — and returns blocks whose runs
drop straight into `richtext`:

```funct
for block in md_parse(text) {
    // block.kind: "paragraph" | "blank" | "heading" | "code-block"
    //           | "block-quote" | "list-item" | "thematic-break"
    // block.level (heading), block.ordered (list), block.lang (code),
    // block.indent, block.lines: [ [ run ] ]
    // run: { value, bold, italic, code, strike, link }
}
```

Syntax markers (`#`, `**`, fences, bullets, link brackets) are already
stripped — you render your own bullet and heading size from `kind`, and
decide what bold/code/link *look* like. `widgets/md_demo.ft` is a
complete worked example.

### Images

`Element::Image` scales a file into its layout box:

```funct
{ kind: "image", path: "~/shots/demo.png", fit: "contain",
  style: { width: "100%", height: "260" } }
```

`fit` is `"contain"` (default — scale down to fit, preserve aspect,
letterbox), `"cover"` (fill the box, preserve aspect, crop), or `"fill"`
(stretch). This is different from `style.background_image`, which always
anchors top-left and stretches — fine for a texture, wrong for a photo.
An image with no size from the flex tree claims a default minimum height
so it can't silently collapse to nothing.

### Drawing: canvas paths

`Canvas` children are absolutely positioned (`x`/`y` in pixels from the
canvas box's top-left, y DOWN) and every one of them **requires an `id`**
— omit it and the whole frame fails to deserialize, leaving the pane
blank. Alongside `rect` / `text` / `sprite` there is `path`, which draws
real vector geometry:

```funct
{ kind: "canvas", style: { width: "100%", height: "220" }, children: [
    // A filled area with a 25%-alpha wash, plus the line on top.
    { kind: "path", id: "area", d: "M 20 200 L 20 120 C 60 90 100 170 140 110 L 140 200 Z",
      fill: "#3c7ae040" },
    { kind: "path", id: "line", d: "M 20 120 C 60 90 100 170 140 110",
      stroke: "#4c8fd8", stroke_width: 2.0, cap: "round", join: "round" },
] }
```

`d` is SVG path data: `M L H V C S Q T A Z` and their relative lowercase
forms, so arcs (donuts) and beziers (curved edges) are available, not
just polylines. Malformed data logs a clear error naming the offending
byte and skips that one item — it never draws something else instead.

Give a path `fill`, `stroke`, or both (fill paints first). A translucent
color is composited on the CPU against `bg` — which defaults to the
theme's `pane_bg` — because Bevy 0.19 will not render a blend-mode mesh
through a per-pane camera. Set `bg` explicitly when a path sits on top of
some other fill rather than on the bare pane.

Before `path` existed, chart widgets drew lines as rows of rotated
`rect`s (`df_view_line.ft`); that trick is no longer necessary.
`cargo run --release -p jim_widget --bin path_probe` renders fills,
strokes, arcs and an alpha wash in a real pane and checks the resulting
pixels, which is how we know this path survives the pane camera.

### Revealing panes by name (pane groups)

A widget can show and hide *other* panes — a dashboard of live terminals,
charts, a diff view — without spawning anything. Put the panes in a named
group once, then publish the group name:

```funct
emit_retained("pane.groups", { show: ["repo-health"] })   // reveal
emit_retained("pane.groups", { show: [] })                // hide everything
```

The payload states the **complete** visible set, so revealing a different
group hides the previous one; you never send an explicit "hide".

Membership is assigned from the shell and persists with the pane:

```sh
jimctl group assign --project P --name repo-health --title "Repo Hub" --title "Diff"
jimctl group show --name repo-health     # by hand, while building
jimctl group list --project P            # verify the wiring
```

Grouped panes stay **alive** while hidden — a terminal keeps its shell, a
widget keeps its fetched data — so revealing is instant. That's the point:
a presentation deck can cut to a live dashboard mid-demo without a
respawn. See `crates/jim-app/src/pane_groups.rs`.

### Styling with Glaze

Inline `style:` records are the escape hatch, not the default. For
anything with more than one styled element, write a **Glaze stylesheet**
(`.glz`) and ask it for styles by name — that is what the language is
for: reuse, variants, pseudo-states, responsive `when` blocks, animated
shader layers, and retuning the whole widget by editing one file. The
full language is documented in `docs/GLAZE.md`.

```funct
// compiled once per load — so saving EITHER file hot-reloads the widget
glaze_load_file(host_env("HOME") + "/.jim/widgets/mywidget.glz")

fn render(w, h) = {
    kind: "vstack", style: glaze("page"), children: [
        // `when vw < 420 { … }` fires off the width you pass in
        { kind: "vstack", style: glaze_at("card", {}, [], w, h), children: [ … ] },
        // static variant params — the branch folds at resolve time
        { kind: "text", value: "3 failed", style: glaze("pill", { intent: "danger" }) },
        // discrete pseudo-state: pick the `:hover` plan for the hot row
        { kind: "hstack", style: glaze("row", {}, ["hover"]), children: [ … ] },
    ]
}
```

| host fn | what it gives you |
|---|---|
| `glaze_load(src)` | compile literal Glaze source |
| `glaze_load_file(path)` | read + compile a `.glz` (`~` expanded); registers it for hot reload |
| `glaze_loaded()` | is a sheet loaded? (for a widget that wants a fallback) |
| `glaze_styles()` | the style names in the sheet |
| `glaze(name[, variant[, states]])` | a `Style` record for `style:` |
| `glaze_at(name, variant, states, vw, vh)` | same, with `when` breakpoints resolved at that size |
| `glaze_slot(name, component[, variant[, states]])` | the typed per-slot style a compound element wants — `toggle` `select` `tabs` `bar` `stepper` `radio` `checkbox` `slider` `table` `toast` `popover` `dialog` `tooltip` |
| `glaze_token(name)` | one token's value — a colour as `"#rrggbb"`, a length/number as a number |
| `glaze_tokens()` | the token names in the sheet |

**Styles are box-only.** `fill`, `radius`, `border`, `shadow`, `pad`,
sizing — but not a font size or a text colour, because `Style` has no
typography. Read those from tokens instead, so a sheet stays the single
source of truth rather than splitting a design system between a `.glz`
and a pile of literals in the `.ft`:

```funct
{ kind: "text", value: title,
  size: glaze_token("size_h1"), color: glaze_token("fg") }
```

`variant` is a record of static params (`{ intent: "danger" }`) compared
as strings inside the sheet, so numbers and bools are fine too. `states`
is a list like `["hover"]`, or a bare `"hover"`.

**Shader layers work.** A `shader {}` / `overlay shader {}` block in the
sheet compiles to WGSL and arrives as a live material on the element — a
funct widget gets animated gradients and glows without touching the GPU
path.

**Everything faults loudly.** A parse error, an unknown token, a typo'd
style name, or an unknown component slot raises a funct fault carrying
the Glaze message. Nothing silently falls back to an unstyled element,
because that reads as a layout bug and costs an hour to trace.

Note *when* each error lands: `glaze_load*` only parses, and styles
resolve lazily, so an unresolved token or a bad expression inside a style
you never ask for stays quiet until something calls `glaze("that-one")`.
The `check` example resolves every style in the sheet, which is the point
of running it.

Two gotchas worth knowing before you write a sheet:

- **One property per line.** Glaze separates properties by newline, so
  `pad 8px radius 8px` on one line parses as a three-argument `pad` and
  fails at resolve time with "`pad` takes 1, 2, or 4 lengths".
- **Check a sheet without running the app:**
  `cargo run -p glaze --example check -- mywidget.glz` parses it and
  resolves every style at a wide and a narrow viewport.

`widgets/glaze_demo.ft` + `widgets/glaze_demo.glz` are a working example
of all of the above, and are covered end-to-end by the tests in
`funct_widget.rs`.

### Function scoping gotcha

User-defined `fn`s are pure: they do **not** see top-level `const`s, and
only host-invoked handlers receive `state`. Helpers take what they need
as parameters. (See the funct fn-scoping notes in memory.)

### Host functions available to scripts

The Glaze surface (`glaze_load` / `glaze_load_file` / `glaze` /
`glaze_at` / `glaze_slot` / `glaze_styles` / `glaze_loaded`, see
"[Styling with Glaze](#styling-with-glaze)"), plus
`request_render`, `set_animating`, `time`, `rand`, `rand_int`,
`hash_str`, `host_env`, `host_log`, `clipboard_set`, `widget_asset`, the
widget↔widget bus `emit` / `emit_retained` / `my_id` (see "[The
widget↔widget bus](#the-widgetwidget-bus)"), the generic subprocess
primitives `proc_spawn` / `proc_write` / `proc_read` / `proc_alive` /
`proc_kill` (plus the push handlers `on_proc_output` / `on_proc_exit` —
see "Driving a subprocess"), and the JSON bridge `parse_json(s)` →
map/array/scalar (`()` on bad input) / `to_json(v)` → compact string,
for talking JSON protocols with subprocesses (see `style_lab.ft`).

---

## Writing a subprocess widget

Speak NDJSON on stdio: read one `HostEvent` per line on stdin, write
`frame` / `state` / `title` messages on stdout. The enum definitions
(`HostEvent`, `WidgetMsg`, `Element`, `Style`) and their exact JSON shape
live in `src/protocol.rs`. The same interaction table above applies —
just delivered as JSON lines instead of handler calls.

---

## Where the code lives

- `src/protocol.rs` — `Element` catalog, `Style`, `HostEvent`,
  `WidgetMsg`. The single source of truth for the UI vocabulary.
- `src/script_widget.rs` — the in-process funct host: worker thread,
  `HostToWorker` channel, handler dispatch, hot reload. The module-level
  doc has the handler table inline.
- `src/lib.rs` — the subprocess host (`WidgetIO`, NDJSON pump) plus the
  shared rendering, hit-testing, scroll, and focused-input typing that
  both paths use.
- `src/render.rs` / `src/layout.rs` — `Element` → Bevy sprites (Taffy
  flexbox layout).
