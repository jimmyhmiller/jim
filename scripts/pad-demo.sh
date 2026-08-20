#!/usr/bin/env bash
# Fill a notebook with one of every cell type, then open it in a pane.
#
#   scripts/pad-demo.sh [notebook-name] [project]
#
# Useful as a live check after touching the pad widgets: if a cell renders
# blank, the log line says which one, and `python3 scripts/pad-render-check.py`
# will usually reproduce it without the GUI.
set -euo pipefail

NOTEBOOK="${1:-pad-demo}"
PROJECT="${2:-Recursion}"
JIMCTL="${JIMCTL:-$(dirname "$0")/../target/release/jimctl}"

"$JIMCTL" pad new "$NOTEBOOK" --force >/dev/null

"$JIMCTL" pad md --notebook "$NOTEBOOK" "# Checkout latency spike

A pad reads like a report: **prose**, figures, and \`code\` in one column, with
space rather than boxes doing the separating.

- Written from a terminal, one cell per command
- Rendered live in the pane as each command returns
- The notebook is a JSONL file, so it survives a restart" >/dev/null

"$JIMCTL" pad stats --notebook "$NOTEBOOK" '[
  {"label":"p95","value":"412ms","delta":"+180ms","up_is_good":false},
  {"label":"requests","value":128400,"delta":"+4%","spark":[3,5,4,8,6,9,12]},
  {"label":"error rate","value":"0.7%","delta":"-0.2%","up_is_good":false}
]' >/dev/null

"$JIMCTL" pad chart --notebook "$NOTEBOOK" '{
  "type":"line","title":"p95 by region","y_label":"milliseconds",
  "series":[
    {"name":"us-east","points":[["2026-08-01",120],["2026-08-02",141],["2026-08-03",412],["2026-08-04",380]]},
    {"name":"eu-west","points":[["2026-08-01",96],["2026-08-02",99],["2026-08-03",104],["2026-08-04",101]]}
  ]}' >/dev/null

"$JIMCTL" pad chart --notebook "$NOTEBOOK" '{
  "type":"bar","title":"Slow queries by table","stacked":true,
  "categories":["orders","carts","users","events"],
  "series":[{"name":"read","values":[42,31,12,8]},{"name":"write","values":[18,9,3,22]}]}' >/dev/null

"$JIMCTL" pad chart --notebook "$NOTEBOOK" '{
  "type":"donut","title":"Time in the request",
  "slices":[{"label":"db","value":210},{"label":"render","value":120},
            {"label":"auth","value":48},{"label":"other","value":34}]}' >/dev/null

"$JIMCTL" pad chart --notebook "$NOTEBOOK" '{
  "type":"histogram","title":"Response times",
  "values":[12,14,15,15,16,18,19,19,20,21,22,24,25,28,30,31,44,52,88,412]}' >/dev/null

"$JIMCTL" pad table --notebook "$NOTEBOOK" --title "Slowest endpoints" '[
  {"endpoint":"/checkout","calls":18402,"p95_ms":412},
  {"endpoint":"/cart","calls":90211,"p95_ms":88},
  {"endpoint":"/search","calls":41028,"p95_ms":61}
]' >/dev/null

"$JIMCTL" pad graph --notebook "$NOTEBOOK" --title "Request path" \
  'digraph { rankdir=LR; web -> api [label="http"]; api -> db [label="write"]; api -> cache; cache -> db; }' >/dev/null

"$JIMCTL" pad code --notebook "$NOTEBOOK" --lang rust --title "the retry loop" 'loop {
    match send(&req) {
        Ok(r) => break r,
        // No backoff: this is the bug.
        Err(_) => continue,
    }
}' >/dev/null

"$JIMCTL" pad json --notebook "$NOTEBOOK" --title "config" \
  '{"retries":{"max":5,"backoff":null},"pool":{"size":32,"timeout_ms":250},"regions":["us-east","eu-west"]}' >/dev/null

"$JIMCTL" pad callout --notebook "$NOTEBOOK" warn "The rollout window overlaps the spike — correlation is not settled." >/dev/null
"$JIMCTL" pad callout --notebook "$NOTEBOOK" success "Backoff shipped; p95 back under 150ms." >/dev/null

"$JIMCTL" pad md --notebook "$NOTEBOOK" '## Findings

> The retry loop had no backoff, so a slow dependency turned into a stampede.

1. The spike starts at the deploy, not at the traffic peak
2. Retries triple the load on `/checkout` within 30 seconds
3. Adding jittered backoff flattens it' >/dev/null

echo "notebook: ~/.jim/pads/$NOTEBOOK.jsonl"
"$JIMCTL" widget --kind script_widget --project "$PROJECT" --title "pad: $NOTEBOOK" \
  --size 760,900 --params "{\"notebook\":\"$NOTEBOOK\"}" pad.ft
