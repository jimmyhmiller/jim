#!/usr/bin/env python3
"""Run pad's Element-producing funct modules outside the editor.

`funct pad_cells.ft` only proves a module PARSES. The failure that actually
bites is a fault at render time — a bad field, a Unit where a record was
expected — because the host then rejects the whole frame and the pane goes
blank with one line in the log. This drives the real render functions against
stub host natives and checks the Elements that come back.

The stubs stand in for natives the standalone `funct` CLI doesn't have
(`md_parse`, `highlight`, `measure_text`, `theme_get`, …). Modules are
concatenated with their `import` lines and `export` keywords stripped, since
the CLI has no way to register natives into an imported module.

    python3 scripts/pad-render-check.py
"""

import pathlib
import re
import subprocess
import sys
import tempfile

WIDGETS = pathlib.Path(__file__).resolve().parent.parent / "crates/jim-widget/widgets"

# Host natives the pad modules call, faked well enough to exercise the code
# paths that use them.
STUBS = r"""
// ---- stub host surface (see scripts/pad-render-check.py) ----
fn theme_get(token) {
    if token == "font_family_body" { return "serif" }
    if token == "font_family_heading" { return "serif" }
    if token == "font_family_mono" { return "mono" }
    "#8090a0"
}
fn measure_text(s, size) = to_float(len(s)) * size * 0.55
fn char_width(size) = size * 0.55
fn host_env(name) = "/tmp"
let params = {}

// A notebook with one of several cell types, plus a line that can't be read —
// the pane has to report that one and still draw the rest.
fn stub_log() {
    let md = "{\"op\":\"update\",\"id\":\"a\",\"cell\":{\"type\":\"markdown\",\"source\":\"# Title\"}}"
    let st = "{\"op\":\"update\",\"id\":\"b\",\"cell\":{\"type\":\"stats\",\"items\":[{\"label\":\"p95\",\"value\":412}]}}"
    let ch = "{\"op\":\"update\",\"id\":\"c\",\"cell\":{\"type\":\"chart\",\"spec\":{\"type\":\"bar\",\"categories\":[\"a\",\"b\"],\"series\":[{\"values\":[3,5]}]}}}"
    let tb = "{\"op\":\"update\",\"id\":\"d\",\"cell\":{\"type\":\"table\",\"columns\":[\"x\"],\"rows\":[[1]]}}"
    md + "\n" + st + "\n" + ch + "\n" + tb + "\nthis line is not json\n"
}

fn read_file(path) {
    if ends_with(path, "state.json") {
        return { ok: true, text: "{\"current\":\"stub\"}", error: "" }
    }
    if ends_with(path, "stub.jsonl") { return { ok: true, text: stub_log(), error: "" } }
    { ok: false, text: "", error: "no such file" }
}
fn request_render() = 0
fn my_id() = "stub"

// A markdown parse just rich enough to exercise every block branch.
fn md_parse(text) {
    let mut blocks = []
    for line in split(text, "\n") {
        let t = trim(line)
        if t == "" { blocks = push(blocks, { kind: "blank", lines: [] })  continue }
        if starts_with(t, "# ") {
            blocks = push(blocks, { kind: "heading", level: 1,
                                    lines: [[{ value: slice(t, 2, len(t) - 2), bold: true,
                                               italic: false, code: false, link: false }]] })
            continue
        }
        if starts_with(t, "- ") {
            blocks = push(blocks, { kind: "list-item", ordered: false, indent: 0,
                                    lines: [[{ value: slice(t, 2, len(t) - 2), bold: false,
                                               italic: false, code: false, link: false }]] })
            continue
        }
        if starts_with(t, "> ") {
            blocks = push(blocks, { kind: "block-quote",
                                    lines: [[{ value: slice(t, 2, len(t) - 2), bold: false,
                                               italic: false, code: false, link: false }]] })
            continue
        }
        if t == "---" { blocks = push(blocks, { kind: "thematic-break", lines: [] })  continue }
        blocks = push(blocks, { kind: "paragraph",
                                lines: [[{ value: t, bold: false, italic: false,
                                           code: contains(t, "`"), link: false }]] })
    }
    blocks
}

fn highlight(code, lang) {
    let mut out = []
    for line in split(code, "\n") {
        out = push(out, [{ text: line, kind: if lang == "" { "default" } else { "keyword" } }])
    }
    out
}
"""

# Each check: the modules to concatenate (dependency order) and a driver.
CHECKS = {
    "cells": (
        ["pad_fmt.ft", "pad_log.ft", "pad_theme.ft", "pad_cells.ft"],
        r"""
fn el_kind(e) = unwrap_or(get(e, "kind"), "")

fn expect_element(what, e) {
    if typeof(e) != "Record" { fail("${what}: not a record, got ${typeof(e)}") }
    if el_kind(e) == "" { fail("${what}: element has no `kind`") }
    check_ids(what, e)
}

// Every canvas child needs an `id` — one missing id makes the host reject the
// WHOLE frame, which is the blank-pane failure this check exists to catch.
fn check_ids(what, e) {
    if el_kind(e) == "canvas" {
        for it in unwrap_or(get(e, "children"), []) {
            let id = unwrap_or(get(it, "id"), "")
            if typeof(id) != "Str" or id == "" {
                let k = unwrap_or(get(it, "kind"), "?")
                fail("${what}: a canvas ${k} item has no id")
            }
        }
    }
    for c in unwrap_or(get(e, "children"), []) {
        if typeof(c) == "Record" and unwrap_or(get(c, "kind"), "") != "" { check_ids(what, c) }
    }
}

fn run() {
    expect_element("markdown", render_markdown({ source: "# Title\n\nA paragraph.\n\n- one\n- two\n\n> quoted\n\n---" }))
    expect_element("empty markdown", render_markdown({ source: "" }))
    expect_element("callout", render_callout({ level: "warn", source: "careful" }))
    expect_element("callout info default", render_callout({ source: "hi" }))
    expect_element("code", render_code({ source: "let x = 1\nlet y = 2", language: "rust", title: "main.rs" }))
    expect_element("code untitled", render_code({ source: "plain" }))
    expect_element("table", render_table({ columns: ["region", "p95"],
                                           rows: [["us-east", 412], ["eu-west", 180]],
                                           title: "Slow queries" }))
    expect_element("table no columns", render_table({ columns: [], rows: [] }))
    expect_element("stats", render_stats({ items: [
        { label: "p95", value: "412ms", delta: "+180ms", up_is_good: false },
        { label: "count", value: 128400, spark: [1, 4, 2, 8, 5] },
    ] }))
    expect_element("stats empty", render_stats({ items: [] }))
    // A record literal can't have a string key, so the open-path set is built.
    expect_element("json", render_json({ value: { a: [1, 2], b: "x" }, title: "config" },
                                       "c1", assoc({}, "c1", true)))
    expect_element("json collapsed", render_json({ value: { a: 1 } }, "c1", {}))
    expect_element("image", render_image({ src: "/tmp/x.png", caption: "a shot" }))
    expect_element("image missing src", render_image({}))
    expect_element("comment", render_comment({ author: "jim", body: "looks right", target: "c1" }))
    println("pad_cells render: ok")
}
run()
""",
    ),
    "chart": (
        ["pad_fmt.ft", "pad_log.ft", "pad_theme.ft", "pad_scale.ft", "pad_cells.ft", "pad_chart.ft"],
        r"""
fn canvas_of(e) {
    if unwrap_or(get(e, "kind"), "") == "canvas" { return e }
    for c in unwrap_or(get(e, "children"), []) {
        if typeof(c) != "Record" { continue }
        let found = canvas_of(c)
        if typeof(found) == "Record" { return found }
    }
    0
}

fn expect_chart(what, spec) {
    let e = render_chart({ spec: spec }, 640.0)
    if typeof(e) != "Record" { fail("${what}: not a record") }
    let cv = canvas_of(e)
    if typeof(cv) != "Record" { fail("${what}: produced no canvas") }
    let kids = unwrap_or(get(cv, "children"), [])
    if len(kids) == 0 { fail("${what}: the canvas is empty") }
    let mut seen = {}
    for it in kids {
        let id = unwrap_or(get(it, "id"), "")
        if typeof(id) != "Str" or id == "" { fail("${what}: a canvas item has no id") }
        // Duplicate ids collapse two marks onto one entity in the host's diff.
        if has(seen, id) { fail("${what}: duplicate canvas id ${id}") }
        seen = assoc(seen, id, true)
        if unwrap_or(get(it, "kind"), "") == "path" {
            let d = unwrap_or(get(it, "d"), "")
            if d == "" { fail("${what}: a path item has empty data") }
            if not starts_with(d, "M") { fail("${what}: path data must start with M: ${d}") }
            if contains(d, "NaN") or contains(d, "inf") { fail("${what}: path data has ${d}") }
        }
    }
}

fn expect_refused(what, spec) {
    let e = render_chart({ spec: spec }, 640.0)
    if typeof(canvas_of(e)) == "Record" { fail("${what}: should have been refused, but drew a chart") }
}

// `type` is a funct keyword, so a spec can't be written as a record literal —
// and parsing the JSON is what the widget actually does anyway.
fn j(src) {
    let v = unwrap_or(json_parse(src), 0)
    if typeof(v) != "Record" { fail("test spec is not valid JSON: ${src}") }
    v
}

fn run() {
    expect_chart("line", j("{\"type\":\"line\",\"title\":\"p95\",\"series\":[{\"name\":\"us-east\",\"points\":[[\"2026-08-01\",120],[\"2026-08-02\",141]]}]}"))
    expect_chart("line with a gap", j("{\"type\":\"line\",\"series\":[{\"points\":[[1,10],[2,null],[3,4]]}]}"))
    expect_chart("two series", j("{\"type\":\"line\",\"series\":[{\"name\":\"a\",\"points\":[[1,1],[2,2]]},{\"name\":\"b\",\"points\":[[1,3],[2,1]]}]}"))
    expect_chart("area", j("{\"type\":\"area\",\"series\":[{\"points\":[[\"a\",3],[\"b\",5],[\"c\",2]]}]}"))
    expect_chart("scatter", j("{\"type\":\"scatter\",\"series\":[{\"points\":[[1,2],[3,4]]}]}"))
    expect_chart("bar", j("{\"type\":\"bar\",\"categories\":[\"a\",\"b\"],\"series\":[{\"values\":[3,5]}]}"))
    expect_chart("grouped bars", j("{\"type\":\"bar\",\"categories\":[\"a\",\"b\"],\"series\":[{\"values\":[3,5]},{\"values\":[4,1]}]}"))
    expect_chart("stacked bars", j("{\"type\":\"bar\",\"stacked\":true,\"categories\":[\"a\",\"b\"],\"series\":[{\"values\":[3,5]},{\"values\":[4,1]}]}"))
    expect_chart("negative bars", j("{\"type\":\"bar\",\"categories\":[\"a\",\"b\"],\"series\":[{\"values\":[-3,5]}]}"))
    expect_chart("donut", j("{\"type\":\"donut\",\"slices\":[{\"label\":\"a\",\"value\":3},{\"label\":\"b\",\"value\":1}]}"))
    expect_chart("histogram", j("{\"type\":\"histogram\",\"values\":[1,2,2,3,5,8,8,9]}"))
    expect_chart("single point", j("{\"type\":\"line\",\"series\":[{\"points\":[[1,5]]}]}"))
    expect_chart("flat series", j("{\"type\":\"line\",\"series\":[{\"points\":[[1,5],[2,5]]}]}"))

    expect_refused("no type", j("{}"))
    expect_refused("unknown type", j("{\"type\":\"sankey\"}"))
    expect_refused("no series", j("{\"type\":\"line\",\"series\":[]}"))
    expect_refused("mixed x kinds", j("{\"type\":\"line\",\"series\":[{\"points\":[[\"2026-08-01\",1],[\"east\",2]]}]}"))
    expect_refused("horizontal bars", j("{\"type\":\"bar\",\"horizontal\":true,\"categories\":[\"a\"],\"series\":[{\"values\":[1]}]}"))
    expect_refused("stacked mixed signs", j("{\"type\":\"bar\",\"stacked\":true,\"categories\":[\"a\"],\"series\":[{\"values\":[1]},{\"values\":[-1]}]}"))
    expect_refused("all-zero donut", j("{\"type\":\"donut\",\"slices\":[{\"label\":\"a\",\"value\":0}]}"))
    println("pad_chart render: ok")
}
run()
""",
    ),
    "pane": (
        ["pad_fmt.ft", "pad_log.ft", "pad_theme.ft", "pad_scale.ft", "pad_cells.ft",
         "pad_chart.ft", "pad_graph.ft", "pad.ft"],
        r"""
fn count_kinds(e, acc0) {
    let mut acc = acc0
    let k = unwrap_or(get(e, "kind"), "")
    if k != "" { acc = assoc(acc, k, unwrap_or(get(acc, k), 0) + 1) }
    for c in unwrap_or(get(e, "children"), []) {
        if typeof(c) == "Record" { acc = count_kinds(c, acc) }
    }
    acc
}

fn run() {
    // `render` is the whole pane: resolve the notebook, reduce the log the
    // stub read_file hands back, and draw every cell.
    let frame = render(760.0, 900.0)
    if typeof(frame) != "Record" { fail("render returned ${typeof(frame)}") }
    if unwrap_or(get(frame, "kind"), "") != "vstack" { fail("the frame should be a vstack") }
    let kinds = count_kinds(frame, {})
    // The stub log has a markdown, a stats, a chart and a table cell, so the
    // frame must contain the elements each of those produces.
    if unwrap_or(get(kinds, "canvas"), 0) < 1 { fail("no chart canvas in the frame") }
    if unwrap_or(get(kinds, "table"), 0) < 1 { fail("no table in the frame") }
    if unwrap_or(get(kinds, "richtext"), 0) < 1 { fail("no rendered markdown in the frame") }

    // A malformed line is reported in the pane rather than dropping the
    // notebook, and the good cells still draw.
    let s = cur_state()
    if len(s.errors) != 1 { fail("expected exactly one reported bad line, got ${len(s.errors)}") }
    if len(s.cells) != 4 { fail("expected 4 cells, got ${len(s.cells)}") }

    // The doorbell only fires for this notebook's topic.
    on_message("pad.other-notebook", {}, "test")
    on_message("pad.stub", {}, "test")

    // Clicking a json disclosure toggles it and nothing else.
    on_click("json:stub/x")
    if not has(cur_state().open, "stub/x") { fail("a json click should open that path") }
    on_click("json:stub/x")
    if has(cur_state().open, "stub/x") { fail("a second click should close it") }
    on_click("not-a-json-id")

    println("pad pane render: ok")
}
run()
""",
    ),
    "graph": (
        ["pad_fmt.ft", "pad_log.ft", "pad_theme.ft", "pad_scale.ft", "pad_cells.ft", "pad_graph.ft"],
        r"""
fn canvas_of(e) {
    if unwrap_or(get(e, "kind"), "") == "canvas" { return e }
    for c in unwrap_or(get(e, "children"), []) {
        if typeof(c) != "Record" { continue }
        let found = canvas_of(c)
        if typeof(found) == "Record" { return found }
    }
    0
}

fn run() {
    let e = render_graph({ dot: "digraph { rankdir=LR; web -> api; api -> db [label=\"write\"]; }",
                           title: "Request path" }, 640.0)
    let cv = canvas_of(e)
    if typeof(cv) != "Record" { fail("graph produced no canvas") }
    let kids = unwrap_or(get(cv, "children"), [])
    if len(kids) < 6 { fail("graph should draw 3 boxes + 2 edges + labels, got ${len(kids)}") }
    let mut seen = {}
    for it in kids {
        let id = unwrap_or(get(it, "id"), "")
        if typeof(id) != "Str" or id == "" { fail("a graph canvas item has no id") }
        if has(seen, id) { fail("duplicate graph canvas id ${id}") }
        seen = assoc(seen, id, true)
        if unwrap_or(get(it, "kind"), "") == "path" {
            let d = unwrap_or(get(it, "d"), "")
            if not starts_with(d, "M") { fail("graph path data must start with M: ${d}") }
            if contains(d, "NaN") { fail("graph path data has NaN: ${d}") }
        }
    }
    // Shapes and multi-line labels.
    let shapes = render_graph({ dot: "digraph { a [shape=circle]; b [shape=diamond, label=\"two\\nlines\"]; a -> b; }" }, 640.0)
    if typeof(canvas_of(shapes)) != "Record" { fail("shaped graph produced no canvas") }
    // A bad graph reports itself instead of drawing.
    if typeof(canvas_of(render_graph({ dot: "flowchart { a }" }, 640.0))) == "Record" {
        fail("an invalid graph should not draw a canvas")
    }
    if typeof(canvas_of(render_graph({ dot: "" }, 640.0))) == "Record" {
        fail("an empty graph should not draw a canvas")
    }
    println("pad_graph render: ok")
}
run()
""",
    ),
}

IMPORT_RE = re.compile(r"^import\s.*?(?:\}\s*from\s*\"[^\"]+\"|\"[^\"]+\")\s*$", re.M | re.S)


def strip_module(text: str) -> str:
    """Drop import statements and `export` / `extern fn` declarations."""
    out, skipping = [], False
    for line in text.splitlines():
        stripped = line.strip()
        if skipping:
            if "from" in stripped and stripped.endswith('"') or stripped.endswith('}'):
                skipping = False
            continue
        if stripped.startswith("import "):
            # Multi-line import lists continue until the closing `from "..."`.
            if not (stripped.endswith('"') and "from" in stripped) and not stripped.startswith('import "'):
                skipping = True
            continue
        if stripped.startswith("extern fn "):
            continue
        if stripped.startswith("export "):
            line = line.replace("export ", "", 1)
        out.append(line)
    return "\n".join(out)


def main() -> int:
    failures = 0
    for name, (modules, driver) in CHECKS.items():
        parts = [STUBS]
        for m in modules:
            parts.append(f"\n// ==== {m} ====\n")
            parts.append(strip_module((WIDGETS / m).read_text()))
        parts.append("\n// ==== driver ====\n")
        parts.append(driver)
        source = "\n".join(parts)
        with tempfile.NamedTemporaryFile("w", suffix=".ft", delete=False) as f:
            f.write(source)
            path = f.name
        proc = subprocess.run(["funct", path], capture_output=True, text=True)
        if proc.returncode != 0 or "ok" not in proc.stdout:
            failures += 1
            print(f"[{name}] FAILED")
            print(proc.stdout.strip())
            print(proc.stderr.strip())
            print(f"  (expanded source kept at {path})")
        else:
            print(f"[{name}] {proc.stdout.strip()}")
            pathlib.Path(path).unlink(missing_ok=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
