//! Grabbing an existing arrow's endpoint and dragging it onto a shape should bind
//! it (and dragging it off should unbind) — Excalidraw behaviour. Driven through
//! the full `Editor` gesture API. Note: a created endpoint lands at the last
//! pointer-MOVE position (pointer-up doesn't move it), so tests place the final
//! move where the endpoint should be.

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
fn the_arrow(ed: &Ed) -> ElementId { ed.scene().iter_live().find(|e| matches!(e.kind, ElementKind::Arrow(_))).unwrap().id.clone() }
fn end_bound_to(ed: &Ed, id: &ElementId) -> Option<ElementId> {
    let a = ed.scene().get(id).unwrap();
    let ElementKind::Arrow(d) = &a.kind else { panic!() };
    d.end_binding.as_ref().map(|b| b.element_id.clone())
}

#[test]
fn drag_endpoint_onto_box_binds_it() {
    let mut ed = editor();
    ed.add_element(Element::new(ElementId::from("box"), 1, 300.0, 200.0, 100.0, 60.0, ElementKind::Rectangle));

    // Draw an arrow ending in empty space at (180,230) (last move = endpoint).
    ed.set_tool(Tool::Arrow);
    ed.handle(down(100.0, 230.0));
    ed.handle(mv(180.0, 230.0));
    ed.handle(up(180.0, 230.0));
    let arrow = the_arrow(&ed);
    assert!(end_bound_to(&ed, &arrow).is_none(), "ends in empty space → unbound");

    // Select it (click on the line), then grab its END vertex (180,230) and drag
    // onto the box centre.
    ed.set_tool(Tool::Select);
    ed.handle(down(140.0, 230.0));
    ed.handle(up(140.0, 230.0));
    assert!(ed.selection().contains(&arrow), "arrow selected");

    ed.handle(down(180.0, 230.0)); // grab endpoint
    ed.handle(mv(300.0, 230.0));
    ed.handle(mv(350.0, 230.0));   // onto the box centre
    ed.handle(up(350.0, 230.0));
    assert_eq!(end_bound_to(&ed, &arrow), Some(ElementId::from("box")), "dropping the end on the box binds it");

    // The box stays clickable (arrow bbox correct).
    assert_eq!(
        ed.scene().topmost_at(Point::new(320.0, 212.0)),
        Some(ElementId::from("box")),
        "box still clickable after the arrow bound to it"
    );
}

#[test]
fn drag_bound_endpoint_off_unbinds() {
    let mut ed = editor();
    ed.add_element(Element::new(ElementId::from("box"), 1, 300.0, 200.0, 100.0, 60.0, ElementKind::Rectangle));
    // Draw an arrow that binds to the box edge (ends at 305,230 near the left edge).
    ed.set_tool(Tool::Arrow);
    ed.handle(down(100.0, 230.0));
    ed.handle(mv(305.0, 230.0));
    ed.handle(up(305.0, 230.0));
    let arrow = the_arrow(&ed);
    assert_eq!(end_bound_to(&ed, &arrow), Some(ElementId::from("box")), "drew onto the box → bound");

    // Grab the bound endpoint and drag it far into empty space → unbinds.
    ed.set_tool(Tool::Select);
    ed.handle(down(140.0, 230.0));
    ed.handle(up(140.0, 230.0));
    assert!(ed.selection().contains(&arrow), "selected");
    let end = {
        let a = ed.scene().get(&arrow).unwrap();
        let ElementKind::Arrow(d) = &a.kind else { panic!() };
        let l = *d.points.last().unwrap();
        Point::new(a.x + l.x, a.y + l.y)
    };
    ed.handle(down(end.x, end.y));
    ed.handle(mv(180.0, 100.0));
    ed.handle(mv(150.0, 80.0));
    ed.handle(up(150.0, 80.0));
    assert!(end_bound_to(&ed, &arrow).is_none(), "dragging the end into empty space unbinds it");
}
