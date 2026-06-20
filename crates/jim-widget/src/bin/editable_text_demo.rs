//! editable_text_demo — exploration of Bevy 0.19's native `EditableText`.
//!
//! 0.19 moved text shaping to parley and shipped `EditableText`: a headless
//! editing model wrapping a `parley::PlainEditor` with Unicode-correct cursor
//! motion, grapheme-aware delete, word motion, selection, and IME/clipboard
//! hooks. It produces layout but deliberately does NOT render itself — the
//! host draws the glyphs (exactly how `jim-editor`'s markdown readback already
//! consumes `TextLayoutInfo`). This demo proves the model end-to-end on our
//! stack: we feed `TextEdit`s from raw `KeyboardInput` (the job the
//! bevy_feathers/bevy_ui_widgets input layer would normally do) and mirror the
//! live `editor.text()` into a `Text2d` so the edits are visible.
//!
//! This is the smallest faithful integration surface for wiring native text
//! editing into a real jim pane: own the keyboard routing (jim already does,
//! via `compute_keyboard_owner`), queue `TextEdit`s, and render the resulting
//! `TextLayoutInfo`. Run it standalone:  `cargo run --bin editable_text_demo`.

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::text::{EditableText, FontSize, TextEdit};

/// Tags the `Text2d` that mirrors the editor's current value.
#[derive(Component)]
struct Mirror;

/// Holds the logical editor entity so input/sync systems can find it.
#[derive(Resource)]
struct Editor(Entity);

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Bevy 0.19 — native EditableText demo".into(),
                resolution: (900u32, 520u32).into(),
                ..default()
            }),
            ..default()
        }))
        .insert_resource(ClearColor(Color::srgb(0.09, 0.10, 0.12)))
        .add_systems(Startup, setup)
        .add_systems(Update, (keyboard_to_edits, mirror_value).chain())
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);

    // The headless editing model. Its required components (TextLayout,
    // TextFont, TextColor, LineHeight, FontHinting) are auto-inserted.
    let editor = commands
        .spawn((
            EditableText::new("Type here. Editing runs through parley's\nPlainEditor: Unicode-correct, multiline."),
            TextFont {
                font: FontSource::Monospace,
                font_size: FontSize::Px(26.0),
                ..default()
            },
        ))
        .id();
    commands.insert_resource(Editor(editor));

    // Static help line.
    commands.spawn((
        Text2d::new("EditableText (Bevy 0.19) — type / Backspace / Delete / Enter"),
        TextFont {
            font: FontSource::SansSerif,
            font_size: FontSize::Px(16.0),
            ..default()
        },
        TextColor(Color::srgb(0.5, 0.55, 0.62)),
        Transform::from_xyz(0.0, 210.0, 0.0),
    ));

    // The mirror that actually shows the editor's live text.
    commands.spawn((
        Mirror,
        Text2d::new(String::new()),
        TextFont {
            font: FontSource::Monospace,
            font_size: FontSize::Px(26.0),
            ..default()
        },
        TextColor(Color::srgb(0.92, 0.94, 0.97)),
        TextLayout::justify(Justify::Center),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));
}

/// Translate raw keyboard events into `TextEdit`s and queue them on the
/// editor. This is the glue jim's own keyboard-owner machinery would supply
/// in a real pane; here we do it directly so the demo is self-contained.
fn keyboard_to_edits(
    mut keys: MessageReader<KeyboardInput>,
    held: Res<ButtonInput<KeyCode>>,
    editor: Res<Editor>,
    mut q: Query<&mut EditableText>,
) {
    let Ok(mut editable) = q.get_mut(editor.0) else {
        return;
    };
    let sel = held.pressed(KeyCode::ShiftLeft) || held.pressed(KeyCode::ShiftRight);
    let word = held.pressed(KeyCode::AltLeft) || held.pressed(KeyCode::AltRight);
    for ev in keys.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        let edit = match &ev.logical_key {
            Key::Character(s) => Some(TextEdit::Insert(s.clone())),
            Key::Space => Some(TextEdit::Insert(" ".into())),
            Key::Enter => Some(TextEdit::Insert("\n".into())),
            Key::Backspace if word => Some(TextEdit::BackspaceWord),
            Key::Backspace => Some(TextEdit::Backspace),
            Key::Delete if word => Some(TextEdit::DeleteWord),
            Key::Delete => Some(TextEdit::Delete),
            Key::ArrowLeft if word => Some(TextEdit::WordLeft(sel)),
            Key::ArrowRight if word => Some(TextEdit::WordRight(sel)),
            Key::ArrowLeft => Some(TextEdit::Left(sel)),
            Key::ArrowRight => Some(TextEdit::Right(sel)),
            Key::ArrowUp => Some(TextEdit::Up(sel)),
            Key::ArrowDown => Some(TextEdit::Down(sel)),
            Key::Home => Some(TextEdit::LineStart(sel)),
            Key::End => Some(TextEdit::LineEnd(sel)),
            _ => None,
        };
        if let Some(edit) = edit {
            editable.queue_edit(edit);
        }
    }
}

/// Copy the editor's current value into the visible `Text2d` whenever it
/// changes. `apply_text_edits` (registered by `TextPlugin`) has already
/// applied this frame's queued edits by the time `Update` reads them back.
fn mirror_value(
    editor: Res<Editor>,
    editables: Query<&EditableText, Changed<EditableText>>,
    mut mirror: Query<&mut Text2d, With<Mirror>>,
) {
    let Ok(editable) = editables.get(editor.0) else {
        return;
    };
    let Ok(mut text) = mirror.single_mut() else {
        return;
    };
    text.0 = editable.value().to_string();
}
