#!/usr/bin/env bash
# Install Phonon-2 for jim's dictation (crates/jim-app/src/dictation/phonon.rs).
#
# Jim runs this itself (it's compiled in) the first time Phonon is needed, and
# again whenever PIN changes; running it by hand does the same thing.
#
# Builds a pinned virtualenv at ~/.jim/phonon/venv on a uv-managed Python —
# not Homebrew's, which a `brew upgrade` would yank out from under the venv —
# and fetches + verifies the model, so jim's `phonon serve` can start
# offline. Only the CPU engine is installed: jim deliberately doesn't use
# MLX (see phonon.rs for why).
#
# Uses the uv on PATH, or else downloads a pinned uv into ~/.jim/phonon/uv
# (it touches no shell profile).
#
# The last step writes PIN to ~/.jim/phonon/installed. Jim treats Phonon as
# installed only when that marker matches the PIN below, so an interrupted
# install is redone, and bumping PIN reinstalls everywhere. Re-running is
# safe; it rebuilds the venv in place. Jim notices the new install (the entry
# point's mtime is part of the shared server's key) and replaces a server
# still running the old one.
set -euo pipefail

# jim parses this line to know which install it expects — keep it one literal.
PIN="fermion-research==0.2.11"
PYTHON_VERSION="3.12"
UV_VERSION="0.5.9"
PHONON_HOME="${PHONON_HOME:-$HOME/.jim/phonon}"
VENV="$PHONON_HOME/venv"
MARKER="$PHONON_HOME/installed"

mkdir -p "$PHONON_HOME"
rm -f "$MARKER"

if command -v uv >/dev/null 2>&1; then
    UV="$(command -v uv)"
elif [ -x "$PHONON_HOME/uv/uv" ]; then
    UV="$PHONON_HOME/uv/uv"
else
    echo "[install-phonon] no uv on PATH; installing uv $UV_VERSION into $PHONON_HOME/uv"
    curl -LsSf "https://astral.sh/uv/$UV_VERSION/install.sh" \
        | UV_UNMANAGED_INSTALL="$PHONON_HOME/uv" sh
    UV="$PHONON_HOME/uv/uv"
fi
echo "[install-phonon] using $UV ($("$UV" --version))"

echo "[install-phonon] creating $VENV (Python $PYTHON_VERSION, uv-managed)"
# Rebuilt from scratch rather than updated, so a re-run can't keep stale
# packages around.
rm -rf "$VENV"
"$UV" venv --quiet --python-preference only-managed --python "$PYTHON_VERSION" "$VENV"
echo "[install-phonon] installing $PIN"
"$UV" pip install --quiet --python "$VENV/bin/python" "$PIN"

echo "[install-phonon] fetching and verifying the Phonon-2 model"
# The CLI still parses an audio path with --download-only (it just doesn't
# check it), so it gets a placeholder.
FERMION_DEVICE=cpu "$VENV/bin/phonon" transcribe --download-only /dev/null

echo "$PIN" > "$MARKER"
echo "[install-phonon] done: $VENV/bin/phonon"
echo "[install-phonon] switch dictation to it from the palette: \"Dictation: Use Phonon\""
