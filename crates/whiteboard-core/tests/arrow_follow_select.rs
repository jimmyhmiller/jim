//! Regressions for arrow behavior driven through the public `Editor` API:
//! bound arrows follow a shape *live* during the drag (not only on release),
//! and line-like elements are selected by their outline, not their bounding box.

use whiteboard_core::editor::Editor;
use whiteboard_core::element::{Element, ElementId, ElementKind};
use whiteboard_core::geometry::Point;
use whiteboard_core::interaction::{InputEvent, Modifiers, PointerButton, Tool};
use whiteboard_core::text::MonospaceMeasurer;

type Ed = Editor<MonospaceMeasurer>;
fn editor() -> Ed { Editor::new(MonospaceMeasurer::default()) }
fn down(x: f64, y: f64) -> InputEvent { InputEvent::PointerDown { pos: Point::new(x,y), button: PointerButton::Primary, mods: Modifiers::default() } }
fn mv(x: f64, y: f64) -> InputEvent { InputEvent::PointerMove { pos: Point::new(x,y), mods: Modifiers::default() } }
fn up(x: f64, y: f64) -> InputEvent { InputEvent::PointerUp { pos: Point::new(x,y), button: PointerButton::Primary, mods: Modifiers::default() } }

fn arrow_end(ed: &Ed, id: &ElementId) -> Point {
    let a = ed.scene().get(id).unwrap();
    let ElementKind::Arrow(d) = &a.kind else { panic!() };
    let l = *d.points.last().unwrap();
    Point::new(a.x + l.x, a.y + l.y)
}

/// A bound arrow tracks its target *during* the move, not only after release.
#[test]
fn bound_arrow_follows_live_during_drag() {
    let mut ed = editor();
    ed.add_element(Element::new(
        ElementId::from("box"), 1, 300.0, 200.0, 100.0, 60.0, ElementKind::Rectangle,
    ));
    // Draw an arrow whose end binds to the box's left edge.
    ed.set_tool(Tool::Arrow);
    ed.handle(down(150.0, 230.0));
    ed.handle(mv(250.0, 230.0));
    ed.handle(mv(305.0, 232.0)); // onto the box's left edge so the end binds
    ed.handle(up(305.0, 232.0));
    let arrow = ed.scene().iter_live()
        .find(|e| matches!(e.kind, ElementKind::Arrow(_))).unwrap().id.clone();
    let before = arrow_end(&ed, &arrow);

    // Grab the box and drag it right; sample the arrow end MID-drag.
    ed.set_tool(Tool::Select);
    ed.handle(down(350.0, 230.0));
    ed.handle(up(350.0, 230.0));
    ed.handle(down(350.0, 230.0));
    ed.handle(mv(410.0, 230.0)); // box +60, still mid-gesture (no pointer-up yet)
    let mid = arrow_end(&ed, &arrow);
    assert!(
        mid.x > before.x + 40.0,
        "arrow end should follow the box live during the drag: before={before:?} mid={mid:?}"
    );
}

/// Clicking the visible stroke selects an arrow; clicking inside its bounding
/// box but well off the line does not.
#[test]
fn arrow_selected_by_outline_not_bounding_box() {
    let mut ed = editor();
    // A long diagonal arrow from (100,100) to (300,300). Its bbox is the whole
    // 200x200 square; (280,120) is inside that box but far from the line.
    ed.set_tool(Tool::Arrow);
    ed.handle(down(100.0, 100.0));
    ed.handle(mv(200.0, 200.0));
    ed.handle(up(300.0, 300.0));
    let arrow = ed.scene().iter_live()
        .find(|e| matches!(e.kind, ElementKind::Arrow(_))).unwrap().id.clone();

    // Click far from the line but inside the bbox: should NOT select.
    ed.set_tool(Tool::Select);
    ed.handle(down(280.0, 120.0));
    ed.handle(up(280.0, 120.0));
    assert!(
        ed.selection().is_empty(),
        "clicking inside the bbox but off the line must not select the arrow"
    );

    // Click on the line (~midpoint): should select.
    ed.handle(down(200.0, 200.0));
    ed.handle(up(200.0, 200.0));
    assert!(
        ed.selection().contains(&arrow),
        "clicking on the stroke should select the arrow"
    );
}
