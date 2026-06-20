//! Render-path check: does `rebuild_instances` emit instances in AGGREGATED
//! (left-heavy) mode for a jim-trace-shaped profile? Uses a headless wgpu
//! device (skips if none available, e.g. headless CI).

use std::sync::Arc;

use flame_core::{ProfileBuilder, TrackKind};
use flame_render::{LayoutMode, Renderer};

fn build_profile() -> flame_core::Profile {
    let mut b = ProfileBuilder::new();
    let p = b.add_process(0, "jim");
    let tid = b.add_thread(Some(p), 1, "thread 1");
    let track = b.add_track(TrackKind::Thread(tid), "thread 1", None);
    let cat = b.intern_category("pane");
    let a = b.intern_string("a");
    let c = b.intern_string("b");
    b.add_complete_slice(track, 0, 1_000, 100, a, cat, None);
    b.add_complete_slice(track, 1, 1_010, 40, c, cat, None);
    b.add_complete_slice(track, 0, 2_000, 120, a, cat, None);
    b.add_complete_slice(track, 1, 2_010, 30, c, cat, None);
    b.finish()
}

fn headless() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("test"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue))
}

#[test]
fn aggregate_render_emits_instances() {
    let Some((device, queue)) = headless() else {
        eprintln!("no wgpu adapter; skipping");
        return;
    };
    let mut r = Renderer::new(&device, &queue, wgpu::TextureFormat::Rgba8UnormSrgb, (1200, 700));
    r.set_profile(Arc::new(build_profile()));
    r.rebuild_instances();
    let time_ordered = r.instances.len();
    eprintln!("time-ordered instances: {time_ordered}");
    assert!(time_ordered > 0, "time-ordered emitted nothing");

    r.set_layout_mode(LayoutMode::LeftHeavy);
    r.rebuild_instances();
    let aggregated = r.instances.len();
    eprintln!(
        "AGGREGATED instances: {aggregated}, layout={:?}, viewport start={} npp={} size={:?}",
        r.layout_mode, r.viewport.start_ns, r.viewport.ns_per_pixel, r.viewport.size_px
    );
    assert!(aggregated > 0, "AGGREGATED emitted ZERO instances (everything disappears)");

    // The viewport must actually frame the aggregated slices (laid out 0-based),
    // not be clamped away to the profile's absolute time range.
    let (agg_start, agg_end) = r.current_time_range();
    let view_end = r.viewport.start_ns + r.viewport.ns_per_pixel * r.viewport.size_px.0 as f64;
    assert!(
        r.viewport.start_ns <= agg_end as f64 && view_end >= agg_start as f64,
        "viewport [{}, {}] does not overlap aggregated range [{}, {}] — slices stranded off-screen",
        r.viewport.start_ns, view_end, agg_start, agg_end
    );

    // Hovering the aggregated "a" bar must report the SUMMED duration (220 ns),
    // not whatever original slice sits at that index in profile.slices.
    let prof = r.profile.clone().expect("profile");
    let mut a_inst = None;
    {
        let active = r.current_slices(&prof);
        for (id, inst) in r.instances.iter().enumerate() {
            if inst.flags & 1 != 0 {
                continue;
            }
            let si = r.slice_indices[id];
            if si == u32::MAX {
                continue;
            }
            if prof.strings.get(active.name[si as usize]) == "a" {
                a_inst = Some(id as u32);
                break;
            }
        }
    }
    assert!(a_inst.is_some(), "no aggregated 'a' bar instance found");
    r.set_hover(a_inst);
    eprintln!("hover status: {}", r.status_text);
    assert!(
        r.status_text.contains("220 ns"),
        "hover on aggregated 'a' should show summed duration 220 ns, got: {}",
        r.status_text
    );
}
