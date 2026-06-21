//! Render-thread phase timing for jim's own trace.
//!
//! The main-thread `prof`/`trace` instrumentation can only see the *main*
//! world's systems. The render sub-app runs on its own thread, so all of its
//! work — extract, prepare (buffer/bind-group creation), queue, the render
//! passes, present — was a single anonymous "untracked gap" in the trace, even
//! though that's exactly where the frame-time spikes were going (the main
//! thread blocks on the pipelined render sync waiting for it).
//!
//! This plugin brackets the [`Render`] schedule's phase sets with span
//! open/close systems that feed the SAME `jim_pane::trace` ring as the rest of
//! the instrumentation. With `JIMTRACE` armed, a slow frame's dump now carries
//! `render` (whole schedule), `render.prepare`, `render.queue`, and
//! `render.passes` spans on the render thread — so "what in rendering is slow"
//! is finally answerable from the trace instead of needing `sample`.
//!
//! Cost when tracing is off: `trace::begin()` is a single relaxed atomic load
//! (same as every other instrumented site), so these systems are ~free.

use bevy::prelude::*;
use bevy::render::{Render, RenderApp, RenderSystems};
use jim_pane::trace;

/// Holds the in-flight `Pending` for each bracketed phase between its open and
/// close systems (which are separate systems in the same schedule run).
#[derive(Resource, Default)]
struct RenderTraceState {
    whole: Option<trace::Pending>,
    prepare: Option<trace::Pending>,
    queue: Option<trace::Pending>,
    passes: Option<trace::Pending>,
}

pub struct RenderTracePlugin;

impl Plugin for RenderTracePlugin {
    fn build(&self, _app: &mut App) {}

    // `finish` runs after every plugin's `build`, so the RenderApp sub-app and
    // its `RenderSystems` set ordering are fully configured before we hook in.
    fn finish(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app.init_resource::<RenderTraceState>().add_systems(
            Render,
            (
                // Whole render schedule: very start → very end.
                open_whole.before(RenderSystems::ExtractCommands),
                close_whole.after(RenderSystems::Cleanup),
                // Prepare zone (buffer/uniform + bind-group creation) — the
                // `create_buffer`/`write_buffer`/`prepare_*_bind_groups` cost.
                open_prepare.before(RenderSystems::Prepare),
                close_prepare.after(RenderSystems::PrepareBindGroups),
                // Queue zone (queue_sprites / phase building).
                open_queue.before(RenderSystems::Queue),
                close_queue.after(RenderSystems::Queue),
                // Render passes (the actual GPU command encoding + present).
                open_passes.before(RenderSystems::Render),
                close_passes.after(RenderSystems::Render),
            ),
        );
    }
}

fn open_whole(mut s: ResMut<RenderTraceState>) {
    s.whole = trace::begin();
}
fn close_whole(mut s: ResMut<RenderTraceState>) {
    trace::end(s.whole.take(), "render", "render", 0);
}
fn open_prepare(mut s: ResMut<RenderTraceState>) {
    s.prepare = trace::begin();
}
fn close_prepare(mut s: ResMut<RenderTraceState>) {
    trace::end(s.prepare.take(), "render.prepare", "render", 0);
}
fn open_queue(mut s: ResMut<RenderTraceState>) {
    s.queue = trace::begin();
}
fn close_queue(mut s: ResMut<RenderTraceState>) {
    trace::end(s.queue.take(), "render.queue", "render", 0);
}
fn open_passes(mut s: ResMut<RenderTraceState>) {
    s.passes = trace::begin();
}
fn close_passes(mut s: ResMut<RenderTraceState>) {
    trace::end(s.passes.take(), "render.passes", "render", 0);
}
