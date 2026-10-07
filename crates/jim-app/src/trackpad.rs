//! Scroll events with the gesture phase macOS reports for them.
//!
//! winit folds `NSEvent.phase` (fingers on the pad) and
//! `NSEvent.momentumPhase` (the inertial tail after they lift) into a single
//! `TouchPhase`, and it reports the START of the momentum stream as
//! `TouchPhase::Started` — the same value a finger touching down gets. So
//! from Bevy's `MouseWheel` alone a gesture cannot tell "fingers came down
//! again" from "momentum began". The sidebar's workspace swipe had to guess
//! from delta magnitudes instead, and momentum that happened to arrive two
//! events to a frame read as a new stroke: one flick, two workspaces.
//!
//! This installs an AppKit local event monitor that records each scroll
//! event's deltas together with both real phases, before winit sees it.
//! Consumers get the facts: a gesture starts only when fingers touch down,
//! and a momentum event can never start one.

use std::sync::{Arc, Mutex};

use bevy::prelude::*;

/// Where one scroll event sits in a gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollStage {
    /// Fingers touched the pad: a new gesture.
    Began,
    /// Fingers are moving on the pad (or resting on it).
    Moving,
    /// Fingers lifted (or the system cancelled the gesture).
    Ended,
    /// The inertial tail after the fingers lifted. Never a new gesture.
    Momentum,
    /// No phase information: a mouse wheel. Gestures on these can only be
    /// delimited by quiet time.
    Unphased,
}

/// One scroll event.
#[derive(Clone, Copy, Debug)]
pub struct ScrollSample {
    /// Logical pixels (`hasPreciseScrollingDeltas`), else wheel lines.
    pub dx: f32,
    pub dy: f32,
    /// True for pixel deltas (trackpad, Magic Mouse), false for lines.
    pub precise: bool,
    pub stage: ScrollStage,
}

/// Scroll events recorded since the last drain, in arrival order. Filled
/// by the monitor on the main thread during event dispatch; drained by
/// whichever system owns the gesture.
#[derive(Resource, Clone, Default)]
pub struct ScrollSamples(Arc<Mutex<Vec<ScrollSample>>>);

impl ScrollSamples {
    pub fn drain(&self) -> Vec<ScrollSample> {
        std::mem::take(&mut *self.0.lock().expect("scroll sample queue poisoned"))
    }
}

pub struct TrackpadPlugin;

impl Plugin for TrackpadPlugin {
    fn build(&self, app: &mut App) {
        let samples = ScrollSamples::default();
        app.insert_resource(samples.clone());
        #[cfg(target_os = "macos")]
        app.add_systems(Startup, move |_main: bevy::ecs::system::NonSendMarker| {
            install_scroll_monitor(samples.clone());
        });
    }
}

/// `NSEvent.phase`/`momentumPhase` → [`ScrollStage`]. Momentum wins: the
/// two are mutually exclusive in practice, and an event with any momentum
/// phase belongs to the inertial tail.
#[cfg(target_os = "macos")]
fn stage_of(phase: objc2_app_kit::NSEventPhase, momentum: objc2_app_kit::NSEventPhase) -> ScrollStage {
    use objc2_app_kit::NSEventPhase as P;
    if momentum != P::None {
        return ScrollStage::Momentum;
    }
    if phase.intersects(P::Began | P::MayBegin) {
        ScrollStage::Began
    } else if phase.intersects(P::Ended | P::Cancelled) {
        ScrollStage::Ended
    } else if phase.intersects(P::Changed | P::Stationary) {
        ScrollStage::Moving
    } else {
        ScrollStage::Unphased
    }
}

#[cfg(target_os = "macos")]
fn install_scroll_monitor(samples: ScrollSamples) {
    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask};
    use std::ptr::NonNull;

    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands the monitor a live event for the duration
        // of the call.
        let ev = unsafe { event.as_ref() };
        let sample = unsafe {
            ScrollSample {
                dx: ev.scrollingDeltaX() as f32,
                dy: ev.scrollingDeltaY() as f32,
                precise: ev.hasPreciseScrollingDeltas(),
                stage: stage_of(ev.phase(), ev.momentumPhase()),
            }
        };
        samples
            .0
            .lock()
            .expect("scroll sample queue poisoned")
            .push(sample);
        // Pass the event on untouched: this only observes.
        event.as_ptr()
    });
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::ScrollWheel, &handler)
    };
    // The monitor is removed when its token is released; it lives for the
    // whole process, so keep the token forever.
    match monitor {
        Some(token) => std::mem::forget(token),
        None => panic!("[trackpad] AppKit refused the scroll-wheel event monitor"),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use objc2_app_kit::NSEventPhase as P;

    /// The whole point: the start of a momentum stream is momentum, not a
    /// finger touching down. winit reports both as `TouchPhase::Started`.
    #[test]
    fn momentum_start_is_not_a_new_gesture() {
        assert_eq!(stage_of(P::None, P::Began), ScrollStage::Momentum);
        assert_eq!(stage_of(P::Began, P::None), ScrollStage::Began);
    }

    #[test]
    fn finger_phases_map_through() {
        assert_eq!(stage_of(P::Changed, P::None), ScrollStage::Moving);
        assert_eq!(stage_of(P::Ended, P::None), ScrollStage::Ended);
        assert_eq!(stage_of(P::None, P::None), ScrollStage::Unphased);
        assert_eq!(stage_of(P::None, P::Changed), ScrollStage::Momentum);
    }
}
