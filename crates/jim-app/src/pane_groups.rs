//! Named pane groups — reveal a set of live panes by name.
//!
//! A [`jim_pane::PaneGroup`] pane is hidden until its group name is in
//! [`VisibleGroups`]. Nothing is spawned or despawned on reveal: the panes
//! are already alive, so a terminal in a group keeps its shell and a widget
//! keeps its fetched data between reveals. Showing a group costs one
//! `Visibility` write per pane.
//!
//! ## Why this exists
//!
//! Pane visibility used to have exactly one input: `(project, canvas
//! level)`, decided in [`crate::projects::sync_visibility`]. That is a
//! *navigation* model — to see a different set of panes you go somewhere
//! else. A presentation deck needs the opposite: reveal a dashboard **over**
//! the slide you're presenting from, without navigating away from it and
//! without the several-second cost of respawning a terminal, re-fetching a
//! repo, or re-running a query mid-demo.
//!
//! Nested canvases were the near-miss alternative (a dashboard = a named
//! child canvas, reveal = `CanvasNav::descend`). It's free, but descending
//! hides *everything* on the parent level — including the deck pane itself —
//! and hijacks the breadcrumb/Cmd+Up navigation the presenter still wants.
//! `PaneGroup` is orthogonal to canvas level, which is the property that
//! actually matters here.
//!
//! ## Driving it
//!
//! Two ways in, both landing on the same resource:
//!
//! - **The bus**, topic `pane.groups`, payload `{"show": ["name", …]}`.
//!   This is the live path: `deck.ft` publishes it (retained) as slides
//!   advance. The topic is deliberately generic — the app never learns
//!   what a "deck" is, it just reveals names.
//! - **`jimctl group`** — assign membership, and show/hide by hand while
//!   building a deck.

use std::collections::HashSet;

use bevy::prelude::*;
use jim_pane::{PaneGroup, PaneTag};

/// Group names currently revealed. Empty = only ungrouped panes show.
///
/// Replaced wholesale rather than toggled: a `pane.groups` message states
/// the complete set that should be visible, so a slide that names no
/// dashboard hides the previous one without needing an explicit "hide".
#[derive(Resource, Default, Debug, Clone)]
pub struct VisibleGroups(pub HashSet<String>);

impl VisibleGroups {
    pub fn is_visible(&self, group: &str) -> bool {
        self.0.contains(group)
    }

    /// Replace the visible set. Returns true if anything changed, so
    /// callers can skip redundant work.
    pub fn set(&mut self, names: impl IntoIterator<Item = String>) -> bool {
        let next: HashSet<String> = names.into_iter().filter(|n| !n.is_empty()).collect();
        if next == self.0 {
            return false;
        }
        self.0 = next;
        true
    }
}

/// The bus topic that sets the visible group set.
pub const TOPIC: &str = "pane.groups";

pub struct PaneGroupsPlugin;

impl Plugin for PaneGroupsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VisibleGroups>()
            .add_systems(Update, apply_bus_group_messages);
    }
}

/// Apply `pane.groups` bus messages to [`VisibleGroups`].
///
/// Only *delivered* messages surface as `BusMessageObserved` — the retained
/// backlog replayed to a late-joining widget does not. That's why a
/// publisher must announce its current state on start rather than relying
/// on retain alone to survive a GUI restart (`deck.ft` does exactly this in
/// `on_start`).
fn apply_bus_group_messages(
    mut observed: MessageReader<jim_widget::BusMessageObserved>,
    mut visible: ResMut<VisibleGroups>,
) {
    for msg in observed.read() {
        if msg.topic != TOPIC {
            continue;
        }
        let names: Vec<String> = msg
            .payload
            .get("show")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if visible.set(names) {
            info!("[groups] visible: {:?}", visible.0);
        }
    }
}

/// Assign / clear a pane's group. `None` removes it from every group.
pub fn set_group(world: &mut World, entity: Entity, group: Option<String>) {
    match group {
        Some(name) => {
            world.entity_mut(entity).insert(PaneGroup(name));
        }
        None => {
            world.entity_mut(entity).remove::<PaneGroup>();
        }
    }
}

/// Every (entity, title, group) triple, for `jimctl group list`.
pub fn list(world: &mut World) -> Vec<(Entity, String, Option<String>)> {
    let mut q =
        world.query_filtered::<(Entity, &jim_pane::PaneTitle, Option<&PaneGroup>), With<PaneTag>>();
    q.iter(world)
        .map(|(e, t, g)| (e, t.0.clone(), g.map(|g| g.0.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_the_whole_visible_set() {
        let mut v = VisibleGroups::default();
        assert!(v.set(["a".to_string(), "b".to_string()]));
        assert!(v.is_visible("a") && v.is_visible("b"));

        // A slide naming a different dashboard hides the previous one —
        // the message states the complete set, it doesn't accumulate.
        assert!(v.set(["c".to_string()]));
        assert!(!v.is_visible("a"), "old group must drop out");
        assert!(v.is_visible("c"));

        // A slide naming no dashboard hides everything.
        assert!(v.set(Vec::<String>::new()));
        assert!(v.0.is_empty());
    }

    /// The deck sends `dashboard: ""` for a slide with no dashboard, and
    /// that must not create a group literally named empty-string.
    #[test]
    fn empty_names_are_dropped() {
        let mut v = VisibleGroups::default();
        v.set(["".to_string(), "real".to_string()]);
        assert_eq!(v.0.len(), 1);
        assert!(v.is_visible("real"));
    }

    /// No-op updates report false so callers can skip redundant work —
    /// this runs on every bus message, including a deck's re-publish.
    #[test]
    fn setting_the_same_names_reports_no_change() {
        let mut v = VisibleGroups::default();
        assert!(v.set(["a".to_string()]));
        assert!(!v.set(["a".to_string()]), "same set = no change");
    }
}
