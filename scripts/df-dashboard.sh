#!/usr/bin/env bash
# df-dashboard.sh — wire up a generic dataflow dashboard from PRIMITIVES.
#
#   scripts/df-dashboard.sh [Project] [preset]
#     Project : pane target (default "Recursion" — never the user's active project)
#     preset  : by-day (default) | blog | top | timeline | combined
#               | quakes | github   ← totally different PUBLIC APIs, same widgets
#               (proves the mechanism is generic: only `params` change)
#
# There are exactly two primitive widgets, configured per instance via
# `params` (no per-endpoint files):
#   http.ft   — params { url, out, curl_cfg, interval }  → GETs any URL,
#               publishes raw JSON on its `out` topic.
#   df_view_* — params { in, rows_path, x, y }           → reads the row
#               array at `rows_path` and plots columns `x`/`y`.
# The "connector" is just a shared topic: http.out == chart.in.
# Swapping the preset changes only params — the widgets are untouched.
set -euo pipefail

PROJECT="${1:-Recursion}"
PRESET="${2:-by-day}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WIDGETS_SRC="$REPO_DIR/crates/jim-widget/widgets"
WIDGETS_DST="$HOME/.jim/widgets"

for f in df.ft http.ft df_view_table.ft df_view_bar.ft df_view_vbars.ft \
         df_view_line.ft df_view_heatmap.ft df_view_stat.ft df_view_multiline.ft; do
    cp "$WIDGETS_SRC/$f" "$WIDGETS_DST/$f"
done

python3 - "$PROJECT" "$PRESET" "$HOME/.jim/dataflow/analytics.curl" <<'PY'
import socket, os, json, sys, time
project, preset, curl_cfg = sys.argv[1], sys.argv[2], sys.argv[3]
base = "https://computer.jimmyhmiller.com"

# A preset is just params. The widgets never change. A preset with
# `series_path` is nested multi-series (→ multi-line chart); otherwise a
# flat row array (→ the single-series chart set).
PRESETS = {
    # --- the analytics API (private, bearer auth via curl_cfg) ---
    "by-day":   dict(url=f"{base}/analytics/by-day?days=14",  rows_path="by_day", x="date",  y="views"),
    "blog":     dict(url=f"{base}/analytics/blog?n=30",       rows_path="posts",  x="slug",  y="views"),
    "top":      dict(url=f"{base}/analytics/top-paths?n=20",  rows_path="top",    x="value", y="views"),
    "timeline": dict(url=f"{base}/analytics/timeline?prefix=/api/&days=30&bucket=hour&n=8",
                     series_path="series", label="label", points="points", x="bucket_start_ms", y="views"),
    # --- proof of generality: totally different PUBLIC APIs, same widgets ---
    # USGS earthquakes (GeoJSON, NESTED — exercises dotted paths)
    "quakes":   dict(url="https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/all_day.geojson",
                     rows_path="features", x="properties.place", y="properties.mag", auth=False),
    # GitHub contributors (top-level array, flat)
    "github":   dict(url="https://api.github.com/repos/bevyengine/bevy/contributors?per_page=25",
                     rows_path="", x="login", y="contributions", auth=False),
}
def send(req):
    s = socket.socket(socket.AF_UNIX); s.connect(os.path.expanduser("~/.jim/socket"))
    s.sendall(json.dumps(req).encode()); s.shutdown(socket.SHUT_WR)
    try: s.recv(256)
    except Exception: pass
    s.close()

def spawn(script, title, pos, size, params):
    send({"action": "spawn_widget", "command": script, "kind": "script_widget",
          "project": project, "title": title, "position": pos, "size": size, "params": params})

def http_node(pr, topic, title, pos):
    cfg = curl_cfg if pr.get("auth", True) else ""   # public APIs need no auth
    spawn("http.ft", title, pos, [320, 140],
          {"url": pr["url"], "out": topic, "curl_cfg": cfg, "interval": 0})

def flat_charts(pr, topic, ox, oy):
    c = {k: v for k, v in pr.items() if k not in ("url", "auth")}; c["in"] = topic
    spawn("df_view_stat.ft",    "stat",    [ox + 330, oy],       [300, 140], c)
    spawn("df_view_table.ft",   "table",   [ox,       oy + 160], [300, 360], c)
    spawn("df_view_bar.ft",     "bars",    [ox + 320, oy + 160], [400, 360], c)
    spawn("df_view_vbars.ft",   "columns", [ox + 740, oy + 160], [420, 300], c)
    spawn("df_view_line.ft",    "line",    [ox,       oy + 540], [400, 280], c)
    spawn("df_view_heatmap.ft", "heatmap", [ox + 420, oy + 540], [400, 280], c)

def series_chart(pr, topic, pos, size):
    c = {k: v for k, v in pr.items() if k not in ("url", "auth")}; c["in"] = topic
    spawn("df_view_multiline.ft", "timeline", pos, size, c)

if preset == "combined":
    # blog graphs (data/raw) + ONE timeline (timeline/raw), side by side
    http_node(PRESETS["blog"], "data/raw", "http · blog", [40, 70])
    flat_charts(PRESETS["blog"], "data/raw", 40, 70)
    http_node(PRESETS["timeline"], "timeline/raw", "http · timeline", [870, 70])
    series_chart(PRESETS["timeline"], "timeline/raw", [870, 390], [880, 430])
    summary = "blog 6-charts + timeline multi-line"
else:
    p = PRESETS.get(preset, PRESETS["by-day"])
    topic = "data/raw"
    http_node(p, topic, "http", [40, 70])
    if "series_path" in p:
        c = {k: v for k, v in p.items() if k not in ("url", "auth")}; c["in"] = topic
        spawn("df_view_stat.ft",      "stat",   [400, 70], [330, 140], c)
        spawn("df_view_multiline.ft", "series", [40, 230], [760, 380], c)
        spawn("df_view_table.ft",     "table",  [820, 230],[380, 380], c)
        summary = "http → multi-line series + table"
    else:
        flat_charts(p, topic, 40, 70)
        summary = "http → 6 charts"

time.sleep(1)
send({"action": "activate_project", "project": project})
print(f"dashboard '{preset}' wired: {summary} into '{project}'")
PY
