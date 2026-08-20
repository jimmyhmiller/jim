//! What the current slide asks the app to show.
//!
//! A deck slide can hand the window to the real application — optionally
//! switching to a named project first. This module is only the record of
//! that request: it consumes the `pane.project` bus topic, resolves the
//! project name, and answers back on `pane.project.status` so the widget
//! knows whether the name meant anything. `present.rs` acts on it.
//!
//! ## Why there is no embedded view here any more
//!
//! This used to draw the target project *inside* the slide: an extra
//! `Camera2d` per guest pane, viewport-clipped to a sub-rect of the deck,
//! with a `Views` tree so input could resolve into it. It was a genuinely
//! interactive picture-in-picture, and it never stopped feeling wrong.
//!
//! The reason is structural, not a list of bugs. An embedded view is a
//! SECOND presentation of entities that already have one. That forces a
//! second camera set (double the render cost, and z between the two sets
//! decided by camera order rather than by anything meaningful), a second
//! input mapping (two answers to "what is under the cursor", so hit-testing
//! needs a view tree, a host-skip rule, and gesture latching), and a second
//! visibility rule (panes of a non-active project forced visible so the
//! copy has something to draw). Every symptom — ghosting, dead clicks,
//! z flicker, pans that stopped at arbitrary points — came out of one of
//! those three.
//!
//! Full screen already has a presentation of the app that is correct,
//! interactive and free: the app. So a slide that wants to show it hides
//! the deck instead. Zero indirection, and every one of those mechanisms
//! deletes itself.
//!
//! The consequence, and it is deliberate: in a floating pane a `project:`
//! or `application:` slide does NOTHING. "The whole project, in a pane" can
//! only be a thumbnail of what is already behind the pane.

use std::collections::HashMap;

use bevy::prelude::*;
use jim_pane::{PaneProject, PaneTag};

use crate::projects::Projects;

pub const TOPIC: &str = "pane.project";

/// What came back: the topic a host widget listens on to learn whether the
/// project it named actually resolved.
///
/// Published as `pane.project.status.<widget id>` — per host, because the
/// bus retains one payload per topic and two decks sharing a topic would
/// overwrite each other's answer. Retained, so a widget that renders before
/// the status is computed still gets it; global-channel, so it reaches the
/// asking widget whatever project it lives in; and only sent on change.
///
/// Payload: `{ host, name, found: bool, panes: usize, whole_app: bool }`.
pub const STATUS_TOPIC: &str = "pane.project.status";

/// A slide's request to hand the window to the application.
#[derive(Debug, Clone, PartialEq)]
pub struct SlideTarget {
    /// Switch to this project before stepping aside. `None` leaves the
    /// active project alone — that is what `application: true` means.
    pub project: Option<u64>,
    /// The name exactly as the slide wrote it, for diagnostics.
    pub name: String,
    /// Keep the sidebar on screen. ON unless the slide says
    /// `<!-- sidebar: false -->`.
    pub show_sidebar: bool,
}

/// The live request per publishing pane.
///
/// Keyed by host so several decks can be alive at once without fighting;
/// `present.rs` only ever reads the entry belonging to the deck that holds
/// the window.
#[derive(Resource, Default, Debug, Clone)]
pub struct SlideTargets {
    pub by_host: HashMap<Entity, SlideTarget>,
}

impl SlideTargets {
    pub fn for_host(&self, host: Entity) -> Option<&SlideTarget> {
        self.by_host.get(&host)
    }
}

/// Last status published per host, so the bus only sees real changes.
#[derive(Resource, Default)]
struct PublishedStatus(HashMap<Entity, (String, bool, usize)>);

pub struct SlideTargetPlugin;

impl Plugin for SlideTargetPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SlideTargets>()
            .init_resource::<PublishedStatus>()
            .add_systems(
                Update,
                (apply_bus_messages, forget_dead_hosts)
                    .chain()
                    .before(crate::present::PresentSet),
            );
    }
}

fn entity_from_widget_id(id: &str) -> Option<Entity> {
    let bits = id.strip_prefix("rw")?;
    u64::from_str_radix(bits, 16).ok().map(Entity::from_bits)
}

fn payload_bool(value: Option<&serde_json::Value>) -> bool {
    payload_bool_or(value, false)
}

fn payload_bool_or(value: Option<&serde_json::Value>, default: bool) -> bool {
    match value {
        Some(serde_json::Value::Bool(v)) => *v,
        Some(serde_json::Value::String(v)) => matches!(v.as_str(), "true" | "yes" | "1"),
        _ => default,
    }
}

/// Decide what a `pane.project` payload asks for.
///
/// `None` clears the host's request. An empty name with `whole_app` set is
/// "the app as it stands"; a named project is "switch there first". A name
/// that doesn't resolve is refused rather than silently treated as the
/// active project — pointing a slide at a typo must not look like success.
fn target_from(
    name: &str,
    whole_app: bool,
    show_sidebar: bool,
    resolve: impl Fn(&str) -> Option<u64>,
) -> Result<Option<SlideTarget>, ()> {
    if name.is_empty() {
        if !whole_app {
            return Ok(None);
        }
        return Ok(Some(SlideTarget {
            project: None,
            name: String::new(),
            show_sidebar,
        }));
    }
    match resolve(name) {
        Some(project) => Ok(Some(SlideTarget {
            project: Some(project),
            name: name.to_string(),
            show_sidebar,
        })),
        None => Err(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_bus_messages(
    mut observed: MessageReader<jim_widget::BusMessageObserved>,
    mut targets: ResMut<SlideTargets>,
    mut published: ResMut<PublishedStatus>,
    mut bus: ResMut<jim_widget::WidgetMsgBus>,
    projects: Res<Projects>,
    hosts: Query<(), With<PaneTag>>,
    panes: Query<&PaneProject, With<PaneTag>>,
) {
    for msg in observed.read() {
        if msg.topic != TOPIC {
            continue;
        }
        let Some(host) = entity_from_widget_id(&msg.sender) else {
            warn!("[slide] sender `{}` is not a widget pane", msg.sender);
            continue;
        };
        if hosts.get(host).is_err() {
            continue;
        }
        let name = msg
            .payload
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let whole_app = payload_bool(msg.payload.get("whole_app"));
        // Defaults ON. "Show me the whole application" without its sidebar
        // is not the whole application — that was the wrong default.
        let show_sidebar = payload_bool_or(msg.payload.get("sidebar"), true);

        match target_from(name, whole_app, show_sidebar, |n| {
            projects.list.iter().find(|p| p.name == n).map(|p| p.id)
        }) {
            Ok(None) => {
                targets.by_host.remove(&host);
                published.0.remove(&host);
            }
            Ok(Some(target)) => {
                let panes_in_target = target
                    .project
                    .map(|id| panes.iter().filter(|p| p.0 == id).count())
                    .unwrap_or(0);
                publish_status(
                    &mut bus,
                    &mut published,
                    host,
                    &target.name,
                    true,
                    panes_in_target,
                    target.project.is_none(),
                );
                targets.by_host.insert(host, target);
            }
            Err(()) => {
                warn!("[slide] no project named `{name}`");
                publish_status(&mut bus, &mut published, host, name, false, 0, false);
                targets.by_host.remove(&host);
            }
        }
    }
}

/// Drop requests whose publishing pane is gone. Nothing else expires a
/// request: a host that is merely off screen keeps its entry, because the
/// bus never replays a retained message as `BusMessageObserved` and a deck
/// only publishes on start and on slide change — so a request dropped while
/// you were elsewhere would never come back.
fn forget_dead_hosts(mut targets: ResMut<SlideTargets>, hosts: Query<(), With<PaneTag>>) {
    let dead: Vec<Entity> = targets
        .by_host
        .keys()
        .copied()
        .filter(|host| hosts.get(*host).is_err())
        .collect();
    for host in dead {
        targets.by_host.remove(&host);
    }
}

fn publish_status(
    bus: &mut jim_widget::WidgetMsgBus,
    published: &mut PublishedStatus,
    host: Entity,
    name: &str,
    found: bool,
    panes: usize,
    whole_app: bool,
) {
    let now = (name.to_string(), found, panes);
    if published.0.get(&host) == Some(&now) {
        return;
    }
    published.0.insert(host, now);
    let widget_id = format!("rw{:x}", host.to_bits());
    bus.push_external(jim_widget::PendingMsg {
        project: None,
        topic: format!("{STATUS_TOPIC}.{widget_id}"),
        payload: serde_json::json!({
            "host": widget_id,
            "name": name,
            "found": found,
            "panes": panes,
            "whole_app": whole_app,
        }),
        sender: "slide-target".to_string(),
        retain: true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(name: &str) -> Option<u64> {
        match name {
            "Recursion" => Some(1),
            "Metaphysics" => Some(10),
            _ => None,
        }
    }

    #[test]
    fn widget_id_round_trips_to_an_entity() {
        let e = Entity::from_raw_u32(7).expect("valid entity");
        assert_eq!(
            entity_from_widget_id(&format!("rw{:x}", e.to_bits())),
            Some(e)
        );
    }

    /// A plain slide clears the request, so the deck comes back.
    #[test]
    fn an_ordinary_slide_clears_the_request() {
        assert_eq!(target_from("", false, false, resolve), Ok(None));
    }

    /// `application: true` hands over the window without switching project.
    #[test]
    fn an_application_slide_does_not_switch_project() {
        let t = target_from("", true, false, resolve)
            .expect("valid")
            .expect("a target");
        assert_eq!(t.project, None);
    }

    /// `project: Name` switches first, then hands over.
    #[test]
    fn a_project_slide_resolves_the_name() {
        let t = target_from("Metaphysics", false, false, resolve)
            .expect("valid")
            .expect("a target");
        assert_eq!(t.project, Some(10));
    }

    /// A typo must be refused, not silently shown as the active project.
    #[test]
    fn an_unknown_project_is_an_error_not_a_fallback() {
        assert_eq!(target_from("Recursoin", false, false, resolve), Err(()));
    }

    /// "The whole application" includes its sidebar. Defaulting it off made
    /// an `application:` slide show a Jim with no sidebar, which is not the
    /// application.
    #[test]
    fn the_sidebar_is_on_unless_a_slide_turns_it_off() {
        let on = target_from("", true, true, resolve).unwrap().unwrap();
        assert!(on.show_sidebar);
        let off = target_from("", true, false, resolve).unwrap().unwrap();
        assert!(!off.show_sidebar);
    }

    #[test]
    fn the_sidebar_preference_rides_along() {
        let t = target_from("Recursion", false, true, resolve)
            .expect("valid")
            .expect("a target");
        assert!(t.show_sidebar);
    }
}
