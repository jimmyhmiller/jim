// The published @excalidraw/{element,math,common} prod builds leak internal
// deep-subpath imports (e.g. `@excalidraw/math/ellipse`) that the packages'
// own `exports` maps only expose for *types*, not runtime. Node and esbuild
// both refuse to resolve them. Each package ships a single bundled
// `dist/prod/index.js` that re-exports everything, so we widen the `exports`
// map to resolve every subpath to that index. Idempotent; run after install.
import { readFileSync, writeFileSync, existsSync } from "node:fs";

for (const pkg of ["@excalidraw/math", "@excalidraw/common", "@excalidraw/element"]) {
  const p = `node_modules/${pkg}/package.json`;
  if (!existsSync(p)) {
    console.error(`patch-exports: ${p} missing (run npm install first)`);
    process.exit(1);
  }
  const j = JSON.parse(readFileSync(p, "utf8"));
  j.exports = j.exports || {};
  const star = j.exports["./*"] || {};
  if (star.default !== "./dist/prod/index.js") {
    j.exports["./*"] = { ...star, default: "./dist/prod/index.js" };
    writeFileSync(p, JSON.stringify(j, null, 2));
    console.log(`patched ${pkg}`);
  } else {
    console.log(`${pkg} already patched`);
  }
}
