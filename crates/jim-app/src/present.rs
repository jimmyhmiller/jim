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
//! - `present.next` / `present.prev` (⌘⇧→ / ⌘⇧←) — advance the presenting
//!   deck from anywhere, or the focused deck when it is still an ordinary
//!   pane. That is the whole reason for a chord: the same controls work
//!   before, during, and after full-screen presentation.
//!
//! Escape is deliberately NOT a presentation key. It belongs to whatever
//! you are demoing — leaving insert mode in vim must not end your talk.
//! Plain →/↓/Space still advance while the deck itself holds focus.

use std::path::{Path, PathBuf};

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
    /// The deck's presentation geometry (window-filling, chrome hidden) is
    /// currently applied. False while a slide has handed the window to the
    /// real app and the deck is back to being an ordinary pane.
    presented_geometry: bool,
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
    /// Start/stop a show from outside the keyboard.
    ///
    /// The same seam `ToggleExpose` uses: a presentation takes over the
    /// whole window, so there is otherwise no way to exercise it from a
    /// script — and the path it turns on is the one that can take the app
    /// down with a render-validation error. Verifiable beats plausible.
    pub pending_toggle: bool,
    /// Which deck [`Self::pending_toggle`] means, by title. `None` uses the
    /// focused pane, exactly like F5.
    pub pending_title: Option<String>,
    /// A scripted slide move: `"ArrowRight"` / `"ArrowLeft"`, delivered to
    /// the presenting deck's worker exactly as [`NEXT`]/[`PREV`] do.
    ///
    /// Navigation is the half of a talk that a script could not reach, and
    /// it is where the interesting transitions live: engaging a mirror
    /// mid-show is a different code path from starting on one, and only the
    /// first of those crashed.
    pub pending_nav: Option<String>,
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
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        app.insert_non_send(DeckPickChannel { tx, rx });
        app.init_resource::<Presentation>()
            .add_action(OPEN)
            .add_action(TOGGLE)
            .add_action(NEXT)
            .add_action(PREV)
            .add_systems(Update, drain_deck_picks)
            .add_systems(
                Update,
                (
                    // `sync_visibility` reads `stepped_aside` to decide
                    // whether the deck is on screen, so resolve it first.
                    apply_pending_toggle.before(apply_app_slide),
                    apply_pending_nav.before(apply_app_slide),
                    apply_app_slide.before(crate::projects::sync_visibility),
                    (presentation_keys, apply_presentation).chain(),
                )
                    .in_set(PresentSet),
            );
    }
}

const OPEN: Action = Action {
    id: "present.open",
    title: "Open Slideshow…",
    category: "View",
    keywords: &["slideshow", "deck", "talk", "presentation", "markdown", "slides"],
    radial_icon: None,
    default_keys: &[],
    run: ActionRun::Custom(action_open_slideshow),
};

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

/// The deck widget script every slideshow pane runs.
const DECK_SCRIPT: &str = "deck.ft";

/// Channel the async Open Slideshow sheet hands the chosen talk back on.
/// Same shape and reasoning as `FilePickChannel` in lib.rs: the sheet is
/// begun on the main thread, awaited off it, and drained here. NonSend
/// because both mpsc ends are `!Sync`.
struct DeckPickChannel {
    tx: std::sync::mpsc::Sender<PathBuf>,
    rx: std::sync::mpsc::Receiver<PathBuf>,
}

/// `present.open`: pick a Markdown talk and open it as a deck pane in the
/// active project. Async sheet, never the blocking `pick_file` — see
/// `action_open_file` in lib.rs for the re-entrancy crash that avoids.
fn action_open_slideshow(ctx: &mut ActionCtx) {
    let Some(tx) = ctx
        .world
        .get_non_send_resource::<DeckPickChannel>()
        .map(|c| c.tx.clone())
    else {
        return;
    };
    let dir = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("talks"))
        .filter(|talks| talks.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| ".".into());
    let fut = rfd::AsyncFileDialog::new()
        .set_directory(dir)
        .set_title("Open slideshow")
        .add_filter("Markdown", &["md", "markdown"])
        .pick_file();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            if let Some(handle) = fut.await {
                let _ = tx.send(handle.path().to_path_buf());
            }
        })
        .detach();
}

fn drain_deck_picks(
    channel: Option<NonSend<DeckPickChannel>>,
    projects: Res<crate::projects::Projects>,
    mut pending: ResMut<crate::projects::PendingActions>,
) {
    let Some(channel) = channel else { return };
    while let Ok(path) = channel.rx.try_recv() {
        let Some(project) = projects.active else {
            warn!("[present] no active project to open {} in", path.display());
            continue;
        };
        pending.new_panes.push(deck_pane_request(&path, project));
    }
}

/// A new deck pane showing the talk at `path`.
///
/// Shared by the palette action and the `open_slideshow` IPC/bus action,
/// so every way of opening a talk names its pane the same way.
pub fn deck_pane_request(path: &Path, project_id: u64) -> crate::projects::NewPaneRequest {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        warn!("[present] reading {} for its title: {e}", path.display());
        String::new()
    });
    crate::projects::NewPaneRequest {
        kind: jim_widget::script_widget::PANE_KIND,
        project_id,
        origin: None,
        size: None,
        config: serde_json::json!({
            "script": DECK_SCRIPT,
            "title": deck_title(&text, path),
            "params": { "path": path.to_string_lossy() },
        }),
    }
}

/// The talk's own `title:` from its front matter — the same key the deck
/// shows in its footer — else the file name without its extension.
fn deck_title(text: &str, path: &Path) -> String {
    front_matter_title(text).unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Slideshow".to_string())
    })
}

/// `title:` from an opening `---` … `---` block, as `deck.ft` parses it.
fn front_matter_title(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            return None;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.trim() == "title" {
                let value = value.trim();
                return (!value.is_empty()).then(|| value.to_string());
            }
        }
    }
    None
}

/// Is this entity a deck widget (as opposed to any other funct widget)?
fn is_deck(world: &World, entity: Entity) -> bool {
    world
        .get::<jim_widget::script_widget::ScriptWidget>(entity)
        .is_some_and(is_deck_widget)
}

/// Consume [`Presentation::pending_toggle`], the scripted form of F5.
///
/// Same rule as the action: starting needs a focused deck, stopping needs
/// nothing, because a running show has exactly one deck to stop.
fn apply_pending_toggle(
    mut presentation: ResMut<Presentation>,
    focused: Res<jim_pane::FocusedPane>,
    widgets: Query<&jim_widget::script_widget::ScriptWidget>,
    titled: Query<(Entity, &jim_pane::PaneTitle)>,
) {
    if !std::mem::take(&mut presentation.pending_toggle) {
        return;
    }
    let named = presentation.pending_title.take();
    if presentation.deck.is_some() {
        presentation.deck = None;
        return;
    }
    let wanted = match &named {
        Some(title) => titled
            .iter()
            .find(|(_, t)| &t.0 == title)
            .map(|(entity, _)| entity),
        None => focused.0,
    };
    match wanted.filter(|e| widgets.get(*e).is_ok_and(is_deck_widget)) {
        Some(deck) => presentation.deck = Some(deck),
        None => match named {
            Some(title) => info!("[present] no deck pane titled `{title}`"),
            None => info!("[present] no deck focused — click a deck pane first"),
        },
    }
}

/// Consume [`Presentation::pending_nav`] — the scripted form of ⌘⇧→/←.
fn apply_pending_nav(
    mut presentation: ResMut<Presentation>,
    widgets: Query<&jim_widget::script_widget::ScriptWidget>,
) {
    let Some(key) = presentation.pending_nav.take() else {
        return;
    };
    let Some(deck) = presentation.deck else {
        info!("[present] nav ignored: no talk running");
        return;
    };
    match widgets.get(deck) {
        Ok(widget) => widget.send_key(&key),
        Err(_) => info!("[present] nav ignored: the presenting deck is gone"),
    }
}

fn is_deck_widget(widget: &jim_widget::script_widget::ScriptWidget) -> bool {
    widget.script_path.ends_with(DECK_SCRIPT)
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

/// Send a navigation key to the full-screen deck, or to the focused deck
/// when it is still an ordinary pane. A running presentation wins over
/// focus because a live slide may deliberately focus an embedded terminal.
fn nav(ctx: &mut ActionCtx, key: &str) {
    let presenting = ctx.world.resource::<Presentation>().deck;
    let focused = ctx.world.resource::<jim_pane::FocusedPane>().0;
    let deck = nav_deck(presenting, focused, |entity| is_deck(ctx.world, entity));
    let Some(deck) = deck else {
        info!("[present] {key}: no presenting or focused deck");
        return;
    };
    if let Some(widget) = ctx
        .world
        .get::<jim_widget::script_widget::ScriptWidget>(deck)
    {
        widget.send_key(key);
    }
}

fn nav_deck(
    presenting: Option<Entity>,
    focused: Option<Entity>,
    is_deck: impl Fn(Entity) -> bool,
) -> Option<Entity> {
    presenting.or_else(|| focused.filter(|entity| is_deck(*entity)))
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

/// Give the deck the window, or give it back.
///
/// Two geometries, switched by `stepped_aside`:
///
/// - **Presenting** — window-filling, chrome hidden, screen-anchored. The
///   slide IS the screen.
/// - **Stepped aside** — the deck's ORDINARY pane: its saved rect on the
///   canvas, its title bar and border back, no anchoring. The show is still
///   running and the deck is still rendering; it has simply stopped
///   covering the window.
///
/// Stepping aside used to HIDE the deck instead, and that removed the one
/// thing the slide was about. An `application:`/`project:` slide says "look
/// at the real app" — and this deck's own pane is part of the real app. You
/// find it on the canvas and it is showing a slide of the app that contains
/// it. Any pane rendering this deck does the same, which is what makes it
/// recursive rather than a picture of something else.
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
        let aside = presentation.stepped_aside;
        if let Ok((mut rect, chrome)) = panes.get_mut(deck) {
            if presentation.saved_rect.is_none() {
                // The deck's own rect on the canvas, remembered once for
                // the whole show — every step aside returns to THIS, not to
                // wherever the previous slide left it.
                presentation.saved_rect = Some(*rect);
                presentation.saved_deck = Some(deck);
                // Keep rendering even when something else hides the pane,
                // so it is already correct the moment it comes back.
                commands
                    .entity(deck)
                    .insert(jim_widget::script_widget::RenderWhileHidden);
            }
            if aside && presentation.presented_geometry {
                presentation.presented_geometry = false;
                set_chrome_visible(chrome, &mut visibility, true);
                commands
                    .entity(deck)
                    .remove::<PaneScreenAnchored>()
                    .remove::<PaneChromeOverride>();
                if let Some(saved) = presentation.saved_rect {
                    *rect = saved;
                }
            } else if !aside && !presentation.presented_geometry {
                presentation.presented_geometry = true;
                set_chrome_visible(chrome, &mut visibility, false);
                // title_h = 0 and no border/radius: the pane is a bare
                // surface. The rect then grows by the content inset on
                // every side (`content_area_th` insets by MARGIN all round
                // once the title bar is gone) so the slide itself — not the
                // pane around it — covers the window exactly.
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
            if !aside {
                let want = PaneRect {
                    pos: Vec2::splat(-MARGIN),
                    size: window_size + Vec2::splat(2.0 * MARGIN),
                    z: 500.0,
                };
                if rect.pos != want.pos || rect.size != want.size || rect.z != want.z {
                    *rect = want;
                }
            }
        }
    } else if presentation.saved_rect.is_some() {
        presentation.stepped_aside = false;
        presentation.presented_geometry = false;
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
                set_chrome_visible(chrome, &mut visibility, true);
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

fn set_chrome_visible(chrome: &PaneChrome, visibility: &mut Query<&mut Visibility>, show: bool) {
    let want = if show {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for part in chrome_parts(chrome) {
        if let Ok(mut vis) = visibility.get_mut(part) {
            *vis = want;
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

    #[test]
    fn a_deck_is_titled_from_its_front_matter() {
        let text = "---\ntitle: An Editor for One\n---\n# Hi\n";
        assert_eq!(
            deck_title(text, Path::new("/t/live-view-test.md")),
            "An Editor for One"
        );
    }

    /// No front matter, or front matter without a title: the file name.
    /// A `title:` in the BODY is slide text, not the deck's name.
    #[test]
    fn an_untitled_deck_falls_back_to_its_file_name() {
        let path = Path::new("/t/live-view-test.md");
        assert_eq!(deck_title("# Hi\ntitle: nope\n", path), "live-view-test");
        assert_eq!(deck_title("---\nstyle: a.glz\n---\ntitle: nope\n", path), "live-view-test");
        assert_eq!(deck_title("---\ntitle:\n---\n", path), "live-view-test");
    }

    /// Stepping aside must NOT hide the deck. The slide is about the real
    /// app, and this deck's own pane is part of the real app — hiding it
    /// removes the thing you are meant to go and find.
    #[test]
    fn stepping_aside_keeps_the_deck_visible_and_ordinary() {
        let mut p = presentation(Some(deck(1)), true, true);
        p.presented_geometry = true;
        assert!(p.stepped_aside());
        assert!(
            p.sidebar_visible(),
            "the app you stepped aside for includes its sidebar"
        );
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

    #[test]
    fn navigation_uses_a_focused_deck_outside_full_screen() {
        let focused = deck(7);
        assert_eq!(
            nav_deck(None, Some(focused), |e| e == focused),
            Some(focused)
        );
        assert_eq!(nav_deck(None, Some(focused), |_| false), None);
    }

    #[test]
    fn presenting_deck_wins_over_focus() {
        let presenting = deck(3);
        let focused = deck(7);
        assert_eq!(
            nav_deck(Some(presenting), Some(focused), |e| e == focused),
            Some(presenting)
        );
    }
}
