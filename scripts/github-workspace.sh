#!/usr/bin/env bash
# github-workspace.sh — spawn the git/GitHub workspace.
#
#   scripts/github-workspace.sh [Project] [RepoPath]
#
# Shaped like a real application, not a widget wall:
#
#   ┌──────────┬──────────────────────────────┐
#   │ Repo     │ Pull Requests                │
#   │ (hub     │ (the main list you read)     │
#   │ sidebar) │                              │
#   └──────────┴──────────────────────────────┘
#
# The hub sidebar is the navigation: Stage(N) / Diff / Branches /
# AI Work / Reviews / Terminal all open on demand as floating panes —
# they're tasks you dip into, not ambient chrome. Nothing else is
# docked by default.
set -euo pipefail

PROJECT="${1:-Recursion}"
REPO="${2:-}"

JIMCTL="$(command -v jimctl || echo "$HOME/.local/bin/jimctl")"

spawn() { # title script params
    "$JIMCTL" widget --kind script_widget --project "$PROJECT" \
        --title "$1" --params "$3" "$2"
}

PARAMS_BASE="{\"project\":\"$PROJECT\""
if [[ -n "$REPO" ]]; then
    PARAMS_COMMON="$PARAMS_BASE,\"repo\":\"$REPO\",\"project_root\":\"$REPO\"}"
else
    PARAMS_COMMON="$PARAMS_BASE}"
fi

spawn "Repo"          repo_hub.ft     "$PARAMS_COMMON"
spawn "Pull Requests" pr_dashboard.ft "$PARAMS_COMMON"

# Give the spawns a beat to land before docking sidebar + main.
sleep 1
"$JIMCTL" dock --project "$PROJECT" \
    --title "Repo" --title "Pull Requests" \
    --template sidebar

echo "github workspace up in project '$PROJECT' (hub sidebar + PR main)"
