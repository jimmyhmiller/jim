// Bundle entry: re-export the real Excalidraw element + math logic as a single
// flat ESM module so esbuild can resolve the deep internal subpaths once and
// gen.mjs can import everything from ./bundle.mjs without a node_modules tree.
export * as Element from "@excalidraw/element";
export * as Math from "@excalidraw/math";
