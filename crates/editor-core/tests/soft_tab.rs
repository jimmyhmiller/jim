use editor_core::commands::insert_soft_tab;
use editor_core::selection::{Range, Selection};
use editor_core::state::EditorState;

fn tab(text: &str, sel: Selection, unit: &str) -> (String, usize) {
    let state = EditorState::new(ropey::Rope::from_str(text), sel).with_indent_unit(unit);
    let tr = insert_soft_tab(&state).expect("tab applies");
    let next = state.apply(&tr);
    (next.doc.to_string(), next.selection.primary_range().head)
}

#[test]
fn pads_to_the_next_stop() {
    assert_eq!(tab("", Selection::cursor(0), "    "), ("    ".into(), 4));
    assert_eq!(tab("ab", Selection::cursor(2), "    "), ("ab  ".into(), 4));
    assert_eq!(tab("abcd", Selection::cursor(4), "    "), ("abcd    ".into(), 8));
}

#[test]
fn replaces_a_single_line_selection() {
    let sel = Selection::single(Range::new(1, 3));
    assert_eq!(tab("abcd", sel, "    "), ("a   d".into(), 4));
}

#[test]
fn indents_a_multiline_selection_as_a_block() {
    let sel = Selection::single(Range::new(0, 5));
    assert_eq!(tab("one\ntwo", sel, "    ").0, "    one\n    two");
}

#[test]
fn tab_unit_inserts_a_tab() {
    assert_eq!(tab("ab", Selection::cursor(2), "\t"), ("ab\t".into(), 3));
}
