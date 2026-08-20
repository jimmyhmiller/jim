# Pads — live notebooks inside Jim

A pad is a notebook pane that a terminal writes into. An agent (or you) runs a
command; a cell appears in the pane as the command returns — charts, tables,
stat rows, diagrams, code, prose. It reads like a typeset report rather than a
dashboard, and it lives on the canvas with everything else, so nothing has to
leave the editor to be looked at.

```sh
jimctl pad new incident-42
jimctl widget --kind script_widget --project Recursion \
  --params '{"notebook":"incident-42"}' pad.ft

jimctl pad md "# Checkout latency spike"
jimctl pad stats '[{"label":"p95","value":"412ms","delta":"+180ms","up_is_good":false}]'
jimctl pad chart '{"type":"line","title":"p95 by region",
  "series":[{"name":"us-east","points":[["2026-08-01",120],["2026-08-02",141]]}]}'
jimctl pad table results.csv --title "Slow queries"
jimctl pad graph 'digraph { rankdir=LR; web -> api; api -> db [label="write"]; }'
jimctl pad callout warn "Rollout overlaps the spike window"
```

## How it works

```text
jimctl pad ── appends JSONL ──▶ ~/.jim/pads/<name>.jsonl
     │                                   │
     └── publishes `pad.<name>` ──▶  pad.ft re-reads the file and re-renders
         on the widget bus
```

The file is the data; the bus is only the doorbell. That split is deliberate:

- The bus carries control signals, and its retained store is in-memory only.
  A notebook has to survive a GUI restart and be complete for a pane opened an
  hour later, so the cells live on disk and the message just says "notebook X
  changed".
- Anything that can append a line can drive a pad — no session, no handshake.
- The doorbell is published on the GLOBAL bus channel (`project: None`), so a
  pad pane sees its own notebook's changes whichever project it sits in. A
  notebook is identified by name, not by project.

One JSON object per line; reduce them in order and you have the document:

```json
{"op":"update","id":"c3f9","ts":1786909149011,"cell":{"type":"markdown","source":"# Hi"}}
{"op":"remove","id":"c3f9"}
{"op":"clear"}
```

`update` on an unknown id appends, so a writer never has to know whether a cell
exists yet. Every add prints the cell's id, and reusing one with `--id`
replaces that cell in place — which is how a long-running agent keeps a single
progress figure current instead of appending a hundred of them:

```sh
jimctl pad stats --id migrated '[{"label":"Files migrated","value":12}]'
jimctl pad stats --id migrated '[{"label":"Files migrated","value":48}]'   # same cell
```

## Where the code lives

Everything the pane draws is funct, in `crates/jim-widget/widgets/`:

| file | what it does |
|---|---|
| `pad.ft` | the pane: resolves the notebook, reduces the log, dispatches cells, handles the doorbell |
| `pad_log.ft` | the record model and the reducer |
| `pad_fmt.ft` | numbers, dates, axis ticks — funct's `str` is not presentable |
| `pad_scale.ft` | chart geometry: domains, scales, bins, arcs, path data |
| `pad_chart.ft` | line / area / scatter / bar / donut / histogram |
| `pad_graph.ft` | DOT: tokenizer, parser, layered layout, drawing |
| `pad_cells.ft` | markdown, callout, code, table, stats, json, image, comment |
| `pad_theme.ft` | the palette and type scale, from the active jim theme |

The writer is `crates/jimctl/src/cmd_pad.rs` (`jimctl pad`).

Markdown and syntax highlighting are host natives — `md_parse` (the same
`markdown-core` the WYSIWYG editor uses) and `highlight` — so neither is
reimplemented in funct.

## Testing

The pure modules run under the standalone `funct` CLI, with no editor:

```sh
cd crates/jim-widget/widgets
funct pad_fmt_test.ft      # formatting, nice ticks, civil dates
funct pad_log_test.ft      # the reducer, malformed lines, update-in-place
funct pad_scale_test.ft    # scales, gaps, histogram edges, donut angles
funct pad_graph_test.ft    # DOT parsing, ranking, layout positions
```

`layout` takes its text measurement as a callback precisely so the graph tests
can run without the host's font metrics. Chart math that mislocates a point
and a diagram that connects the wrong boxes both look plausible on screen,
which is why they are tested rather than eyeballed.

Running a module directly (`funct pad_chart.ft`) is a syntax check — funct's
newline sensitivity and record-literal rules reject things that read fine.

## Drawing

Charts and diagrams are drawn with `CanvasItem::Path` (`jim-widget/src/vector.rs`):
SVG path data, tessellated with lyon, so areas, arcs and curves are available
and everything stays crisp as the canvas zooms. Two constraints are worth
knowing before touching that code:

- Paths render with an **opaque** material and flatten their own alpha against
  `bg` (default: the theme's `pane_bg`). Bevy 0.19 will not render a
  blend-mode `Mesh2d` through a per-pane camera at all.
- `cargo run --release -p jim_widget --bin path_probe` draws fills, strokes,
  arcs and an alpha wash in a real pane and checks the resulting pixels. Run it
  after any change to the path pipeline; it is how we know the above still
  holds.

## House style

The look is not configurable, and these are the reasons why:

- One muted color for a single series. The categorical palette only appears
  when there are series to tell apart.
- Line charts label their ends directly; a legend is a fallback, not the
  primary channel. Color should never be the only way to read a chart.
- Bars start at zero and round only the end that carries the data.
- A null `y` is a gap in the line, not a zero — a dropped metric breaks the
  line rather than diving it to the floor.
- Space separates cells, not borders. Tables are set with a heavy top rule,
  mono data, no boxes.

The CLI refuses input that would draw something misleading — more than eight
series, a stacked bar chart mixing signs, mixed x-value kinds in one chart —
with an explanation, at the door rather than in the pane.

## Not done yet

- Export (md / html / pdf). The standalone `pad` desktop app does this; the
  in-editor pane does not.
- Comment threads are rendered but not composed — cells of type `comment`
  display, and nothing writes them yet.
- Horizontal bars, subgraphs/clusters in DOT. Both report a clear error rather
  than drawing something approximate.
- Virtualization. A notebook of a few dozen cells is fine; a few thousand
  would want `virtual.ft`.
