//! Window-filling presentation ownership for markdown deck widgets.

use bevy::prelude::*;
use jim_pane::{PaneRect, PaneScreenAnchored, PaneTag};

#[derive(Resource, Default)]
pub struct Presentation {
    deck: Option<Entity>,
    candidate: Option<Entity>,
    saved_deck: Option<Entity>,
    saved_rect: Option<PaneRect>,
}

pub struct PresentPlugin;

impl Plugin for PresentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Presentation>().add_systems(
            Update,
            (observe_decks, presentation_keys, apply_presentation).chain(),
        );
    }
}

fn entity_from_widget_id(id: &str) -> Option<Entity> {
    let bits = id.strip_prefix("rw")?;
    u64::from_str_radix(bits, 16).ok().map(Entity::from_bits)
}

fn observe_decks(
    mut messages: MessageReader<jim_widget::BusMessageObserved>,
    mut presentation: ResMut<Presentation>,
) {
    for msg in messages.read() {
        if msg.topic == "deck.slide" {
            if let Some(entity) = entity_from_widget_id(&msg.sender) {
                presentation.candidate = Some(entity);
            }
        }
    }
}

fn presentation_keys(
    keys: Res<ButtonInput<KeyCode>>,
    mut presentation: ResMut<Presentation>,
    focused: Res<jim_pane::FocusedPane>,
    widgets: Query<&jim_widget::script_widget::ScriptWidget>,
) {
    if keys.just_pressed(KeyCode::F5) {
        presentation.deck = if presentation.deck.is_some() {
            None
        } else {
            focused
                .0
                .filter(|entity| {
                    widgets
                        .get(*entity)
                        .is_ok_and(|widget| widget.script_path.ends_with("deck.ft"))
                })
                .or(presentation.candidate)
        };
    }
    if presentation.deck.is_some() && keys.just_pressed(KeyCode::Escape) {
        presentation.deck = None;
        return;
    }
    let Some(deck) = presentation.deck else {
        return;
    };
    let Ok(widget) = widgets.get(deck) else {
        presentation.deck = None;
        return;
    };
    let deck_already_receives_navigation = focused.0 == Some(deck);
    for (code, name) in [
        (KeyCode::ArrowRight, "ArrowRight"),
        (KeyCode::ArrowDown, "ArrowDown"),
        (KeyCode::PageDown, "ArrowRight"),
        (KeyCode::Space, "ArrowRight"),
        (KeyCode::ArrowLeft, "ArrowLeft"),
        (KeyCode::ArrowUp, "ArrowUp"),
        (KeyCode::PageUp, "ArrowLeft"),
        (KeyCode::Home, "Home"),
        (KeyCode::End, "End"),
    ] {
        let normally_forwarded = matches!(
            code,
            KeyCode::ArrowRight
                | KeyCode::ArrowDown
                | KeyCode::ArrowLeft
                | KeyCode::ArrowUp
                | KeyCode::Home
                | KeyCode::End
        );
        if keys.just_pressed(code) && !(deck_already_receives_navigation && normally_forwarded) {
            widget.send_key(name);
        }
    }
}

fn apply_presentation(
    mut presentation: ResMut<Presentation>,
    windows: Query<&Window>,
    mut panes: Query<&mut PaneRect, With<PaneTag>>,
    mut commands: Commands,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    if let Some(deck) = presentation.deck {
        let size = Vec2::new(window.width(), window.height());
        if let Ok(mut rect) = panes.get_mut(deck) {
            if presentation.saved_rect.is_none() {
                presentation.saved_rect = Some(*rect);
                presentation.saved_deck = Some(deck);
            }
            *rect = PaneRect {
                pos: Vec2::ZERO,
                size,
                z: 500.0,
            };
            commands.entity(deck).insert(PaneScreenAnchored);
        }
    } else if presentation.saved_rect.is_some() {
        if let Some(deck) = presentation.saved_deck.take() {
            if let Some(saved) = presentation.saved_rect.take() {
                if let Ok(mut rect) = panes.get_mut(deck) {
                    *rect = saved;
                }
            }
            commands.entity(deck).remove::<PaneScreenAnchored>();
        } else {
            presentation.saved_rect = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_sender_decodes() {
        let e = Entity::from_raw_u32(17).unwrap();
        assert_eq!(
            entity_from_widget_id(&format!("rw{:x}", e.to_bits())),
            Some(e)
        );
    }
}
