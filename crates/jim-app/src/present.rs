//! Window-filling presentation ownership for markdown deck widgets.
//!
//! F5 (`present.toggle`) hands the whole window to one deck pane. "Whole window" is meant
//! literally: the pane's chrome (title bar, close button, border, shadow)
//! is hidden and its rect is grown by the chrome insets, so the widget's
//! CONTENT — the slide — is exactly the window. A slideshow with a title
//! bar and an 8px frame around it does not read as a slideshow.
//!
//! ## Keys
//!
//! Navigation is an app-level [`Action`], not a key grab, so it is
//! rebindable from `~/.jim/keybinds.json` and listed in the palette:
//!
//! - `present.toggle` (F5) — starts a show ONLY when the focused pane is a
//!   deck. There is no "whichever deck published last" fallback: guessing
//!   picked decks in other projects, including hidden ones. Once a show is
//!   running there is exactly one deck to stop, so F5 ends it from
//!   anywhere, whatever holds focus.
//! - `present.next` / `present.prev` (⌘⇧→ / ⌘⇧←) — advance from ANYWHERE,
//!   including while a live terminal on the slide has the keyboard. That
//!   is the whole reason for a chord: a slide can embed real panes, and
//!   demoing them must not cost you the ability to advance.
//!
//! Escape is deliberately NOT a presentation key. It belongs to whatever
//! you are demoing — leaving insert mode in vim must not end your talk.
//! Plain →/↓/Space still advance while the deck itself holds focus.

use bevy::prelude::*;
use jim_pane::{MARGIN, PaneChrome, PaneChromeOverride, PaneRect, PaneScreenAnchored, PaneTag};

use crate::actions::{Action, ActionCtx, ActionRun, AppActionsExt, KeyChord};

#[derive(Resource, Default)]
pub struct Presentation {
    deck: Option<Entity>,
    saved_deck: Option<Entity>,
    saved_rect: Option<PaneRect>,
    /// The current slide is an `application:` slide: the deck hides itself
    /// and you look at the real application.
    stepped_aside: bool,
    /// Show the sidebar while stepped aside.
    show_sidebar: bool,
    /// Project that was active before the current `project:` slide
    /// switched away from it.
    ///
    /// A slide must not permanently change the app. Without this, visiting
    /// a `project: Metaphysics` slide left Metaphysics active for the rest
    /// of the talk — so an `application:` slide seen afterwards showed
    /// Metaphysics too, and two different slides rendered identically
    /// depending on which order you viewed them in. A deck has to be
    /// idempotent: a slide looks the same however you got there.
    saved_project: Option<u64>,
}

impl Presentation {
    /// The deck currently holding the window, if a talk is running.
    ///
    /// The canvas plumbing asks: while presenting, the pane-camera clip
    /// region has to open to the whole window. Otherwise a "full window"
    /// deck is laid out full width but RENDERED clipped at the sidebar's
    /// edge — the left of every slide simply missing, with the sidebar
    /// showing through where it should be.
    pub fn active(&self) -> Option<Entity> {
        self.deck
    }

    /// The deck has stepped aside for an `application:` slide.
    ///
    /// This replaces the embedded whole-application mirror. Mirroring the
    /// active project meant drawing the same entities a second time, with a
    /// second input mapping and a z-order decided by camera order — which is
    /// where the ghosting, the dead clicks and the z flicker all came from.
    /// Hiding one pane shows the real thing instead, once, fully
    /// interactive, for free.
    pub fn stepped_aside(&self) -> bool {
        self.stepped_aside
    }

    /// Should the sidebar be drawn right now?
    ///
    /// Hidden for the whole talk: it is chrome, and a slide with a sidebar
    /// down its left edge does not read as a slide. An `application:` slide
    /// can ask for it back with `<!-- sidebar: true -->` when the point of
    /// the demo is the app's own navigation.
    pub fn sidebar_visible(&self) -> bool {
        match (self.deck.is_some(), self.stepped_aside) {
            (false, _) => true,
            (true, true) => self.show_sidebar,
            (true, false) => false,
        }
    }
}

/// Chrome pieces hidden while presenting, mirroring what `jim_pane::dock`
/// does for a docked cell. Restored on exit.
fn chrome_parts(chrome: &PaneChrome) -> [Entity; 5] {
    [
        chrome.shadow,
        chrome.title_bar,
        chrome.title_text,
        chrome.title_cover,
        chrome.close_button,
    ]
}

/// Presentation systems. `slide_targets` resolves the current request
/// before this runs, so a slide change takes effect the same frame.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct PresentSet;

pub struct PresentPlugin;

impl Plugin for PresentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Presentation>()
            .add_action(TOGGLE)
            .add_action(NEXT)
            .add_action(PREV)
            .add_systems(
                Update,
                (
                    // `sync_visibility` reads `stepped_aside` to decide
                    // whether the deck is on screen, so resolve it first.
                    apply_app_slide.before(crate::projects::sync_visibility),
                    (presentation_keys, apply_presentation).chain(),
                )
                    .in_set(PresentSet),
            );
    }
}

const TOGGLE: Action = Action {
    id: "present.toggle",
    title: "Present / End Presentation",
    category: "View",
    keywords: &["slideshow", "deck", "fullscreen", "talk"],
    radial_icon: None,
    default_keys: &[KeyChord::plain(KeyCode::F5)],
    run: ActionRun::Custom(toggle_presentation),
};

const NEXT: Action = Action {
    id: "present.next",
    title: "Next Slide",
    category: "View",
    keywords: &["slide", "advance", "deck"],
    radial_icon: None,
    default_keys: &[KeyChord::cmd_shift(KeyCode::ArrowRight)],
    run: ActionRun::Custom(|ctx| nav(ctx, "ArrowRight")),
};

const PREV: Action = Action {
    id: "present.prev",
    title: "Previous Slide",
    category: "View",
    keywords: &["slide", "back", "deck"],
    radial_icon: None,
    default_keys: &[KeyChord::cmd_shift(KeyCode::ArrowLeft)],
    run: ActionRun::Custom(|ctx| nav(ctx, "ArrowLeft")),
};

/// Is this entity a deck widget (as opposed to any other funct widget)?
fn is_deck(world: &World, entity: Entity) -> bool {
    world
        .get::<jim_widget::script_widget::ScriptWidget>(entity)
        .is_some_and(|w| w.script_path.ends_with("deck.ft"))
}

fn toggle_presentation(ctx: &mut ActionCtx) {
    // Stopping needs no focus: a running show has exactly one deck, and
    // requiring focus to end it would strand you the moment you clicked
    // into a demo terminal.
    if ctx.world.resource::<Presentation>().deck.is_some() {
        ctx.world.resource_mut::<Presentation>().deck = None;
        return;
    }
    let focused = ctx.world.resource::<jim_pane::FocusedPane>().0;
    match focused.filter(|e| is_deck(ctx.world, *e)) {
        Some(deck) => ctx.world.resource_mut::<Presentation>().deck = Some(deck),
        // Loud enough to explain the no-op, quiet enough not to nag: the
        // old fallback silently presented a deck in some other project.
        None => info!("[present] no deck focused — click a deck pane, then F5"),
    }
}

/// Send a navigation key straight to the presenting deck's worker,
/// bypassing focus entirely.
fn nav(ctx: &mut ActionCtx, key: &str) {
    let Some(deck) = ctx.world.resource::<Presentation>().deck else {
        warn!("[navdbg] {key}: no presenting deck");
        return;
    };
    warn!(
        "[navdbg] {key} -> deck {deck:?} (widget: {})",
        ctx.world
            .get::<jim_widget::script_widget::ScriptWidget>(deck)
            .is_some()
    );
    if let Some(widget) = ctx
        .world
        .get::<jim_widget::script_widget::ScriptWidget>(deck)
    {
        widget.send_key(key);
    }
}

/// Keys that only apply while the DECK ITSELF holds focus: Space and
/// PageUp/PageDown, so a presenter remote works without a chord. Arrows
/// and Home/End are already forwarded to a focused widget by
/// `script_widget::forward_keys_to_workers`, so re-sending them here would
/// advance two slides per press.
///
/// Nothing is grabbed when focus is elsewhere. Clicking into a live pane on
/// a slide hands it the whole keyboard — Space types a space, Escape leaves
/// insert mode — and ⌘⇧→ still advances.
/// Resolve whether the current slide hands the window to the real app.
///
/// Only while presenting: in a floating pane an `application:` slide does
/// nothing at all. A deck pane is a few hundred pixels of canvas; "show the
/// whole application" inside it can only ever be a thumbnail of the thing
/// already behind it, which is why the mirror never felt right.
fn apply_app_slide(
    mut presentation: ResMut<Presentation>,
    targets: Res<crate::slide_targets::SlideTargets>,
    mut projects: ResMut<crate::projects::Projects>,
) {
    let target = presentation
        .deck
        .and_then(|deck| targets.for_host(deck))
        .cloned();
    let stepped_aside = target.is_some();
    let show_sidebar = target.as_ref().is_some_and(|t| t.show_sidebar);
    if presentation.stepped_aside != stepped_aside {
        presentation.stepped_aside = stepped_aside;
    }
    if presentation.show_sidebar != show_sidebar {
        presentation.show_sidebar = show_sidebar;
    }
    // `project:` switches the app for real — you are looking at the actual
    // project, not a picture of it — but only FOR THIS SLIDE. Leaving the
    // slide puts the app back, so no slide's effect outlives it.
    match target.and_then(|t| t.project) {
        Some(want) => {
            if presentation.saved_project.is_none() {
                presentation.saved_project = projects.active;
            }
            if projects.active != Some(want) {
                projects.set_active(want);
            }
        }
        None => {
            if let Some(previous) = presentation.saved_project.take() {
                if projects.active != Some(previous) {
                    projects.set_active(previous);
                }
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
    let Some(deck) = presentation.deck else {
        return;
    };
    let Ok(widget) = widgets.get(deck) else {
        presentation.deck = None;
        return;
    };
    if focused.0 != Some(deck) {
        return;
    }
    for (code, name) in [
        (KeyCode::Space, "ArrowRight"),
        (KeyCode::PageDown, "ArrowRight"),
        (KeyCode::PageUp, "ArrowLeft"),
    ] {
        if keys.just_pressed(code) {
            widget.send_key(name);
        }
    }
}

fn apply_presentation(
    mut presentation: ResMut<Presentation>,
    mut projects: ResMut<crate::projects::Projects>,
    windows: Query<&Window>,
    mut panes: Query<(&mut PaneRect, &PaneChrome), With<PaneTag>>,
    mut visibility: Query<&mut Visibility>,
    mut commands: Commands,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    if let Some(deck) = presentation.deck {
        let window_size = Vec2::new(window.width(), window.height());
        if let Ok((mut rect, chrome)) = panes.get_mut(deck) {
            let entering = presentation.saved_rect.is_none();
            if entering {
                presentation.saved_rect = Some(*rect);
                presentation.saved_deck = Some(deck);
                for part in chrome_parts(chrome) {
                    if let Ok(mut vis) = visibility.get_mut(part) {
                        *vis = Visibility::Hidden;
                    }
                }
                // title_h = 0 and no border/radius: the pane is a bare
                // surface. The rect then grows by the content inset on
                // every side (`content_area_th` insets by MARGIN all round
                // once the title bar is gone) so the slide itself — not the
                // pane around it — covers the window exactly.
                // Keep the deck rendering even while an `application:`
                // slide hides it, so it is already correct the moment the
                // next ordinary slide brings it back.
                commands
                    .entity(deck)
                    .insert(jim_widget::script_widget::RenderWhileHidden);
                commands.entity(deck).insert((
                    PaneChromeOverride {
                        title_h: 0.0,
                        corner_radius: 0.0,
                        border_width: 0.0,
                        bg: None,
                    },
                    PaneScreenAnchored,
                ));
            }
            let want = PaneRect {
                pos: Vec2::splat(-MARGIN),
                size: window_size + Vec2::splat(2.0 * MARGIN),
                z: 500.0,
            };
            if rect.pos != want.pos || rect.size != want.size || rect.z != want.z {
                *rect = want;
            }
        }
    } else if presentation.saved_rect.is_some() {
        presentation.stepped_aside = false;
        if let Some(project) = presentation.saved_project.take() {
            projects.set_active(project);
        }
        if let Some(deck) = presentation.saved_deck.take() {
            if let Some(saved) = presentation.saved_rect.take() {
                if let Ok((mut rect, _)) = panes.get_mut(deck) {
                    *rect = saved;
                }
            }
            if let Ok((_, chrome)) = panes.get(deck) {
                for part in chrome_parts(chrome) {
                    if let Ok(mut vis) = visibility.get_mut(part) {
                        *vis = Visibility::Inherited;
                    }
                }
            }
            commands
                .entity(deck)
                .remove::<PaneScreenAnchored>()
                .remove::<PaneChromeOverride>()
                .remove::<jim_widget::script_widget::RenderWhileHidden>();
        } else {
            presentation.saved_rect = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deck(n: u32) -> Entity {
        Entity::from_raw_u32(n).expect("valid entity")
    }

    fn presentation(deck: Option<Entity>, stepped_aside: bool, show_sidebar: bool) -> Presentation {
        Presentation {
            deck,
            stepped_aside,
            show_sidebar,
            ..Default::default()
        }
    }

    /// `project:` and `application:` are full-screen-only directives. In a
    /// floating pane they do nothing at all — "the whole project" inside a
    /// pane could only be a thumbnail of what is already behind it, which
    /// is the embedded mirror that never worked.
    #[test]
    fn a_slide_target_does_nothing_outside_a_presentation() {
        let p = presentation(None, false, false);
        assert!(!p.stepped_aside());
        assert!(p.sidebar_visible(), "no talk running: sidebar is normal");
    }

    /// The sidebar is chrome: gone for the whole talk, and back only when
    /// an app slide explicitly asks for it.
    #[test]
    fn the_sidebar_is_hidden_for_the_duration_of_a_talk() {
        assert!(presentation(None, false, false).sidebar_visible());
        assert!(!presentation(Some(deck(1)), false, false).sidebar_visible());
        assert!(!presentation(Some(deck(1)), true, false).sidebar_visible());
        assert!(presentation(Some(deck(1)), true, true).sidebar_visible());
    }

    /// Escape must never be bound here: it belongs to whatever is being
    /// demoed on the slide. Binding it cost you the talk every time you
    /// left insert mode in vim.
    #[test]
    fn presentation_actions_never_grab_escape() {
        for action in [TOGGLE, NEXT, PREV] {
            assert!(
                !action.default_keys.iter().any(|c| c.key == KeyCode::Escape),
                "{} binds Escape",
                action.id
            );
        }
    }

    /// Advancing must not need the deck focused — that is the entire point
    /// of a chord, since a slide can hand the keyboard to a live terminal.
    #[test]
    fn slide_navigation_is_a_modified_chord() {
        for action in [NEXT, PREV] {
            let chord = action.default_keys[0];
            assert!(
                chord.cmd && chord.shift,
                "{} must be a chord, not a bare key that a demo pane needs",
                action.id
            );
        }
    }
}
