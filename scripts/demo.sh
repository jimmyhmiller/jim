#!/bin/sh
# demo.sh — build + launch the Glaze component playground as a widget pane.
#
# `glaze_ui` is a subprocess widget that exercises every component we've built:
# Bar, Toggle, Tabs, Table, Slider, Checkbox, Radio, Stepper, Select, Tooltip,
# Dialog, Popover, Toast — all Glaze-slot-styled and interactive.
#
#   ./scripts/demo.sh    # build glaze_ui + spawn a pane in the running app
#
# Requires the terminal-bevy app to be running (./scripts/dev-restart.sh). The
# app must carry the current widget-bevy code, or it will reject the new Element
# types — so run ./scripts/dev-restart.sh after pulling component changes.

set -e
cd "$(dirname "$0")/.."

echo "[demo] building glaze_ui (release)..."
cargo build --release -p widget_bevy --bin glaze_ui

GLAZE_UI="$(pwd)/target/release/glaze_ui"
TBWIDGET="$(pwd)/target/release/tbwidget"

if [ ! -x "$TBWIDGET" ]; then
    echo "[demo] $TBWIDGET not found — run ./scripts/dev-restart.sh first." >&2
    exit 1
fi

echo "[demo] spawning 'Glaze Demo' pane..."
"$TBWIDGET" --title "Glaze Demo" -- "$GLAZE_UI"
echo "[demo] done — look for the 'Glaze Demo' pane in the app."
