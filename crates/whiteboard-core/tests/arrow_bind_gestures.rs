//! Full-gesture regressions for arrow→shape binding, driven through the public
//! `Editor` API (the same path the GUI uses). These catch interaction bugs that
//! unit-testing the binding math in isolation misses — e.g. a bound arrow leaving
//! a stale bounding box so you can no longer click the shape it points at, or the
//! two ways an arrow can attach (edge vs. a fixed interior point).

use whiteboard_core::editor::Editor;
use whiteboard_core::element::{Element, ElementId, ElementKind};
use whiteboard_core::geometry::Point;
use whiteboard_core::interaction::{InputEvent, Modifiers, PointerButton, Tool};
use whiteboard_core::text::MonospaceMeasurer;

type Ed = Editor<MonospaceMeasurer>;
fn editor() -> Ed {
    Editor::new(MonospaceMeasurer::default())
}
fn down(x: f64, y: f64) -> InputEvent {
    InputEvent::PointerDown { pos: Point::new(x, y), button: PointerButton::Primary, mods: Modifiers::default() }
}
fn mv(x: f64, y: f64) -> InputEvent {
    InputEvent::PointerMove { pos: Point::new(x, y), mods: Modifiers::default() }
}
fn up(x: f64, y: f64) -> InputEvent {
    InputEvent::PointerUp { pos: Point::new(x, y), button: PointerButton::Primary, mods: Modifiers::default() }
}
fn arrow_end(ed: &Ed, id: &ElementId) -> Point {
    let a = ed.scene().get(id).unwrap();
    let ElementKind::Arrow(d) = &a.kind else { panic!() };
    let l = *d.points.last().unwrap();
    Point::new(a.x + l.x, a.y + l.y)
}
fn the_arrow(ed: &Ed) -> ElementId {
    ed.scene()
        .iter_live()
        .find(|e| matches!(e.kind, ElementKind::Arrow(_)))
        .unwrap()
        .id
        .clone()
}

/// Draw an arrow ending near a box's EDGE: it snaps to a gap outside the edge,
/// its bbox shrinks to match (so the box stays clickable), and it follows.
#[test]
fn edge_binding_keeps_box_clickable_and_follows() {
    let mut ed = editor();
    ed.add_element(Element::new(
        ElementId::from("box"), 1, 300.0, 200.0, 100.0, 60.0, ElementKind::Rectangle,
    )); // centre (350,230)

    ed.set_tool(Tool::Arrow);
    ed.handle(down(150.0, 230.0));
    ed.handle(mv(250.0, 230.0));
    ed.handle(mv(305.0, 230.0)); // near the LEFT edge → edge binding
    ed.handle(up(305.0, 230.0));
    let arrow = the_arrow(&ed);

    assert!(arrow_end(&ed, &arrow).x < 300.0, "endpoint snapped outside the edge");
    assert_eq!(
        ed.scene().topmost_at(Point::new(350.0, 230.0)),
        Some(ElementId::from("box")),
        "box clickable through the arrow's (correct) bbox"
    );

    ed.set_tool(Tool::Select);
    ed.handle(down(350.0, 230.0));
    ed.handle(up(350.0, 230.0));
    assert!(ed.selection().contains(&ElementId::from("box")));
    ed.handle(down(350.0, 230.0));
    ed.handle(mv(470.0, 230.0));
    ed.handle(up(470.0, 230.0));

    let box_el = ed.scene().get(&ElementId::from("box")).unwrap();
    assert!((box_el.x - 420.0).abs() < 1.0, "box moved +120, x={}", box_el.x);
    let end = arrow_end(&ed, &arrow);
    assert!(end.x > 380.0 && end.x < 470.0, "endpoint followed to the moved edge, end={end:?}");
}

/// Draw an arrow into the CENTRE of a box: it anchors to that interior point
/// (lives at the centre), not the edge, and follows the box keeping that point.
#[test]
fn interior_binding_lives_at_center_and_follows() {
    let mut ed = editor();
    ed.add_element(Element::new(
        ElementId::from("box"), 1, 300.0, 200.0, 100.0, 60.0, ElementKind::Rectangle,
    )); // centre (350,230)

    ed.set_tool(Tool::Arrow);
    ed.handle(down(150.0, 230.0));
    ed.handle(mv(250.0, 230.0));
    ed.handle(mv(350.0, 230.0)); // drop on the CENTRE
    ed.handle(up(350.0, 230.0));
    let arrow = the_arrow(&ed);

    // The endpoint lives at the centre (it did NOT jump to the edge).
    let end = arrow_end(&ed, &arrow);
    assert!((end.x - 350.0).abs() < 2.0 && (end.y - 230.0).abs() < 2.0, "endpoint at centre, end={end:?}");
    // It's a fixed-point binding.
    let a = ed.scene().get(&arrow).unwrap();
    let ElementKind::Arrow(d) = &a.kind else { panic!() };
    assert!(d.end_binding.as_ref().unwrap().fixed_point.is_some(), "interior fixed-point binding");

    // Grab the box by a spot away from the arrow line and drag it +120.
    ed.set_tool(Tool::Select);
    ed.handle(down(320.0, 212.0));
    ed.handle(up(320.0, 212.0));
    assert!(ed.selection().contains(&ElementId::from("box")), "box clickable away from the arrow");
    ed.handle(down(320.0, 212.0));
    ed.handle(mv(440.0, 212.0));
    ed.handle(up(440.0, 212.0));

    // The endpoint kept its interior position → now the new centre (470,230).
    let end = arrow_end(&ed, &arrow);
    assert!((end.x - 470.0).abs() < 2.0 && (end.y - 230.0).abs() < 2.0, "endpoint followed to new centre, end={end:?}");
}
