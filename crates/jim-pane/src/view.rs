//! Views — the plural form of "where the world is drawn".
//!
//! The canvas has always had exactly one mapping from world to screen:
//! [`crate::PaneViewport`], a single `(origin, pan, zoom)` that every
//! hit-test, drag, dock and camera-sync system reads. That is why content
//! can only appear in one place — not because the content is singular
//! (Bevy happily draws one entity through many cameras; `cube.rs` already
//! does) but because the *mapping* is.
//!
//! A [`View`] is one such mapping plus the screen rect it draws into.
//! [`ROOT_VIEW`] is the window itself, so today's behaviour is "the
//! degenerate case with one view". Child views are sub-rects — a slide
//! showing a project, or the whole application inside a slide.
//!
//! ## The tree
//!
//! Views nest: a view lives inside a pane, which may itself be drawn by
//! another view. Composition is rect containment down the parent chain, so
//! there is no separate coordinate stack to keep consistent.
//!
//! Recursion is wanted (a slideshow showing itself) and is bounded by
//! [`MAX_VIEW_DEPTH`] — a view deeper than that is simply never created.
//! A bounded tree cannot cycle, so there is no cycle detection to get
//! wrong.
//!
//! ## Input
//!
//! [`Views::resolve`] is the single funnel: window point → the *deepest*
//! view containing it, plus that point in the view's canvas space.
//! Everything downstream (hit-test, focus, drag, resize) already works in
//! canvas space, so it keeps working unchanged once it asks the funnel
//! instead of the global viewport.

use bevy::math::{Rect, Vec2};
use bevy::prelude::{Entity, Resource};

use crate::PaneViewport;

/// Identifies one view. [`ROOT_VIEW`] is always the window.
pub type ViewId = u32;

/// The window itself — the view that has always existed.
pub const ROOT_VIEW: ViewId = 0;

/// How deep view nesting may go before children stop being created.
///
/// A whole-application view contains the slide that hosts it, so nesting
/// is unbounded in principle. Two levels is enough to *read* as recursive
/// without the cost growing without limit.
pub const MAX_VIEW_DEPTH: u8 = 2;

/// One place the world is drawn.
#[derive(Debug, Clone)]
pub struct View {
    pub id: ViewId,
    /// `None` only for [`ROOT_VIEW`].
    pub parent: Option<ViewId>,
    /// Screen rect in window-logical pixels.
    pub rect: Rect,
    /// Canvas ↔ screen mapping for this view. For the root this is the
    /// value that used to live in the global [`PaneViewport`].
    pub transform: PaneViewport,
    /// Which project's panes this view shows. `None` = whatever the host
    /// considers active (the root's meaning).
    pub project: Option<u64>,
    /// The pane this view is embedded in. `None` for [`ROOT_VIEW`] (the
    /// window is nobody's child).
    ///
    /// Input needs this: the host pane covers its own view's rect, and it
    /// is usually the TOPMOST thing there (a presenting deck sits at
    /// `z = 500` and fills the window). Without knowing the host, every
    /// click aimed at a pane *inside* the view is swallowed by the pane the
    /// view is drawn in — which is how a live project view ends up looking
    /// interactive and being inert.
    pub host: Option<Entity>,
    /// 0 for the root; `parent.depth + 1` otherwise.
    pub depth: u8,
}

impl View {
    /// Window point → this view's canvas space.
    pub fn to_canvas(&self, window_pt: Vec2) -> Vec2 {
        self.transform.window_to_canvas(window_pt)
    }
    /// This view's canvas space → window point.
    pub fn to_window(&self, canvas_pt: Vec2) -> Vec2 {
        self.transform.canvas_to_window(canvas_pt)
    }
}

/// Every live view. Always contains [`ROOT_VIEW`].
#[derive(Resource, Debug, Clone)]
pub struct Views {
    views: Vec<View>,
    next_id: ViewId,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            views: vec![View {
                id: ROOT_VIEW,
                parent: None,
                host: None,
                // Filled in each frame by the host from the real window.
                rect: Rect::from_corners(Vec2::ZERO, Vec2::new(f32::MAX, f32::MAX)),
                transform: PaneViewport::default(),
                project: None,
                depth: 0,
            }],
            next_id: 1,
        }
    }
}

impl Views {
    pub fn root(&self) -> &View {
        &self.views[0]
    }

    /// Keep the root in step with the window and the active project's
    /// pan/zoom. Called once per frame by the host, alongside the write to
    /// the legacy [`PaneViewport`] resource.
    pub fn set_root(&mut self, rect: Rect, transform: PaneViewport, project: Option<u64>) {
        let root = &mut self.views[0];
        root.rect = rect;
        root.transform = transform;
        root.project = project;
    }

    pub fn get(&self, id: ViewId) -> Option<&View> {
        self.views.iter().find(|v| v.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &View> {
        self.views.iter()
    }

    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        false // the root always exists
    }

    /// Add a child view. Returns `None` if the parent is unknown or the
    /// depth cap would be exceeded — the caller draws nothing rather than
    /// recursing forever.
    pub fn insert(
        &mut self,
        parent: ViewId,
        rect: Rect,
        transform: PaneViewport,
        project: Option<u64>,
        host: Option<Entity>,
    ) -> Option<ViewId> {
        let depth = self.get(parent)?.depth.checked_add(1)?;
        if depth > MAX_VIEW_DEPTH {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.views.push(View {
            id,
            parent: Some(parent),
            rect,
            transform,
            project,
            host,
            depth,
        });
        Some(id)
    }

    /// Update a child view's geometry in place.
    pub fn update(&mut self, id: ViewId, rect: Rect, transform: PaneViewport) -> bool {
        if id == ROOT_VIEW {
            return false;
        }
        match self.views.iter_mut().find(|v| v.id == id) {
            Some(v) => {
                v.rect = rect;
                v.transform = transform;
                true
            }
            None => false,
        }
    }

    /// Remove a view and everything nested inside it. The root is never
    /// removable — losing it would leave the window unmapped.
    pub fn remove(&mut self, id: ViewId) {
        if id == ROOT_VIEW {
            return;
        }
        let mut doomed = vec![id];
        let mut i = 0;
        while i < doomed.len() {
            let parent = doomed[i];
            for v in &self.views {
                if v.parent == Some(parent) {
                    doomed.push(v.id);
                }
            }
            i += 1;
        }
        self.views.retain(|v| !doomed.contains(&v.id));
    }

    /// The **deepest** view containing `window_pt`, and that point in its
    /// canvas space.
    ///
    /// Deepest-first is what makes a slide view over the canvas win for
    /// points inside it, while everything outside still lands on the root.
    /// This is the single funnel every interaction should go through; the
    /// old `viewport.window_to_canvas(pt)` is exactly `resolve(pt)` in a
    /// world with one view.
    pub fn resolve(&self, window_pt: Vec2) -> (ViewId, Vec2) {
        let mut best: Option<&View> = None;
        for v in &self.views {
            if !v.rect.contains(window_pt) {
                continue;
            }
            if best.is_none_or(|b| v.depth > b.depth) {
                best = Some(v);
            }
        }
        match best {
            Some(v) => (v.id, v.to_canvas(window_pt)),
            // Outside every view's rect (e.g. off-window): fall back to the
            // root's mapping so callers never have to handle `None`.
            None => (ROOT_VIEW, self.root().to_canvas(window_pt)),
        }
    }

    /// Is `pane` the host of `view` or of any view it nests inside?
    ///
    /// Hit-testing asks this to skip the panes a point is looking
    /// *through*. A click inside a slide's live project view is aimed at
    /// the project, not at the slide — and not at whatever hosts the slide
    /// either, so the whole ancestor chain is excluded, not just the
    /// nearest host. The root view has no host, so this is always false in
    /// a one-view world and the ordinary hit-test is unchanged.
    pub fn is_host_of(&self, view: ViewId, pane: Entity) -> bool {
        let mut cur = self.get(view);
        while let Some(v) = cur {
            if v.host == Some(pane) {
                return true;
            }
            cur = v.parent.and_then(|p| self.get(p));
        }
        false
    }

    /// Project framed by the deepest view under a window point.
    pub fn project_at(&self, window_pt: Vec2) -> Option<u64> {
        let (id, _) = self.resolve(window_pt);
        self.get(id).and_then(|view| view.project)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(zoom: f32, pan: Vec2) -> PaneViewport {
        PaneViewport {
            origin: Vec2::ZERO,
            pan,
            zoom,
        }
    }

    fn views_with_child() -> (Views, ViewId) {
        let mut v = Views::default();
        v.set_root(
            Rect::from_corners(Vec2::ZERO, Vec2::new(1000.0, 800.0)),
            vp(1.0, Vec2::ZERO),
            Some(1),
        );
        let child = v
            .insert(
                ROOT_VIEW,
                Rect::from_corners(Vec2::new(200.0, 200.0), Vec2::new(600.0, 600.0)),
                vp(0.5, Vec2::ZERO),
                Some(2),
                None,
            )
            .expect("child fits under the depth cap");
        (v, child)
    }

    /// With one view, `resolve` must behave exactly like the old global
    /// `window_to_canvas`. Stage 1's whole contract is "nothing changes".
    #[test]
    fn root_only_matches_the_global_viewport() {
        let mut v = Views::default();
        let t = vp(2.0, Vec2::new(10.0, 10.0));
        v.set_root(
            Rect::from_corners(Vec2::ZERO, Vec2::new(800.0, 600.0)),
            t,
            None,
        );
        let p = Vec2::new(100.0, 50.0);
        let (id, canvas) = v.resolve(p);
        assert_eq!(id, ROOT_VIEW);
        assert_eq!(canvas, t.window_to_canvas(p));
    }

    /// A point inside a child resolves to the CHILD's mapping — this is
    /// what makes a slide view interactive rather than a picture.
    #[test]
    fn deepest_view_wins_inside_its_rect() {
        let (v, child) = views_with_child();
        let inside = Vec2::new(300.0, 300.0);
        let (id, canvas) = v.resolve(inside);
        assert_eq!(id, child);
        assert_eq!(canvas, v.get(child).unwrap().to_canvas(inside));
        // …and differs from what the root would have said, or the funnel
        // isn't actually doing anything.
        assert_ne!(canvas, v.root().to_canvas(inside));
    }

    /// Outside the child, the root still owns the point.
    #[test]
    fn points_outside_a_child_fall_back_to_the_root() {
        let (v, _) = views_with_child();
        let outside = Vec2::new(50.0, 50.0);
        assert_eq!(v.resolve(outside).0, ROOT_VIEW);
    }

    /// Recursion is bounded by construction: past the cap, no view exists,
    /// so the region draws empty instead of nesting forever.
    #[test]
    fn nesting_stops_at_the_depth_cap() {
        let mut v = Views::default();
        let rect = Rect::from_corners(Vec2::ZERO, Vec2::new(100.0, 100.0));
        let mut parent = ROOT_VIEW;
        for depth in 1..=MAX_VIEW_DEPTH {
            parent = v
                .insert(parent, rect, PaneViewport::default(), None, None)
                .unwrap_or_else(|| panic!("depth {depth} must be allowed"));
            assert_eq!(v.get(parent).unwrap().depth, depth);
        }
        assert!(
            v.insert(parent, rect, PaneViewport::default(), None, None)
                .is_none(),
            "one past the cap must be refused"
        );
    }

    /// The pane a view is drawn INSIDE is looked through, not at. Without
    /// this, a click aimed at a project embedded in a slide is taken by the
    /// slide — which, while presenting, covers the whole window — and the
    /// embedded project is a picture instead of the real thing.
    #[test]
    fn a_view_looks_through_its_host_chain() {
        let deck = Entity::from_raw_u32(11).expect("valid entity");
        let outer_host = Entity::from_raw_u32(12).expect("valid entity");
        let bystander = Entity::from_raw_u32(13).expect("valid entity");
        let rect = Rect::from_corners(Vec2::ZERO, Vec2::new(100.0, 100.0));
        let mut v = Views::default();
        let outer = v
            .insert(
                ROOT_VIEW,
                rect,
                PaneViewport::default(),
                None,
                Some(outer_host),
            )
            .unwrap();
        let inner = v
            .insert(outer, rect, PaneViewport::default(), None, Some(deck))
            .unwrap();

        assert!(v.is_host_of(inner, deck), "the view's own host");
        assert!(v.is_host_of(inner, outer_host), "and every host above it");
        assert!(!v.is_host_of(inner, bystander));
        // The root has no host, so a one-view world hit-tests as it always did.
        assert!(!v.is_host_of(ROOT_VIEW, deck));
    }

    /// Removing a view takes its whole subtree — a leaked grandchild would
    /// keep stealing input inside a region that is no longer drawn.
    #[test]
    fn removing_a_view_removes_its_descendants() {
        let mut v = Views::default();
        let rect = Rect::from_corners(Vec2::ZERO, Vec2::new(100.0, 100.0));
        let child = v
            .insert(ROOT_VIEW, rect, PaneViewport::default(), None, None)
            .unwrap();
        let grandchild = v
            .insert(child, rect, PaneViewport::default(), None, None)
            .unwrap();
        assert_eq!(v.len(), 3);
        v.remove(child);
        assert_eq!(v.len(), 1);
        assert!(v.get(child).is_none() && v.get(grandchild).is_none());
    }

    /// The root must survive everything: without it there is no mapping
    /// for the window at all.
    #[test]
    fn the_root_cannot_be_removed() {
        let mut v = Views::default();
        v.remove(ROOT_VIEW);
        assert_eq!(v.len(), 1);
        assert_eq!(v.root().id, ROOT_VIEW);
    }
}
