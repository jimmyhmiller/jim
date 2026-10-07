//! Key chord → editor action. One table shared by pane editors and
//! embedded (portal) editors, so the two can't drift apart.
//!
//! macOS bindings follow the platform text system: ⌘ moves by line /
//! document, ⌥ by word, and Ctrl gives the Emacs motions every Cocoa text
//! field has (⌃A, ⌃E, ⌃K, …). Elsewhere Ctrl plays both the ⌘ and ⌥ roles.
//!
//! Not bound on purpose: ⌘[ / ⌘] (Jim's pane focus cycling), ⇧⌘\ (the
//! cube), and anything that adds cursors — typing, paste and cut only act
//! on the primary range, so a second cursor would silently do nothing.

use bevy::input::keyboard::KeyCode;
use editor_core::commands::*;
use editor_core::state::EditorState;
use editor_core::transaction::Transaction;

pub(crate) type Cmd = fn(&EditorState) -> Option<Transaction>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// How far an Up/Down-style move goes. Resolved by the host, which knows
/// the layout (soft wrap, WYSIWYG markdown) and the viewport height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Row,
    Page,
}

#[derive(Clone, Copy)]
pub(crate) enum Action {
    /// Moves the caret/selection only; not an undo step.
    Select(Cmd),
    /// Changes the document; recorded in history.
    Edit(Cmd),
    /// Up (`dir < 0`) or down, by `step`; `extend` keeps the anchor.
    Vertical { step: Step, dir: i32, extend: bool },
    /// Open a new line below the current one, wherever the caret is.
    InsertLineBelow,
    Undo,
    Redo,
    Copy,
    Cut,
    Paste,
    Save,
    /// ⌘/ — comment toggle, or the raw/rendered switch in a markdown pane.
    ToggleComment,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Select(c) => write!(f, "Select({:p})", *c as *const ()),
            Action::Edit(c) => write!(f, "Edit({:p})", *c as *const ()),
            Action::Vertical { step, dir, extend } => {
                write!(f, "Vertical({step:?}, {dir}, extend={extend})")
            }
            Action::InsertLineBelow => f.write_str("InsertLineBelow"),
            Action::Undo => f.write_str("Undo"),
            Action::Redo => f.write_str("Redo"),
            Action::Copy => f.write_str("Copy"),
            Action::Cut => f.write_str("Cut"),
            Action::Paste => f.write_str("Paste"),
            Action::Save => f.write_str("Save"),
            Action::ToggleComment => f.write_str("ToggleComment"),
        }
    }
}

/// Should an unbound key with these modifiers be typed as text? ⌥ is
/// excluded to keep the existing behaviour (⌥-letters are reserved for
/// shortcuts rather than macOS dead keys).
pub(crate) fn types_text(m: Mods) -> bool {
    !(m.meta || m.ctrl || m.alt)
}

pub(crate) fn lookup(key: KeyCode, m: Mods) -> Option<Action> {
    if cfg!(target_os = "macos") {
        lookup_mac(key, m)
    } else {
        lookup_other(key, m)
    }
}

/// Pick the selecting or moving variant of a motion.
fn motion(extend: bool, select: Cmd, cursor: Cmd) -> Option<Action> {
    Some(Action::Select(if extend { select } else { cursor }))
}

fn vertical(step: Step, dir: i32, extend: bool) -> Option<Action> {
    Some(Action::Vertical { step, dir, extend })
}

/// Bindings with no modifier beyond Shift, identical on every platform.
fn lookup_plain(key: KeyCode, shift: bool) -> Option<Action> {
    use KeyCode::*;
    match key {
        ArrowLeft => motion(shift, select_char_left, cursor_char_left),
        ArrowRight => motion(shift, select_char_right, cursor_char_right),
        ArrowUp => vertical(Step::Row, -1, shift),
        ArrowDown => vertical(Step::Row, 1, shift),
        PageUp => vertical(Step::Page, -1, shift),
        PageDown => vertical(Step::Page, 1, shift),
        Home => motion(shift, select_line_start, cursor_line_start),
        End => motion(shift, select_line_end, cursor_line_end),
        Backspace => Some(Action::Edit(delete_char_backward)),
        Delete => Some(Action::Edit(delete_char_forward)),
        Enter | NumpadEnter => Some(Action::Edit(insert_newline_and_indent)),
        Tab if shift => Some(Action::Edit(indent_less)),
        Tab => Some(Action::Edit(insert_soft_tab)),
        _ => None,
    }
}

fn lookup_mac(key: KeyCode, m: Mods) -> Option<Action> {
    use KeyCode::*;
    let s = m.shift;
    match (m.meta, m.alt, m.ctrl) {
        (false, false, false) => lookup_plain(key, s),
        // ⌘ — line/document motions and the document commands.
        (true, false, false) => match key {
            ArrowLeft => motion(s, select_line_boundary_left, cursor_line_boundary_left),
            ArrowRight => motion(s, select_line_boundary_right, cursor_line_boundary_right),
            ArrowUp => motion(s, select_doc_start, cursor_doc_start),
            ArrowDown => motion(s, select_doc_end, cursor_doc_end),
            Home => motion(s, select_doc_start, cursor_doc_start),
            End => motion(s, select_doc_end, cursor_doc_end),
            Backspace => Some(Action::Edit(delete_line_boundary_backward)),
            Delete => Some(Action::Edit(delete_line_boundary_forward)),
            Enter | NumpadEnter if !s => Some(Action::InsertLineBelow),
            KeyA if !s => Some(Action::Select(select_all)),
            KeyL if !s => Some(Action::Select(select_line)),
            KeyK if s => Some(Action::Edit(delete_line)),
            KeyZ if s => Some(Action::Redo),
            KeyZ => Some(Action::Undo),
            KeyC if !s => Some(Action::Copy),
            KeyX if !s => Some(Action::Cut),
            // ⇧⌘V is an app-global shortcut (profiler vsync toggle).
            KeyV if !s => Some(Action::Paste),
            KeyS if !s => Some(Action::Save),
            Slash if !s => Some(Action::ToggleComment),
            _ => None,
        },
        // ⌥ — word motions, and moving/duplicating lines.
        (false, true, false) => match key {
            ArrowLeft => motion(s, select_group_left, cursor_group_left),
            ArrowRight => motion(s, select_group_right, cursor_group_right),
            ArrowUp if s => Some(Action::Edit(copy_line_up)),
            ArrowDown if s => Some(Action::Edit(copy_line_down)),
            ArrowUp => Some(Action::Edit(move_line_up)),
            ArrowDown => Some(Action::Edit(move_line_down)),
            Backspace => Some(Action::Edit(delete_group_backward)),
            Delete => Some(Action::Edit(delete_group_forward)),
            _ => None,
        },
        // ⌃ — the Cocoa text-system Emacs bindings.
        (false, false, true) => match key {
            KeyA => motion(s, select_line_start, cursor_line_start),
            KeyE => motion(s, select_line_end, cursor_line_end),
            KeyF => motion(s, select_char_right, cursor_char_right),
            KeyB => motion(s, select_char_left, cursor_char_left),
            KeyN => vertical(Step::Row, 1, s),
            KeyP => vertical(Step::Row, -1, s),
            KeyD => Some(Action::Edit(delete_char_forward)),
            KeyH => Some(Action::Edit(delete_char_backward)),
            KeyK => Some(Action::Edit(delete_to_line_end)),
            KeyT => Some(Action::Edit(transpose_chars)),
            KeyO => Some(Action::Edit(split_line)),
            _ => None,
        },
        _ => None,
    }
}

fn lookup_other(key: KeyCode, m: Mods) -> Option<Action> {
    use KeyCode::*;
    let s = m.shift;
    match (m.ctrl, m.alt, m.meta) {
        (false, false, false) => lookup_plain(key, s),
        (true, false, false) => match key {
            ArrowLeft => motion(s, select_group_left, cursor_group_left),
            ArrowRight => motion(s, select_group_right, cursor_group_right),
            Home => motion(s, select_doc_start, cursor_doc_start),
            End => motion(s, select_doc_end, cursor_doc_end),
            Backspace => Some(Action::Edit(delete_group_backward)),
            Delete => Some(Action::Edit(delete_group_forward)),
            Enter | NumpadEnter if !s => Some(Action::InsertLineBelow),
            KeyA if !s => Some(Action::Select(select_all)),
            KeyL if !s => Some(Action::Select(select_line)),
            KeyK if s => Some(Action::Edit(delete_line)),
            KeyZ if s => Some(Action::Redo),
            KeyZ => Some(Action::Undo),
            KeyY if !s => Some(Action::Redo),
            KeyC if !s => Some(Action::Copy),
            KeyX if !s => Some(Action::Cut),
            KeyV if !s => Some(Action::Paste),
            KeyS if !s => Some(Action::Save),
            Slash if !s => Some(Action::ToggleComment),
            _ => None,
        },
        (false, true, false) => match key {
            ArrowUp if s => Some(Action::Edit(copy_line_up)),
            ArrowDown if s => Some(Action::Edit(copy_line_down)),
            ArrowUp => Some(Action::Edit(move_line_up)),
            ArrowDown => Some(Action::Edit(move_line_down)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> Mods {
        if cfg!(target_os = "macos") {
            Mods { meta: true, ..Default::default() }
        } else {
            Mods { ctrl: true, ..Default::default() }
        }
    }

    fn same(a: Option<Action>, f: Cmd) -> bool {
        matches!(a, Some(Action::Select(g) | Action::Edit(g)) if g as usize == f as usize)
    }

    #[test]
    fn document_commands() {
        assert!(matches!(lookup(KeyCode::KeyZ, cmd()), Some(Action::Undo)));
        let redo = Mods { shift: true, ..cmd() };
        assert!(matches!(lookup(KeyCode::KeyZ, redo), Some(Action::Redo)));
        assert!(matches!(lookup(KeyCode::KeyS, cmd()), Some(Action::Save)));
        assert!(same(lookup(KeyCode::KeyL, cmd()), select_line));
        assert!(same(lookup(KeyCode::KeyK, redo), delete_line));
    }

    #[test]
    fn app_shortcuts_stay_unbound() {
        assert!(lookup(KeyCode::BracketLeft, cmd()).is_none());
        assert!(lookup(KeyCode::BracketRight, cmd()).is_none());
        let shift = Mods { shift: true, ..cmd() };
        assert!(lookup(KeyCode::KeyV, shift).is_none());
    }

    #[test]
    fn tab_and_shift_tab() {
        assert!(same(lookup(KeyCode::Tab, Mods::default()), insert_soft_tab));
        let shift = Mods { shift: true, ..Default::default() };
        assert!(same(lookup(KeyCode::Tab, shift), indent_less));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_motions() {
        let alt = Mods { alt: true, ..Default::default() };
        let ctrl = Mods { ctrl: true, ..Default::default() };
        assert!(same(lookup(KeyCode::ArrowLeft, cmd()), cursor_line_boundary_left));
        assert!(same(lookup(KeyCode::ArrowUp, cmd()), cursor_doc_start));
        assert!(same(lookup(KeyCode::ArrowLeft, alt), cursor_group_left));
        assert!(same(lookup(KeyCode::ArrowUp, alt), move_line_up));
        assert!(same(lookup(KeyCode::Backspace, cmd()), delete_line_boundary_backward));
        assert!(same(lookup(KeyCode::KeyA, ctrl), cursor_line_start));
        assert!(same(lookup(KeyCode::KeyK, ctrl), delete_to_line_end));
        let sel = Mods { shift: true, ..cmd() };
        assert!(same(lookup(KeyCode::ArrowRight, sel), select_line_boundary_right));
    }
}
