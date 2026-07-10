#!/bin/sh
# Build, kill the running GUI, relaunch — leaves daemon children alive
# (they persist across GUI restarts so terminal panes survive).
#
# Usage:
#   ./scripts/dev-restart.sh                 # release build (default — much faster runtime)
#   ./scripts/dev-restart.sh --debug         # debug build (faster compile, slower runtime)
#   ./scripts/dev-restart.sh -- --some-arg   # pass --some-arg to the GUI binary

set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

PROFILE=release
CARGO_PROFILE_ARGS="--release"
GUI_ARGS=""

while [ $# -gt 0 ]; do
    case "$1" in
        --release)
            PROFILE=release
            CARGO_PROFILE_ARGS="--release"
            shift
            ;;
        --debug)
            PROFILE=debug
            CARGO_PROFILE_ARGS=""
            shift
            ;;
        --)
            shift
            GUI_ARGS="$*"
            break
            ;;
        *)
            echo "unknown flag: $1" >&2
            exit 2
            ;;
    esac
done

echo "[dev-restart] building ($PROFILE)..."
# Bare build → workspace default-members (jim_app + jimctl + jim_widget),
# which covers `jim`, `jimctl`, and the `glaze_ui` widget that make-bundle
# copies into the .app. One command, no -p juggling.
cargo build $CARGO_PROFILE_ARGS

# Refresh the .app bundle so it carries the freshly-built binary and
# libghostty-vt dylib (copied in, not symlinked into target/).
# LaunchServices identity stays stable across rebuilds because
# CFBundleIdentifier doesn't change.
"$SCRIPT_DIR/make-bundle.sh" ${CARGO_PROFILE_ARGS:+--release}

# Launch via the bundle (not target/$PROFILE/jim directly) so
# AppKit walks up to Contents/Info.plist and treats us as a bundled
# app: stable Dock tile, pin survival, proper icon.
BIN="Jim.app/Contents/MacOS/jim"
if [ ! -x "$BIN" ]; then
    echo "[dev-restart] $BIN not found (bundle build failed?)" >&2
    exit 1
fi

# Kill any existing Jim GUI. Match BOTH profiles so a release-built GUI
# from a prior run gets cleaned up too, and accept both the bundle path
# (current launch route) and bare target/ paths (older runs predating
# the .app wrapper). Exclude:
#   - `jim-daemon` binary (separate path; survives by design)
#   - any `jim --daemon ...` invocation (the daemon-mode subprocess)
#   - `jim bus-daemon` (the widget/agent message-bus daemon; survives
#     GUI restarts so retained messages + agent roster persist)
ABS_BIN="$(pwd)/$BIN"
KILL=$(ps -ax -o pid,command \
    | awk '($0 ~ /Jim\.app\/Contents\/MacOS\/jim($|[[:space:]])/ \
            || $0 ~ /target\/(debug|release)\/jim($|[[:space:]])/) \
           && $0 !~ /--daemon/ \
           && $0 !~ /jim-daemon/ \
           && $0 !~ /bus-daemon/ { print $1 }')
if [ -n "$KILL" ]; then
    echo "[dev-restart] killing existing GUI(s): $KILL"
    kill $KILL 2>/dev/null || true
    # Give them a beat to release the socket before the new instance binds.
    sleep 0.4
fi

LOG=${TMPDIR:-/tmp}/jim-${PROFILE}.log
APP="$(pwd)/Jim.app"
echo "[dev-restart] launching → $LOG"
# Launch from a FRESH login-shell environment, not this script's inherited
# env. dev-restart is usually run from inside a Claude Code session, which
# exports GIT_EDITOR=true, CLAUDECODE=1, CLAUDE_CODE_*, etc. Those would
# otherwise flow all the way through to the GUI → the long-lived jim-daemon
# it spawns → every terminal pane (so git silently uses GIT_EDITOR=true
# instead of the user's core.editor). Note `open` does NOT isolate us here:
# on current macOS it forwards the caller's environment to the launched app.
#
# `env -i` drops the inherited env entirely; `zsh -l` then rebuilds PATH and
# friends from /etc/zprofile + the user's profile — giving the GUI exactly
# the environment a Terminal.app login shell has, with no agent vars to chase
# by name. We still launch via `open` (inside that clean shell) so the app
# keeps its bundle/Dock identity; -n forces a fresh instance and
# --stdout/--stderr reproduce the log redirection (stdin defaults to
# /dev/null). The dylib lives inside the bundle (rpath
# @executable_path/../Frameworks), so no DYLD_* env vars are needed.
env -i \
    HOME="$HOME" USER="$USER" LOGNAME="$LOGNAME" \
    TERM="${TERM:-xterm-256color}" SHELL="$SHELL" TMPDIR="$TMPDIR" LANG="$LANG" \
    /bin/zsh -lc "exec open -n '$APP' --stdout '$LOG' --stderr '$LOG' ${GUI_ARGS:+--args $GUI_ARGS}"
echo "[dev-restart] launched Jim.app via LaunchServices → $LOG"
