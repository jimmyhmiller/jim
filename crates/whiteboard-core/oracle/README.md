# Excalidraw oracle

A **differential oracle**: it runs the genuine Excalidraw element/geometry logic
(`@excalidraw/element` + `@excalidraw/math`) headlessly in Node and writes golden
JSON fixtures that `whiteboard-core` is tested against. This is how we drive
`whiteboard-core` toward true Excalidraw parity (bounds, arrow binding focus/gap,
elbow fixed-point bindings) — the real implementation is the source of truth.

## What's committed vs generated

Committed (reproducible inputs + outputs):
- `package.json`, `package-lock.json` — pinned Excalidraw + esbuild versions.
- `build.sh`, `patch-exports.mjs`, `entry.mjs`, `gen.mjs` — the toolchain.
- `../tests/oracle/*.json` — the **golden fixtures** (the test inputs/outputs).

Generated (gitignored, rebuilt on demand):
- `node_modules/`, `bundle.mjs`.

`cargo test` consumes only the committed fixtures, so **Node is not needed to run
the tests** — only to regenerate fixtures.

## Regenerate the fixtures

Requires Node (tested with v24) and network access for the first install.

```sh
cd crates/whiteboard-core/oracle
./build.sh        # npm ci + patch package exports + esbuild bundle -> bundle.mjs
node gen.mjs      # write ../tests/oracle/{bounds,binding,elbow}.json
```

Then run the parity tests:

```sh
cargo test -p whiteboard-core --test oracle_parity            # green (supported) checks
cargo test -p whiteboard-core --test oracle_parity -- --ignored   # the port targets (task #4)
```

Output is deterministic: every element gets a fixed id/seed/version, no
timestamps are written, so re-running `gen.mjs` produces byte-identical files.
Commit the regenerated fixtures.

## Bumping the Excalidraw version

1. Edit the pinned versions in `package.json`.
2. `rm -f package-lock.json bundle.mjs && rm -rf node_modules && ./build.sh`
3. `node gen.mjs`, review the fixture diff, commit `package-lock.json` + fixtures.

## Why the `patch-exports.mjs` step?

The published `@excalidraw/{element,math,common}` prod builds import internal deep
subpaths (e.g. `@excalidraw/math/ellipse`) that their own `exports` maps only
expose for TypeScript types, not runtime. Each package ships a single bundled
`dist/prod/index.js` that re-exports everything, so `patch-exports.mjs` widens
each `exports` map to resolve every subpath to that index. Without it, Node and
esbuild both fail to resolve the imports.

`gen.mjs` also shims `globalThis.window`/`document` because Excalidraw guards a
dev-only validation on `window?.…`, which still throws when `window` is
undeclared in Node.

## Notes / TODO

- Freedraw bounds (perfect-freehand outline) are not yet generated — needs
  pressure/outline setup. Arrows/lines/shapes are covered.
- The current fixtures cover `getElementBounds` / `getElementAbsoluteCoords`,
  regular-arrow binding focus/gap + follow-on-move, and one elbow fixed-point
  case. Extend `gen.mjs` as the port (task #4) needs more coverage.
