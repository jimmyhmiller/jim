//! Drive the editor through real pointer gestures and print the resulting live
//! elements as JSON — used to seed a whiteboard pane for visual verification.

use whiteboard_core::editor::Editor;
use whiteboard_core::element::Element;
use whiteboard_core::interaction::{InputEvent, Modifiers, PointerButton, Tool};
use whiteboard_core::text::MonospaceMeasurer;
use whiteboard_core::shape::RoughGenerator;
use whiteboard_core::Point;

type Ed = Editor<MonospaceMeasurer, RoughGenerator>;

fn down(e: &mut Ed, x: f64, y: f64) {
    e.handle(InputEvent::PointerDown {
        pos: Point::new(x, y),
        button: PointerButton::Primary,
        mods: Modifiers::default(),
    });
}
fn mv(e: &mut Ed, x: f64, y: f64) {
    e.handle(InputEvent::PointerMove {
        pos: Point::new(x, y),
        mods: Modifiers::default(),
    });
}
fn up(e: &mut Ed, x: f64, y: f64) {
    e.handle(InputEvent::PointerUp {
        pos: Point::new(x, y),
        button: PointerButton::Primary,
        mods: Modifiers::default(),
    });
}

fn main() {
    let mut e = Editor::new_rough(MonospaceMeasurer::default());

    // Rectangle.
    e.set_tool(Tool::Rectangle);
    down(&mut e, 60.0, 60.0);
    mv(&mut e, 220.0, 160.0);
    up(&mut e, 220.0, 160.0);

    // Ellipse.
    e.set_tool(Tool::Ellipse);
    down(&mut e, 280.0, 70.0);
    mv(&mut e, 420.0, 190.0);
    up(&mut e, 420.0, 190.0);

    // Arrow.
    e.set_tool(Tool::Arrow);
    down(&mut e, 80.0, 260.0);
    mv(&mut e, 300.0, 320.0);
    up(&mut e, 300.0, 320.0);

    // Freehand squiggle.
    e.set_tool(Tool::Freedraw);
    down(&mut e, 320.0, 280.0);
    for i in 0..40 {
        let t = i as f64;
        let x = 320.0 + t * 4.0;
        let y = 320.0 + (t * 0.5).sin() * 30.0;
        mv(&mut e, x, y);
    }
    up(&mut e, 480.0, 320.0);

    let els: Vec<&Element> = e.scene().iter_live().collect();
    println!("{}", serde_json::to_string(&els).unwrap());
}
