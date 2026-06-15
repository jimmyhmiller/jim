#!/usr/bin/env bash
# website-blog-dashboard.sh — blog-visitor analytics on the "Website" project.
#
# A curated dataflow dashboard built from the SAME generic primitives as
# df-dashboard.sh (http.ft source + df_view_* charts), wired by shared
# topics. Nothing here is endpoint-specific code — only `params`.
#
#   scripts/website-blog-dashboard.sh [Project] [days] [bucket]
#     Project : pane target (default "Website")
#     days    : analytics window for the timeline (default 30)
#     bucket  : timeline granularity, hour|day (default hour — the
#               finest the API supports; minute/Nm fall back to day)
#
# Graphs (all blog-scoped — blog posts live at /api/<slug>):
#   • stat      — headline total/peak/avg blog-post views
#   • bars      — blog posts ranked by views
#   • table     — the same, as a sortable table
#   • timeline  — per-post views over time (the visits breakdown), one
#                 line per top post + legend
set -euo pipefail

PROJECT="${1:-Website}"
DAYS="${2:-30}"
BUCKET="${3:-hour}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WIDGETS_SRC="$REPO_DIR/crates/jim-widget/widgets"
WIDGETS_DST="$HOME/.jim/widgets"

# Keep the installed primitives in sync with the repo.
for f in df.ft http.ft df_view_table.ft df_view_bar.ft df_view_stat.ft \
         df_view_multiline.ft; do
    cp "$WIDGETS_SRC/$f" "$WIDGETS_DST/$f"
done

python3 - "$PROJECT" "$DAYS" "$BUCKET" "$HOME/.jim/dataflow/analytics.curl" <<'PY'
import socket, os, json, sys, time
project, days, bucket, curl_cfg = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
base = "https://computer.jimmyhmiller.com"

# Two sources (private analytics, bearer auth via curl_cfg), each on its
# own topic so the views fan out without cross-talk.
BLOG_URL     = f"{base}/analytics/blog?n=20"
# n=1000 → ALL blog posts as their own series (not a top-N cap).
TIMELINE_URL = f"{base}/analytics/timeline?prefix=/api/&days={days}&bucket={bucket}&n=1000"
BLOG_TOPIC, TL_TOPIC = "blog/raw", "blog/timeline/raw"

# blog payload: { posts: [{ slug, path, views }], total_views, ... }
blog_view = dict(rows_path="posts", x="slug", y="views")
# timeline payload: { series: [{ label, path, points: [{bucket_start_ms, views}] }] }
tl_view = dict(series_path="series", label="label", points="points",
               x="bucket_start_ms", y="views")

def send(req):
    s = socket.socket(socket.AF_UNIX); s.connect(os.path.expanduser("~/.jim/socket"))
    s.sendall(json.dumps(req).encode()); s.shutdown(socket.SHUT_WR)
    try: s.recv(256)
    except Exception: pass
    s.close()

def spawn(script, title, pos, size, params):
    send({"action": "spawn_widget", "command": script, "kind": "script_widget",
          "project": project, "title": title, "position": pos, "size": size,
          "params": params})

# --- sources (top row) ---
spawn("http.ft", "http · blog", [40, 70], [300, 130],
      {"url": BLOG_URL, "out": BLOG_TOPIC, "curl_cfg": curl_cfg, "interval": 0})
spawn("http.ft", "http · timeline", [360, 70], [300, 130],
      {"url": TIMELINE_URL, "out": TL_TOPIC, "curl_cfg": curl_cfg, "interval": 0})
spawn("df_view_stat.ft", "blog views", [680, 70], [340, 130],
      {**blog_view, "in": BLOG_TOPIC})

# --- blog ranking (middle row) ---
spawn("df_view_bar.ft", "posts ranked", [40, 220], [460, 420],
      {**blog_view, "in": BLOG_TOPIC})
spawn("df_view_table.ft", "posts table", [520, 220], [380, 420],
      {**blog_view, "in": BLOG_TOPIC})

# --- timeline breakdown of visits (bottom, wide) ---
spawn("df_view_multiline.ft", "visits over time", [40, 660], [1080, 440],
      {**tl_view, "in": TL_TOPIC})

time.sleep(1)
send({"action": "activate_project", "project": project})
print(f"blog dashboard wired into '{project}': stat + bars + table + "
      f"{days}-day per-post timeline @ {bucket} buckets")
PY
