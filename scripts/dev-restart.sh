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

# Finding the running GUI.
#
# This used to match only the full command line:
#     /Jim\.app\/Contents\/MacOS\/jim($|[[:space:]])/
# A GUI that has CRASHED but not yet been reaped shows in `ps` as "(jim)" —
# parenthesised, with no path — so it did not match, was never killed, and the
# script happily launched another one on top of it. Five crashed instances
# accumulated that way in one session before anyone noticed.
#
# Match on the executable name as well, which is stable in both states, and
# exclude the daemons (which survive restarts by design) and the CEF webview
# hosts (handled separately below).
jim_gui_pids() {
    ps -Ao pid=,comm=,command= | awk '
        {
            pid = $1
            comm = $2
            cmd = ""
            for (i = 3; i <= NF; i++) cmd = cmd $i " "
            # macOS `ps -o comm=` prints the full path, so compare basenames.
            n = split(comm, parts, "/")
            base = parts[n]
        }
        base ~ /daemon/            { next }   # jim-daemon
        cmd  ~ /--daemon/          { next }   # jim --daemon …
        cmd  ~ /bus-daemon/        { next }   # jim bus-daemon
        base == "jim-webview-host" { next }   # CEF host, reaped below
        (base == "jim" || base == "(jim)" \
         || cmd ~ /Jim\.app\/Contents\/MacOS\/jim( |$)/ \
         || cmd ~ /target\/(debug|release)\/jim( |$)/) { print pid }
    '
}

KILL=$(jim_gui_pids)
if [ -n "$KILL" ]; then
    # whisper-server is a normal child process, not one of Jim's persistent
    # daemons. macOS reparents it to PID 1 if the GUI is killed first, so old
    # restarts used to accumulate ~1GB orphan servers. Reap direct Whisper
    # children while their owning GUI PID is still available.
    WHISPER_KILL=""
    for gui_pid in $KILL; do
        children=$(ps -ax -o pid=,ppid=,comm= \
            | awk -v parent="$gui_pid" '$2 == parent && $3 ~ /whisper-server$/ { print $1 }')
        WHISPER_KILL="$WHISPER_KILL $children"
    done
    if [ -n "$(echo "$WHISPER_KILL" | tr -d ' ')" ]; then
        echo "[dev-restart] killing GUI-owned whisper server(s):$WHISPER_KILL"
        kill $WHISPER_KILL 2>/dev/null || true
    fi

    echo "[dev-restart] killing existing GUI(s): $KILL"
    kill $KILL 2>/dev/null || true

    # Verify they are actually gone. Never assume: launching on top of a
    # survivor is exactly how instances pile up.
    REMAIN=""
    i=0
    while [ $i -lt 20 ]; do
        sleep 0.1
        REMAIN=$(jim_gui_pids)
        [ -z "$REMAIN" ] && break
        i=$((i + 1))
    done

    if [ -n "$REMAIN" ]; then
        echo "[dev-restart] still alive after SIGTERM, sending SIGKILL: $REMAIN" >&2
        kill -9 $REMAIN 2>/dev/null || true
        sleep 0.5
        REMAIN=$(jim_gui_pids)
    fi

    if [ -n "$REMAIN" ]; then
        echo "[dev-restart] ERROR: GUI process(es) still running: $REMAIN" >&2
        echo "[dev-restart] refusing to launch — that would leave two Jims running." >&2
        exit 1
    fi
fi

# CEF webview hosts are children of the GUI, each owning a Chromium. They are
# NOT daemons and must not outlive the GUI; orphaned ones keep rendering and
# stack up across restarts.
HOSTS=$(ps -Ao pid=,comm= | awk '$2 == "jim-webview-host" { print $1 }')
if [ -n "$HOSTS" ]; then
    echo "[dev-restart] reaping webview host(s): $HOSTS"
    kill $HOSTS 2>/dev/null || true
    sleep 0.2
    HOSTS=$(ps -Ao pid=,comm= | awk '$2 == "jim-webview-host" { print $1 }')
    [ -n "$HOSTS" ] && kill -9 $HOSTS 2>/dev/null || true
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

# Belt and braces: prove we ended up with exactly one GUI. Silent duplicates
# are what made this script untrustworthy in the first place, so say so loudly
# rather than leaving it to be discovered by eye.
sleep 1
RUNNING=$(jim_gui_pids | tr '\n' ' ' | sed 's/ *$//')
COUNT=$(printf '%s' "$RUNNING" | wc -w | tr -d ' ')
if [ "$COUNT" = "1" ]; then
    echo "[dev-restart] verified: 1 GUI running (pid $RUNNING)"
else
    echo "[dev-restart] WARNING: expected 1 GUI, found $COUNT: $RUNNING" >&2
fi
