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
ABS_BIN="$(pwd)/$BIN"
KILL=$(ps -ax -o pid,command \
    | awk '($0 ~ /Jim\.app\/Contents\/MacOS\/jim($|[[:space:]])/ \
            || $0 ~ /target\/(debug|release)\/jim($|[[:space:]])/) \
           && $0 !~ /--daemon/ \
           && $0 !~ /jim-daemon/ { print $1 }')
if [ -n "$KILL" ]; then
    echo "[dev-restart] killing existing GUI(s): $KILL"
    kill $KILL 2>/dev/null || true
    # Give them a beat to release the socket before the new instance binds.
    sleep 0.4
fi

LOG=${TMPDIR:-/tmp}/jim-${PROFILE}.log
echo "[dev-restart] launching → $LOG"
# The dylib lives inside the bundle (Contents/Frameworks) and the
# binary's rpath was set to @executable_path/../Frameworks by
# make-bundle.sh, so no DYLD_* env vars are needed. `& disown`
# detaches the child cleanly without going through nohup.
"$ABS_BIN" $GUI_ARGS </dev/null >"$LOG" 2>&1 &
NEW_PID=$!
disown $NEW_PID 2>/dev/null || true
echo "[dev-restart] started PID $NEW_PID"
