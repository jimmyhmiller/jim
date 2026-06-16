//! Differential parity tests against the **real Excalidraw** logic.
//!
//! The golden fixtures under `tests/oracle/*.json` are produced by running the
//! genuine `@excalidraw/element` + `@excalidraw/math` functions headlessly; see
//! `oracle/README.md` for how to regenerate them. These tests load those golden
//! files (no Node needed) and assert whiteboard-core reproduces Excalidraw.
//!
//! Status:
//! - `bounds_absolute_coords_match` — GREEN: our `element_bounds` equals
//!   Excalidraw's `getElementAbsoluteCoords` (axis-aligned point/box bbox).
//! - `bounds_visual_match` — IGNORED: curve-aware `getElementBounds`
//!   (arrowhead/curve extents) — pending the bounds port (task #4).
//! - `binding_focus_gap_match` / `elbow_fixed_point_match` — IGNORED: pending
//!   the binding-model port (task #4). They are the executable spec for it.

use serde_json::Value;
use whiteboard_core::element::{binding_endpoint, binding_gap, determine_focus_distance, Element};
use whiteboard_core::geometry::{element_bounds, Point};
use whiteboard_core::io::load_excalidraw_str;

fn point(v: &Value) -> Point {
    Point::new(f(v, 0), f(v, 1))
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/oracle/{}.json", env!("CARGO_MANIFEST_DIR"), name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// Build a whiteboard-core `Element` from a serialized Excalidraw element by
/// loading it through the real `.excalidraw` document loader. The fixtures store
/// only geometry; non-geometric style fields the loader requires are filled from
/// Excalidraw's defaults here.
fn load_element(el: &Value) -> Element {
    let mut obj = serde_json::Map::new();
    // Excalidraw element defaults for the fields the loader requires but that
    // don't affect geometry.
    for (k, v) in [
        ("strokeColor", Value::from("#1e1e1e")),
        ("backgroundColor", Value::from("transparent")),
        ("fillStyle", Value::from("solid")),
        ("strokeStyle", Value::from("solid")),
        ("strokeWidth", Value::from(2)),
        ("roughness", Value::from(1)),
        ("opacity", Value::from(100)),
        ("angle", Value::from(0)),
        ("seed", Value::from(1)),
        ("version", Value::from(1)),
        ("versionNonce", Value::from(1)),
        ("isDeleted", Value::from(false)),
        ("groupIds", Value::Array(vec![])),
        ("boundElements", Value::Null),
        ("updated", Value::from(1)),
        ("link", Value::Null),
        ("locked", Value::from(false)),
    ] {
        obj.insert(k.to_string(), v);
    }
    // Overlay the fixture's actual fields.
    for (k, v) in el.as_object().expect("element is an object") {
        obj.insert(k.clone(), v.clone());
    }
    let doc = serde_json::json!({
        "type": "excalidraw",
        "version": 2,
        "source": "oracle",
        "elements": [Value::Object(obj)],
    });
    let els = load_excalidraw_str(&doc.to_string()).expect("load_excalidraw_str");
    els.into_iter().next().expect("exactly one element loaded")
}

fn approx(a: f64, b: f64, tol: f64, what: &str) {
    assert!(
        (a - b).abs() <= tol,
        "{what}: got {a}, expected {b} (|Δ|={} > {tol})",
        (a - b).abs()
    );
}

fn f(v: &Value, i: usize) -> f64 {
    v.as_array().unwrap()[i].as_f64().unwrap()
}

/// Our `element_bounds` is the axis-aligned point/box bounding box, which must
/// match Excalidraw's `getElementAbsoluteCoords` for every element kind.
#[test]
fn bounds_absolute_coords_match() {
    let fx = fixture("bounds");
    let tol = 1e-3;
    for case in fx["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let el = load_element(&case["element"]);
        let got = element_bounds(&el);
        let exp = &case["expected"]["absoluteCoords"];
        approx(got.min_x(), f(exp, 0), tol, &format!("{name} x1"));
        approx(got.min_y(), f(exp, 1), tol, &format!("{name} y1"));
        approx(got.max_x(), f(exp, 2), tol, &format!("{name} x2"));
        approx(got.max_y(), f(exp, 3), tol, &format!("{name} y2"));
    }
}

/// Curve/arrowhead-aware visual bounds (`getElementBounds`). Pending the bounds
/// port: our model returns the point bbox, which differs for curved arrows.
#[test]
#[ignore = "pending curve/arrowhead bounds port (task #4)"]
fn bounds_visual_match() {
    let fx = fixture("bounds");
    let tol = 0.5;
    for case in fx["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let el = load_element(&case["element"]);
        let got = element_bounds(&el);
        let exp = &case["expected"]["bounds"];
        approx(got.min_x(), f(exp, 0), tol, &format!("{name} x1"));
        approx(got.min_y(), f(exp, 1), tol, &format!("{name} y1"));
        approx(got.max_x(), f(exp, 2), tol, &format!("{name} x2"));
        approx(got.max_y(), f(exp, 3), tol, &format!("{name} y2"));
    }
}

/// Binding `focus` (Excalidraw's perpendicular-offset model, 0 = aim at center).
/// Ported via `determine_focus_distance`; verified against the real algorithm.
#[test]
fn binding_focus_match() {
    let fx = fixture("binding");
    let tol = 1e-4;
    for case in fx["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let target = load_element(&case["target"]);
        let exp = &case["expected"];
        let adjacent = point(&exp["adjacentAtBind"]);
        let edge = point(&exp["edgeAtBind"]);
        let got = determine_focus_distance(&target, adjacent, edge);
        let want = exp["endBinding"]["focus"].as_f64().unwrap();
        approx(got, want, tol, &format!("{name} focus"));
    }
}

/// Binding `gap`: clearance from the bound endpoint to the target outline,
/// floored at 1 and capped by `maxBindingGap`. Ported via `binding_gap`.
///
/// (Ellipse `gap` is skipped: Excalidraw's `distanceToEllipseElement` returns NaN
/// for a point at the exact center, which our model doesn't reproduce — and is
/// not behavior worth matching. Ellipse outline distance is covered elsewhere.)
#[test]
fn binding_gap_match() {
    let fx = fixture("binding");
    let tol = 1e-3;
    for case in fx["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let want = &case["expected"]["endBinding"]["gap"];
        if want.is_null() {
            continue; // NaN gap (ellipse center) — see doc comment.
        }
        let target = load_element(&case["target"]);
        let edge = point(&case["expected"]["edgeAtBind"]);
        approx(binding_gap(&target, edge), want.as_f64().unwrap(), tol, &format!("{name} gap"));
    }
}

/// The attached endpoint after the target moves, via the perpendicular-offset
/// focus model + outline-with-gap intersection (`binding_endpoint`). This is the
/// behaviour that keeps an arrow aimed at the same spot (the centre, for
/// `focus == 0`) as its target moves.
///
/// Tolerance: rectangles/ellipses match to sub-pixel; diamonds allow ~0.3px
/// because Excalidraw rounds the gap-expanded corners with offset Bézier arcs
/// while we use exact circular arcs — a sub-pixel difference only at sharp
/// corners. Ellipse cases with a `null` (NaN) gap are skipped, matching
/// `binding_gap_match` (Excalidraw's `distanceToEllipse` returns NaN at centre).
#[test]
fn binding_endpoint_match() {
    let fx = fixture("binding");
    for case in fx["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let exp = &case["expected"];
        let gap = &exp["endBinding"]["gap"];
        if gap.is_null() {
            continue; // NaN gap (ellipse centre) — see doc comment.
        }
        // The target moves +dx in x; the arrow's adjacent vertex stays put.
        let mut target = load_element(&case["target"]);
        let dx = exp["targetMovedDx"].as_f64().unwrap();
        target.x += dx;

        let focus = exp["endBinding"]["focus"].as_f64().unwrap();
        let adjacent = point(&exp["adjacentAtBind"]);
        let got = binding_endpoint(&target, focus, gap.as_f64().unwrap(), adjacent);
        let want = point(&exp["endpointAfterMove"]);

        let tol = if case["target"]["type"] == "diamond" {
            0.3
        } else {
            1e-2
        };
        approx(got.x, want.x, tol, &format!("{name} endpoint x"));
        approx(got.y, want.y, tol, &format!("{name} endpoint y"));
    }
}

/// Elbow-arrow fixed-point binding. Pending the elbow port.
#[test]
#[ignore = "pending elbow/fixed-point binding port (task #4)"]
fn elbow_fixed_point_match() {
    let fx = fixture("elbow");
    assert!(!fx["cases"].as_array().unwrap().is_empty());
    panic!("elbow parity not implemented yet");
}
