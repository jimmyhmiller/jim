//! Keyed animation store for discrete-state style transitions (Glaze
//! `transition <state> <duration> [easing]`).
//!
//! The widget tree is event-driven and fully despawned/respawned on each
//! render, so animation state cannot live on entities. It lives here instead,
//! keyed by `(pane, element_id, state)`:
//!
//! - Render arms read the current eased value through the per-pane
//!   [`AnimSnapshot`] in `LayoutCtx` and push an [`AnimRequest`] declaring the
//!   state's target + transition (collected in `WidgetTargets.anims`).
//! - After each render the host syncs requests into [`WidgetAnim`]. A new key
//!   is inserted *at* its target (no animate-on-first-appearance); an existing
//!   key whose target changed starts animating.
//! - [`tick_widget_anims`] advances in-flight values each frame and flips the
//!   owning pane's `force_render`, so the pane re-renders (and repaints with
//!   the new eased value) only while something is actually animating.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use glaze::Easing;

/// One animated state value. `linear` is raw 0..1 progress toward `target`;
/// the eased output is `easing.apply(linear)`.
#[derive(Debug, Clone)]
pub struct AnimEntry {
    pub linear: f32,
    pub target: f32,
    pub duration_ms: f32,
    pub easing: Easing,
}

impl AnimEntry {
    pub fn eased(&self) -> f32 {
        self.easing.apply(self.linear)
    }
    pub fn in_flight(&self) -> bool {
        self.linear != self.target
    }
}

/// A render arm's declaration for one animated state: "element `element_id`'s
/// `state` is now `target`; changes animate per this transition".
#[derive(Debug, Clone)]
pub struct AnimRequest {
    pub element_id: String,
    pub state: String,
    pub target: f32,
    pub duration_ms: f32,
    pub easing: Easing,
}

/// The session-wide animation store. Survives widget re-renders (full
/// despawn/respawn); entries are dropped when their pane despawns.
#[derive(Resource, Default)]
pub struct WidgetAnim {
    entries: HashMap<(Entity, String, String), AnimEntry>,
}

impl WidgetAnim {
    /// True while any transition is mid-flight. Hosts that run winit in a
    /// reactive update mode use this to flip to `Continuous` for the
    /// animation's duration (otherwise the tween only advances on the next
    /// input/timeout wake).
    pub fn any_in_flight(&self) -> bool {
        self.entries.values().any(AnimEntry::in_flight)
    }

    /// Current eased value for an element's state, if it's tracked.
    pub fn eased(&self, pane: Entity, element_id: &str, state: &str) -> Option<f32> {
        self.entries
            .get(&(pane, element_id.to_string(), state.to_string()))
            .map(AnimEntry::eased)
    }

    /// Snapshot this pane's eased values for the render pass.
    pub fn snapshot_for(&self, pane: Entity) -> AnimSnapshot {
        let mut values = HashMap::new();
        for ((p, id, state), e) in &self.entries {
            if *p == pane {
                values.insert((id.clone(), state.clone()), e.eased());
            }
        }
        AnimSnapshot { values }
    }

    /// Sync a render pass's requests into the store. New keys are inserted
    /// already at their target (an element's first appearance never animates);
    /// existing keys pick up the new target/transition and animate from
    /// wherever they currently are.
    pub fn apply_requests(&mut self, pane: Entity, requests: &[AnimRequest]) {
        for r in requests {
            let key = (pane, r.element_id.clone(), r.state.clone());
            match self.entries.get_mut(&key) {
                Some(e) => {
                    e.target = r.target;
                    e.duration_ms = r.duration_ms;
                    e.easing = r.easing;
                    if e.duration_ms <= 0.0 {
                        e.linear = e.target;
                    }
                }
                None => {
                    self.entries.insert(
                        key,
                        AnimEntry {
                            linear: r.target,
                            target: r.target,
                            duration_ms: r.duration_ms,
                            easing: r.easing,
                        },
                    );
                }
            }
        }
    }

    /// Advance in-flight entries by `dt_ms`, returning the panes that need a
    /// re-render (including the tick an entry lands on its target, so the
    /// final exact end state gets painted).
    pub fn advance(&mut self, dt_ms: f32) -> Vec<Entity> {
        let mut dirty = Vec::new();
        for ((pane, _, _), e) in self.entries.iter_mut() {
            if !e.in_flight() {
                continue;
            }
            let step = if e.duration_ms <= 0.0 {
                1.0
            } else {
                dt_ms / e.duration_ms
            };
            if e.linear < e.target {
                e.linear = (e.linear + step).min(e.target);
            } else {
                e.linear = (e.linear - step).max(e.target);
            }
            if !dirty.contains(pane) {
                dirty.push(*pane);
            }
        }
        dirty
    }

    /// Drop entries owned by despawned panes.
    pub fn retain_panes(&mut self, alive: impl Fn(Entity) -> bool) {
        self.entries.retain(|(pane, _, _), _| alive(*pane));
    }
}

/// Per-pane, read-only view of eased values, carried in `LayoutCtx` so render
/// arms can paint mid-flight states without resource access.
#[derive(Debug, Clone, Default)]
pub struct AnimSnapshot {
    values: HashMap<(String, String), f32>,
}

impl AnimSnapshot {
    pub fn value(&self, element_id: &str, state: &str) -> Option<f32> {
        self.values
            .get(&(element_id.to_string(), state.to_string()))
            .copied()
    }
}

/// Advance all in-flight animations and force a re-render on the panes that
/// own them. Runs before the render systems so the repaint lands this frame.
pub fn tick_widget_anims(
    time: Res<Time>,
    mut store: ResMut<WidgetAnim>,
    mut subprocess: Query<&mut crate::WidgetRender>,
    mut sw: Query<&mut crate::script_widget::ScriptWidget>,
    panes: Query<Entity, With<jim_pane::PaneTag>>,
) {
    store.retain_panes(|pane| panes.contains(pane));
    for pane in store.advance(time.delta_secs() * 1000.0) {
        if let Ok(mut r) = subprocess.get_mut(pane) {
            r.force_render = true;
        }
        if let Ok(mut w) = sw.get_mut(pane) {
            w.force_render = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: f32) -> AnimRequest {
        AnimRequest {
            element_id: "t".into(),
            state: "checked".into(),
            target,
            duration_ms: 100.0,
            easing: Easing::Linear,
        }
    }

    #[test]
    fn first_appearance_lands_at_target_without_animating() {
        let mut store = WidgetAnim::default();
        let pane = Entity::from_raw_u32(1).unwrap();
        store.apply_requests(pane, &[req(1.0)]);
        assert_eq!(store.eased(pane, "t", "checked"), Some(1.0));
        assert!(store.advance(16.0).is_empty());
    }

    #[test]
    fn target_change_animates_and_reports_dirty_pane() {
        let mut store = WidgetAnim::default();
        let pane = Entity::from_raw_u32(1).unwrap();
        store.apply_requests(pane, &[req(0.0)]);
        store.apply_requests(pane, &[req(1.0)]);
        // halfway through the 100ms transition
        assert_eq!(store.advance(50.0), vec![pane]);
        assert_eq!(store.eased(pane, "t", "checked"), Some(0.5));
        // overshoot clamps to target; the landing tick still reports dirty
        assert_eq!(store.advance(500.0), vec![pane]);
        assert_eq!(store.eased(pane, "t", "checked"), Some(1.0));
        // settled: nothing further to render
        assert!(store.advance(16.0).is_empty());
    }

    #[test]
    fn reversing_mid_flight_continues_from_current_value() {
        let mut store = WidgetAnim::default();
        let pane = Entity::from_raw_u32(1).unwrap();
        store.apply_requests(pane, &[req(0.0)]);
        store.apply_requests(pane, &[req(1.0)]);
        store.advance(60.0);
        store.apply_requests(pane, &[req(0.0)]); // toggled back mid-flight
        store.advance(30.0);
        assert_eq!(store.eased(pane, "t", "checked"), Some(0.3));
    }

    #[test]
    fn zero_duration_snaps() {
        let mut store = WidgetAnim::default();
        let pane = Entity::from_raw_u32(1).unwrap();
        store.apply_requests(pane, &[req(0.0)]);
        let mut snap = req(1.0);
        snap.duration_ms = 0.0;
        store.apply_requests(pane, &[snap]);
        assert_eq!(store.eased(pane, "t", "checked"), Some(1.0));
    }

    #[test]
    fn snapshot_scopes_to_pane_and_despawn_cleans_up() {
        let mut store = WidgetAnim::default();
        let a = Entity::from_raw_u32(1).unwrap();
        let b = Entity::from_raw_u32(2).unwrap();
        store.apply_requests(a, &[req(1.0)]);
        store.apply_requests(b, &[req(0.0)]);
        assert_eq!(store.snapshot_for(a).value("t", "checked"), Some(1.0));
        assert_eq!(store.snapshot_for(b).value("t", "checked"), Some(0.0));
        store.retain_panes(|p| p == a);
        assert!(store.eased(b, "t", "checked").is_none());
    }
}
