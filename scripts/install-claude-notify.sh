#!/usr/bin/env bash
# Install the "pulse jim when Claude stops" notification setup into
# ~/.claude/settings.json on this machine.
#
# Two things get merged in (idempotently — safe to re-run):
#   1. A Stop hook that writes a BEL (\a) to /dev/tty every time Claude
#      finishes a turn. jim's terminal emulator catches the BEL via its
#      on_bell callback and pulses the pane.
#   2. preferredNotifChannel = "terminal_bell", so jim also pulses on
#      Claude's built-in attention events (permission prompt / idle wait).
#
# Existing settings, hooks, and other Stop-hook commands are preserved.
set -euo pipefail

SETTINGS="${CLAUDE_SETTINGS:-$HOME/.claude/settings.json}"
BELL_CMD="printf '\\a' > /dev/tty 2>/dev/null || true"

PY="$(command -v python3 || true)"
if [ -z "$PY" ]; then
  echo "error: python3 not found on PATH; cannot merge JSON safely." >&2
  exit 1
fi

mkdir -p "$(dirname "$SETTINGS")"
[ -f "$SETTINGS" ] || echo '{}' > "$SETTINGS"

# Back up before touching it.
cp "$SETTINGS" "$SETTINGS.bak.$(date +%Y%m%d-%H%M%S)"

BELL_CMD="$BELL_CMD" SETTINGS="$SETTINGS" "$PY" - <<'PYEOF'
import json, os, sys

path = os.environ["SETTINGS"]
bell = os.environ["BELL_CMD"]

with open(path) as f:
    data = json.load(f)

# 1. preferredNotifChannel
data["preferredNotifChannel"] = "terminal_bell"

# 2. Stop hook BEL command (idempotent)
hooks = data.setdefault("hooks", {})
stop = hooks.get("Stop")
if not isinstance(stop, list):
    stop = []
    hooks["Stop"] = stop

# Strip any prior copy of our BEL command from every group.
for group in stop:
    if isinstance(group, dict) and isinstance(group.get("hooks"), list):
        group["hooks"] = [
            h for h in group["hooks"]
            if not (isinstance(h, dict) and h.get("command") == bell)
        ]

# Ensure there's at least one group, then append the BEL command to it.
if not stop:
    stop.append({"hooks": []})
first = stop[0]
first.setdefault("hooks", [])
first["hooks"].append({"type": "command", "command": bell})

with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")

print(f"updated {path}")
PYEOF

echo "Done. Restart Claude Code (new session) for the Stop hook to take effect."
