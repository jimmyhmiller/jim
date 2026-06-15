#!/bin/sh
# One-time setup so you can launch the Jim editor channel with the clean
# `--channels` flag instead of `--dangerously-load-development-channels`.
#
# What it does (all LOCAL — nothing is published):
#   1. registers the local marketplace at channel-plugin/ (`jim-local`)
#   2. installs the `jim` plugin from it
#   3. drops the managed-settings.json that allowlists the plugin (sudo)
#
# After this, launch a session with:
#   claude --channels plugin:jim@jim-local
#
# Re-running is safe (idempotent).
set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
MKT="$REPO/channel-plugin"
MANAGED_DIR="/Library/Application Support/ClaudeCode"
MANAGED="$MANAGED_DIR/managed-settings.json"

if ! command -v jimctl >/dev/null 2>&1; then
    echo "error: jimctl not on PATH." >&2
    echo "  run: cargo install --path '$REPO/crates/jimctl' --bin jimctl --force" >&2
    exit 1
fi
if ! command -v claude >/dev/null 2>&1; then
    echo "error: claude CLI not found on PATH." >&2
    exit 1
fi

echo "==> registering local marketplace: $MKT"
claude plugin marketplace add "$MKT" 2>/dev/null \
    || claude plugin marketplace update jim-local

echo "==> installing plugin jim@jim-local"
claude plugin install jim@jim-local || true

echo "==> managed settings: $MANAGED"
if [ -f "$MANAGED" ]; then
    echo "    A managed-settings.json already exists. NOT overwriting it (it may"
    echo "    hold other policy). Merge these keys into it yourself:"
    echo "    ---"
    cat "$MKT/managed-settings.json"
    echo "    ---"
else
    echo "    Installing (requires sudo — this is an admin-only system path)…"
    sudo mkdir -p "$MANAGED_DIR"
    sudo cp "$MKT/managed-settings.json" "$MANAGED"
    echo "    installed."
fi

cat <<EOF

Done. Launch a Jim-channel session with:

  claude --channels plugin:jim@jim-local

Handy alias (add to ~/.zshrc):

  alias cj='claude --channels plugin:jim@jim-local'

To undo: claude plugin uninstall jim@jim-local && claude plugin marketplace remove jim-local
         sudo rm "$MANAGED"
EOF
