// Generate golden fixtures from the REAL Excalidraw element/math logic.
//
//   ./build.sh && node gen.mjs
//
// Writes deterministic JSON under ../tests/oracle/ that whiteboard-core's
// `tests/oracle_parity.rs` compares against offline (no Node at test time).
//
// Determinism: every element gets fixed id/seed/version/versionNonce so the
// rough geometry (and thus bounds) is identical run-to-run. No timestamps are
// written. Re-run after bumping the pinned Excalidraw version in package.json;
// commit the regenerated fixtures.

// Headless shims: Excalidraw guards a dev-only validation on `window?.…`, which
// still ReferenceErrors when `window` is undeclared.
globalThis.window = globalThis.window || {};
globalThis.document = globalThis.document || {};

import { writeFileSync, mkdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { Element as E } from "./bundle.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = join(HERE, "..", "tests", "oracle");
mkdirSync(OUT, { recursive: true });

const VERSION = JSON.parse(
  readFileSync(join(HERE, "node_modules", "@excalidraw", "element", "package.json"), "utf8"),
).version;

const {
  newElement,
  newLinearElement,
  newArrowElement,
  Scene,
  bindLinearElement,
  updateBoundElements,
  getElementBounds,
  getElementAbsoluteCoords,
} = E;

let idCounter = 0;
// Make an element deterministic: fixed id + stable rough seed/version so bounds
// don't wobble between runs.
function det(el, id) {
  el.id = id ?? `el-${idCounter++}`;
  el.seed = 12345;
  el.version = 1;
  el.versionNonce = 1;
  el.updated = 1;
  return el;
}

function rect(id, x, y, w, h, extra = {}) {
  return det(newElement({ type: "rectangle", x, y, width: w, height: h, ...extra }), id);
}
function ellipse(id, x, y, w, h) {
  return det(newElement({ type: "ellipse", x, y, width: w, height: h }), id);
}
function diamond(id, x, y, w, h) {
  return det(newElement({ type: "diamond", x, y, width: w, height: h }), id);
}
function line(id, x, y, points, extra = {}) {
  return det(newLinearElement({ type: "line", x, y, points, ...extra }), id);
}
function arrow(id, x, y, points, extra = {}) {
  return det(newArrowElement({ type: "arrow", x, y, points, ...extra }), id);
}

const round = (n) => (typeof n === "number" ? Math.round(n * 1e6) / 1e6 : n);
const roundArr = (a) => a.map(round);

// Strip an element down to the geometry fields whiteboard-core needs to load it
// (the standard .excalidraw element shape). Keeps it small + stable.
function serializeEl(el) {
  const keep = {
    id: el.id,
    type: el.type,
    x: round(el.x),
    y: round(el.y),
    width: round(el.width),
    height: round(el.height),
    angle: round(el.angle || 0),
    strokeWidth: el.strokeWidth,
    roundness: el.roundness ?? null,
  };
  if (el.points) keep.points = el.points.map(roundArr);
  if (el.pressures) keep.pressures = el.pressures;
  if (el.startBinding) keep.startBinding = el.startBinding;
  if (el.endBinding) keep.endBinding = el.endBinding;
  return keep;
}

function globalEndpoints(a) {
  const first = a.points[0];
  const last = a.points[a.points.length - 1];
  return {
    start: [round(a.x + first[0]), round(a.y + first[1])],
    end: [round(a.x + last[0]), round(a.y + last[1])],
  };
}

// ---------------------------------------------------------------- bounds ----
function genBounds() {
  const cases = [];
  const add = (name, el) => {
    try {
      const scene = new Scene([el]);
      const map = scene.getNonDeletedElementsMap();
      cases.push({
        name,
        element: serializeEl(el),
        expected: {
          bounds: roundArr(getElementBounds(el, map)),
          absoluteCoords: roundArr(getElementAbsoluteCoords(el, map)),
        },
      });
    } catch (err) {
      console.error(`bounds case "${name}" FAILED: ${err && err.message}`);
    }
  };

  add("rectangle", rect("r", 100, 120, 200, 90));
  add("ellipse", ellipse("e", 100, 120, 80, 80));
  add("diamond", diamond("d", 100, 120, 120, 60));
  add("line_2pt", line("l2", 50, 50, [[0, 0], [120, 40]]));
  add("line_zigzag", line("lz", 50, 50, [[0, 0], [60, -30], [120, 30], [180, 0]]));
  add("arrow_2pt", arrow("a2", 50, 50, [[0, 0], [120, 40]]));
  add(
    "arrow_curved",
    arrow("ac", 50, 50, [[0, 0], [60, -40], [120, 30]], {
      roundness: { type: 2 },
    }),
  );
  // NOTE: freedraw bounds (perfect-freehand outline) not yet generated — needs
  // pressure/outline setup; arrows/lines/shapes are the priority. TODO.

  return { kind: "bounds", excalidrawVersion: VERSION, cases };
}

// --------------------------------------------------------------- binding ----
// Regular (non-elbow) arrow: bind its END to a target and record the resulting
// focus/gap + global endpoint, then move the target and record the followed
// endpoint. This is the model whiteboard-core must reproduce.
function genBinding() {
  const cases = [];
  const add = (name, target, aArrow) => {
    const scene = new Scene([target, aArrow]);
    bindLinearElement(aArrow, target, "end", scene);
    const beforeBinding = aArrow.endBinding ? { ...aArrow.endBinding } : null;
    const before = globalEndpoints(aArrow);
    // The arrow + target geometry AT BIND TIME (before the move below) — this is
    // the input whiteboard-core must reproduce focus/gap/endpoint from.
    const targetAtBind = serializeEl(target);
    const arrowAtBind = serializeEl(aArrow);

    // Move the target +120 in x and let the arrow follow.
    const moved = { ...target, x: target.x + 120 };
    scene.replaceAllElements([moved, aArrow]);
    updateBoundElements(moved, scene);
    const after = globalEndpoints(aArrow);

    cases.push({
      name,
      target: targetAtBind,
      arrow: arrowAtBind,
      expected: {
        endBinding: beforeBinding
          ? { focus: round(beforeBinding.focus), gap: round(beforeBinding.gap) }
          : null,
        // adjacent = the arrow's neighbor point, edge = the bound endpoint, both
        // global, at bind time (the inputs to determineFocusDistance).
        adjacentAtBind: before.start,
        edgeAtBind: before.end,
        endpointAtBind: before.end,
        targetMovedDx: 120,
        endpointAfterMove: after.end,
      },
    });
  };

  // Aim at the left-middle of a rectangle.
  add("arrow_to_rect_left_mid", rect("r", 300, 200, 100, 60), arrow("a", 150, 230, [[0, 0], [155, 2]]));
  // Aim straight at the center of a rectangle (focus should be ~0).
  add("arrow_to_rect_center", rect("r", 300, 200, 100, 60), arrow("a", 150, 230, [[0, 0], [200, 0]]));
  // Aim at the center of an ellipse.
  add("arrow_to_ellipse_center", ellipse("e", 300, 200, 100, 100), arrow("a", 120, 250, [[0, 0], [230, 0]]));
  // Aim at a diamond.
  add("arrow_to_diamond", diamond("d", 300, 200, 120, 80), arrow("a", 140, 240, [[0, 0], [220, 0]]));

  return { kind: "binding", excalidrawVersion: VERSION, cases };
}

// ----------------------------------------------------------------- elbow ----
// Elbow arrows use a fixed-point binding (a normalized [u,v] inside the target).
function genElbow() {
  const cases = [];
  const add = (name, target, elbow) => {
    const scene = new Scene([target, elbow]);
    try {
      bindLinearElement(elbow, target, "end", scene);
      const eb = elbow.endBinding ? { ...elbow.endBinding } : null;
      cases.push({
        name,
        target: serializeEl(target),
        arrow: serializeEl(elbow),
        expected: {
          endBinding: eb
            ? {
                focus: round(eb.focus),
                gap: round(eb.gap),
                fixedPoint: eb.fixedPoint ? roundArr(eb.fixedPoint) : null,
              }
            : null,
          endpointAtBind: globalEndpoints(elbow).end,
        },
      });
    } catch (err) {
      cases.push({ name, error: String(err && err.message) });
    }
  };

  add(
    "elbow_to_rect",
    rect("r", 300, 200, 120, 80),
    arrow("eb", 120, 240, [[0, 0], [220, 0]], { elbowed: true }),
  );

  return { kind: "elbow", excalidrawVersion: VERSION, cases };
}

function write(name, data) {
  const path = join(OUT, `${name}.json`);
  writeFileSync(path, JSON.stringify(data, null, 2) + "\n");
  console.log(`wrote ${path} (${data.cases.length} cases)`);
}

write("bounds", genBounds());
write("binding", genBinding());
write("elbow", genElbow());
console.log(`done — Excalidraw ${VERSION}`);
