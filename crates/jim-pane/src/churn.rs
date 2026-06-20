//! Per-frame entity-churn counter.
//!
//! [`prof`](crate::prof) and [`trace`](crate::trace) both time *systems*, but
//! the most expensive thing a frame can do is often invisible to them: when a
//! system queues a big batch of `commands.spawn(...)` / `entity.despawn()`,
//! the actual entity creation and teardown happens later, at a Bevy **sync
//! point** *between* systems — inside the schedule stage but inside no profiler
//! span. A funct widget that rebuilds its whole flow tree from scratch
//! (despawn every child, respawn hundreds) shows ~0ms in its render span while
//! the Update stage balloons to tens of ms applying the deferred churn. That's
//! the classic "Update is 86ms but every span sums to nothing" mystery.
//!
//! This module makes that cost legible. Two process-global counters tick once
//! per entity spawned / despawned, fed by `On<Add, ChildOf>` / `On<Remove,
//! ChildOf>` observers the host installs (every parented entity carries
//! exactly one `ChildOf`, so the count is *entities*, not components). The
//! profiler resets them at frame start and reads them at frame end, so a slow
//! frame's dump carries "spawned 812 / despawned 812" right next to its stage
//! times — pointing straight at the rebuild-from-scratch widget.
//!
//! Cost when neither profiler is on: the observer still fires (Bevy dispatches
//! it), but [`note_spawn`]/[`note_despawn`] bail on a single relaxed atomic
//! load before touching the counters — same "free when disabled" deal as the
//! other two layers.

use std::sync::atomic::{AtomicU64, Ordering};

static SPAWNS: AtomicU64 = AtomicU64::new(0);
static DESPAWNS: AtomicU64 = AtomicU64::new(0);

/// Churn tracks against whichever profiler is live; it has no toggle of its
/// own. Recording is wasted work unless something will read it this frame.
#[inline]
fn recording() -> bool {
    crate::prof::enabled() || crate::trace::enabled()
}

/// Count one entity spawned this frame. Cheap no-op when no profiler is on.
#[inline]
pub fn note_spawn() {
    if recording() {
        SPAWNS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Count one entity despawned this frame. Cheap no-op when no profiler is on.
#[inline]
pub fn note_despawn() {
    if recording() {
        DESPAWNS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Zero the counters. Call once at the very top of the frame (alongside
/// [`trace::begin_frame`](crate::trace::begin_frame)) so the read at frame end
/// is exactly this frame's churn.
pub fn reset() {
    SPAWNS.store(0, Ordering::Relaxed);
    DESPAWNS.store(0, Ordering::Relaxed);
}

/// `(spawned, despawned)` since the last [`reset`]. Read at end of frame.
pub fn snapshot() -> (u64, u64) {
    (
        SPAWNS.load(Ordering::Relaxed),
        DESPAWNS.load(Ordering::Relaxed),
    )
}
