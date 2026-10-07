//! Headless integration tests for the KeyboardInput -> editor-command
//! glue in `handle_input`. These exercise the *translation* between
//! Bevy key events and editor-core commands — a layer that isn't
//! covered by editor-core's own tests.
//!
//! The test App uses `MinimalPlugins + InputPlugin + HeadlessEditorPlugin`
//! and spawns a single editor entity wired up as the focused target.
//! No window, no rendering, no fonts.

use bevy::input::ButtonState;
use bevy::input::InputPlugin;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use editor_core::selection::Selection;
use editor_core::state::EditorState;
use jim_editor::highlight::Highlighter;
use jim_editor::{
    EditorHighlighter, EditorMetrics, EditorScroll, EditorStateComp, HeadlessEditorPlugin,
    LineRows, PANE_KIND, TextDragAnchor,
};
use jim_pane::{FocusedPane, PaneKindMarker, PaneRect, PaneTag};
use ropey::Rope;

fn make_app(initial: &str) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(InputPlugin);
    app.add_plugins(HeadlessEditorPlugin);
    // handle_input consults EditorMetrics for the caret-visible nudge;
    // tests never render, so a dummy cell width is fine.
    app.insert_resource(EditorMetrics { cell_width: 9.6 });
    let initial = initial.to_string();
    app.add_systems(Startup, move |mut commands: Commands| {
        let e = commands
            .spawn((
                PaneTag,
                PaneKindMarker(PANE_KIND),
                EditorStateComp(
                    EditorState::new(Rope::from_str(&initial), Selection::cursor(0))
                        .with_indent_unit("    "),
                ),
                EditorHighlighter(Highlighter::new()),
                LineRows::default(),
                EditorScroll::default(),
                TextDragAnchor::default(),
                PaneRect {
                    pos: Vec2::ZERO,
                    size: Vec2::new(800.0, 600.0),
                    z: 0.0,
                },
            ))
            .id();
        commands.insert_resource(FocusedPane(Some(e)));
    });
    // Run startup once so the entity exists before tests drive events.
    app.update();
    app
}

fn press(app: &mut App, key_code: KeyCode, logical: Key) {
    app.world_mut().write_message(KeyboardInput {
        key_code,
        logical_key: logical,
        state: ButtonState::Pressed,
        text: None,
        repeat: false,
        window: Entity::PLACEHOLDER,
    });
}

fn press_char(app: &mut App, c: char) {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    press(app, KeyCode::KeyA, Key::Character(s.into()));
}

fn read_state(app: &mut App) -> EditorState {
    let mut q = app.world_mut().query::<&EditorStateComp>();
    q.single(app.world()).expect("one editor entity").0.clone()
}

#[test]
fn typing_a_char_inserts_at_caret() {
    let mut app = make_app("");
    press_char(&mut app, 'h');
    app.update();
    press_char(&mut app, 'i');
    app.update();
    let s = read_state(&mut app);
    assert_eq!(s.doc.to_string(), "hi");
    assert_eq!(s.selection.primary_range().head, 2);
}

#[test]
fn arrow_right_moves_caret() {
    let mut app = make_app("abc");
    press(&mut app, KeyCode::ArrowRight, Key::ArrowRight);
    app.update();
    assert_eq!(read_state(&mut app).selection.primary_range().head, 1);
}

#[test]
fn end_key_jumps_to_line_end() {
    let mut app = make_app("hello\nworld");
    press(&mut app, KeyCode::End, Key::End);
    app.update();
    assert_eq!(read_state(&mut app).selection.primary_range().head, 5);
}

#[test]
fn backspace_at_caret_deletes_prior_char() {
    let mut app = make_app("abc");
    press(&mut app, KeyCode::End, Key::End);
    app.update();
    press(&mut app, KeyCode::Backspace, Key::Backspace);
    app.update();
    let s = read_state(&mut app);
    assert_eq!(s.doc.to_string(), "ab");
    assert_eq!(s.selection.primary_range().head, 2);
}

#[test]
fn alt_backspace_deletes_prior_word() {
    let mut app = make_app("one two");
    press(&mut app, KeyCode::End, Key::End);
    app.update();
    press(&mut app, KeyCode::AltLeft, Key::Alt);
    app.update();
    press(&mut app, KeyCode::Backspace, Key::Backspace);
    app.update();
    let s = read_state(&mut app);
    assert_eq!(s.doc.to_string(), "one ");
    assert_eq!(s.selection.primary_range().head, 4);
}

#[test]
fn enter_inserts_newline_and_indents() {
    let mut app = make_app("    fn foo {");
    press(&mut app, KeyCode::End, Key::End);
    app.update();
    press(&mut app, KeyCode::Enter, Key::Enter);
    app.update();
    let s = read_state(&mut app);
    assert!(s.doc.to_string().contains('\n'));
    assert!(s.selection.primary_range().head > 12);
}

#[test]
fn arrow_keys_do_not_insert_text() {
    let mut app = make_app("");
    press(&mut app, KeyCode::ArrowRight, Key::ArrowRight);
    app.update();
    assert_eq!(read_state(&mut app).doc.to_string(), "");
}

#[test]
fn key_release_does_not_trigger_command() {
    let mut app = make_app("abc");
    app.world_mut().write_message(KeyboardInput {
        key_code: KeyCode::ArrowRight,
        logical_key: Key::ArrowRight,
        state: ButtonState::Released,
        text: None,
        repeat: false,
        window: Entity::PLACEHOLDER,
    });
    app.update();
    assert_eq!(read_state(&mut app).selection.primary_range().head, 0);
}

// ---- Standard-editor bindings (keymap.rs) ----

fn hold(app: &mut App, key_code: KeyCode, logical: Key) {
    press(app, key_code, logical);
    app.update();
}

fn caret_at(app: &mut App, pos: usize) {
    let mut q = app.world_mut().query::<&mut EditorStateComp>();
    let mut sc = q.single_mut(app.world_mut()).expect("one editor entity");
    sc.0.selection = Selection::cursor(pos);
}

fn key(app: &mut App, key_code: KeyCode, logical: Key) -> EditorState {
    press(app, key_code, logical);
    app.update();
    read_state(app)
}

#[test]
fn tab_inserts_spaces_to_the_next_stop() {
    let mut app = make_app("ab");
    caret_at(&mut app, 2);
    let s = key(&mut app, KeyCode::Tab, Key::Tab);
    assert_eq!(s.doc.to_string(), "ab  ");
    assert_eq!(s.selection.primary_range().head, 4);
}

#[test]
fn shift_tab_dedents() {
    let mut app = make_app("        x");
    caret_at(&mut app, 8);
    hold(&mut app, KeyCode::ShiftLeft, Key::Shift);
    let s = key(&mut app, KeyCode::Tab, Key::Tab);
    assert_eq!(s.doc.to_string(), "    x");
}

#[test]
fn page_down_moves_many_rows() {
    let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let mut app = make_app(&text);
    let s = key(&mut app, KeyCode::PageDown, Key::PageDown);
    let line = s.doc.char_to_line(s.selection.primary_range().head);
    assert!(line > 10, "PageDown only reached line {line}");
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;

    #[test]
    fn cmd_left_goes_to_line_start() {
        let mut app = make_app("one\ntwo three");
        caret_at(&mut app, 9);
        hold(&mut app, KeyCode::SuperLeft, Key::Super);
        let s = key(&mut app, KeyCode::ArrowLeft, Key::ArrowLeft);
        assert_eq!(s.selection.primary_range().head, 4);
    }

    #[test]
    fn cmd_down_goes_to_doc_end() {
        let mut app = make_app("one\ntwo");
        hold(&mut app, KeyCode::SuperLeft, Key::Super);
        let s = key(&mut app, KeyCode::ArrowDown, Key::ArrowDown);
        assert_eq!(s.selection.primary_range().head, 7);
    }

    #[test]
    fn cmd_backspace_deletes_to_line_start() {
        let mut app = make_app("keep\ndrop this");
        caret_at(&mut app, 14);
        hold(&mut app, KeyCode::SuperLeft, Key::Super);
        let s = key(&mut app, KeyCode::Backspace, Key::Backspace);
        assert_eq!(s.doc.to_string(), "keep\n");
    }

    #[test]
    fn alt_up_moves_the_line() {
        let mut app = make_app("one\ntwo");
        caret_at(&mut app, 5);
        hold(&mut app, KeyCode::AltLeft, Key::Alt);
        let s = key(&mut app, KeyCode::ArrowUp, Key::ArrowUp);
        assert_eq!(s.doc.to_string(), "two\none");
    }

    #[test]
    fn cmd_enter_opens_a_line_below() {
        let mut app = make_app("    foo();\nbar");
        caret_at(&mut app, 5);
        hold(&mut app, KeyCode::SuperLeft, Key::Super);
        let s = key(&mut app, KeyCode::Enter, Key::Enter);
        assert_eq!(s.doc.to_string(), "    foo();\n    \nbar");
        assert_eq!(s.selection.primary_range().head, 15);
    }

    #[test]
    fn cmd_shift_k_deletes_the_line() {
        let mut app = make_app("one\ntwo\nthree");
        caret_at(&mut app, 5);
        hold(&mut app, KeyCode::SuperLeft, Key::Super);
        hold(&mut app, KeyCode::ShiftLeft, Key::Shift);
        let s = key(&mut app, KeyCode::KeyK, Key::Character("k".into()));
        assert_eq!(s.doc.to_string(), "one\nthree");
    }

    #[test]
    fn ctrl_k_kills_to_line_end() {
        let mut app = make_app("abc def\nx");
        caret_at(&mut app, 3);
        hold(&mut app, KeyCode::ControlLeft, Key::Control);
        let s = key(&mut app, KeyCode::KeyK, Key::Character("k".into()));
        assert_eq!(s.doc.to_string(), "abc\nx");
    }

    #[test]
    fn ctrl_a_does_not_type_or_select_all() {
        let mut app = make_app("abc\ndef");
        caret_at(&mut app, 6);
        hold(&mut app, KeyCode::ControlLeft, Key::Control);
        let s = key(&mut app, KeyCode::KeyA, Key::Character("a".into()));
        assert_eq!(s.doc.to_string(), "abc\ndef");
        assert_eq!(s.selection.primary_range().head, 4);
        assert!(s.selection.primary_range().is_empty());
    }

    #[test]
    fn cmd_slash_comments_by_default() {
        let mut app = make_app("x");
        hold(&mut app, KeyCode::SuperLeft, Key::Super);
        let s = key(&mut app, KeyCode::Slash, Key::Character("/".into()));
        assert_eq!(s.doc.to_string(), "// x");
    }
}
