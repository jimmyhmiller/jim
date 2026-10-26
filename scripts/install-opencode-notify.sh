#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"

ROOT="$ROOT" CONFIG_DIR="$CONFIG_DIR" python3 - <<'PY'
import json, os, pathlib, time

path = pathlib.Path(os.environ["CONFIG_DIR"]) / "cli.json"
plugin = str(pathlib.Path(os.environ["ROOT"]) / "tools/opencode-notify")
raw = path.read_text() if path.exists() else "{}"
data = json.loads(raw)
plugins = data.setdefault("plugins", [])
if not isinstance(plugins, list):
    raise SystemExit("cli.json plugins must be an array")

# Replace the old desktop-only stop notifier without touching other plugins.
plugins[:] = [entry for entry in plugins if entry != "./stop-notify"]
if plugin not in plugins:
    plugins.append(plugin)
out = json.dumps(data, indent=2) + "\n"
if out != raw:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        backup = path.with_name(f"cli.json.bak.{time.time_ns()}")
        backup.write_text(raw)
        print(f"Backup: {backup}")
    path.write_text(out)
print(f"Installed Jim notification plugin in {path}")
PY

echo "Restart OpenCode's TUI to load the plugin; no server or Jim restart is needed."
