#!/bin/zsh
# Install claude-bus as a LaunchAgent.
#
# We copy the binary into ~/Library/Application Support/editor-idea/bin/
# rather than launching it directly from target/release/, because macOS
# TCC blocks launchd-spawned processes that live under ~/Documents from
# starting (the kTCCServiceSystemPolicyDocumentsFolder prompt never
# fires while launchd is the parent, so the binary hangs in dyld
# forever). After this script, re-run it any time you `cargo build
# --release` to pick up a new bus binary.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${(%):-%x}")/../../.." && pwd)"
SRC_BIN="$REPO_ROOT/target/release/claude-bus"
INSTALL_DIR="$HOME/Library/Application Support/editor-idea/bin"
INSTALL_BIN="$INSTALL_DIR/claude-bus"
PLIST_LABEL="com.editor-idea.claude-bus"
PLIST_SRC="$REPO_ROOT/crates/claude-bus/dist/com.editor-idea.claude-bus.plist"
PLIST_DST="$HOME/Library/LaunchAgents/$PLIST_LABEL.plist"

if [[ ! -x "$SRC_BIN" ]]; then
  echo "build first: cargo build --release -p claude-bus" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
cp -f "$SRC_BIN" "$INSTALL_BIN"

# Render the plist with the install path substituted in. The committed
# plist references the install path directly so this is a straight copy
# today, but keeping it as a sed step lets us host-localize later
# without remembering.
mkdir -p "$HOME/Library/LaunchAgents"
sed "s|@INSTALL_BIN@|$INSTALL_BIN|g; s|@HOME@|$HOME|g" "$PLIST_SRC" > "$PLIST_DST"

if launchctl print "gui/$(id -u)/$PLIST_LABEL" >/dev/null 2>&1; then
  launchctl bootout "gui/$(id -u)/$PLIST_LABEL" || true
fi
launchctl bootstrap "gui/$(id -u)" "$PLIST_DST"
echo "installed; tail ~/.claude/bus.log to watch"
