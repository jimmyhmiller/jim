//! A single selected arrow/line must show only endpoint handles — NOT the
//! bounding-box-with-resize-handles overlay that shapes get (Excalidraw parity:
//! "the annoying box around arrows" must not appear).

use whiteboard_core::editor::Editor;
use whiteboard_core::element::{Element, ElementId, ElementKind, LinearData};
use whiteboard_core::geometry::Point;
use whiteboard_core::text::MonospaceMeasurer;

type Ed = Editor<MonospaceMeasurer>;
fn editor() -> Ed { Editor::new(MonospaceMeasurer::default()) }

fn overlay_extra(ed: &Ed) -> usize {
    let base = ed.render().commands.len();
    let over = ed.render_with_overlay().commands.len();
    over - base
}

#[test]
fn arrow_selection_has_no_bounding_box() {
    // Rectangle: selecting it draws the full box + resize/rotation handles.
    let mut ed = editor();
    ed.add_element(Element::new(ElementId::from("r"), 1, 100.0, 100.0, 80.0, 50.0, ElementKind::Rectangle));
    ed.select([ElementId::from("r")]);
    let rect_extra = overlay_extra(&ed);
    assert!(rect_extra >= 8, "a selected rectangle should draw a box + handles, got {rect_extra}");

    // Arrow: selecting it draws only its endpoint dots — far fewer commands, and
    // no enclosing box.
    let mut ed = editor();
    let data = LinearData::arrow(vec![Point::new(0.0, 0.0), Point::new(120.0, 40.0)]);
    ed.add_element(Element::new(ElementId::from("a"), 1, 100.0, 100.0, 120.0, 40.0, ElementKind::Arrow(data)));
    ed.select([ElementId::from("a")]);
    let arrow_extra = overlay_extra(&ed);

    assert!(arrow_extra > 0, "a selected arrow should still show endpoint handles");
    assert!(
        arrow_extra < rect_extra,
        "a selected arrow must NOT draw the bounding box: arrow_extra={arrow_extra} rect_extra={rect_extra}"
    );
    // Endpoint dots only: 2 vertices × (fill + stroke) = 4 commands. Allow slack.
    assert!(arrow_extra <= 6, "expected only endpoint handles for an arrow, got {arrow_extra}");
}
