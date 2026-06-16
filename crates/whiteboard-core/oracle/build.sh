#!/usr/bin/env bash
# Reproducibly build the Excalidraw oracle bundle.
#
#   ./build.sh         # install (npm ci if a lockfile exists), patch, bundle
#
# Produces ./bundle.mjs — a single flat ESM file with the real Excalidraw
# element + math logic, importable headlessly by gen.mjs. node_modules/ and
# bundle.mjs are gitignored (regenerated); package.json + package-lock.json are
# committed so versions are pinned. After building, run `node gen.mjs` to
# regenerate the committed golden fixtures under ../tests/oracle/.
set -euo pipefail
cd "$(dirname "$0")"

if [ -f package-lock.json ]; then
  npm ci
else
  npm install
  echo "[build] no lockfile existed; one was just generated — commit package-lock.json"
fi

node patch-exports.mjs

npx esbuild entry.mjs \
  --bundle --platform=node --format=esm \
  --outfile=bundle.mjs

echo "[build] bundle.mjs ready ($(wc -c < bundle.mjs) bytes)"
echo "[build] next: node gen.mjs   # regenerate golden fixtures"
