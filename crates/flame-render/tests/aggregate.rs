//! Does left-heavy (AGGREGATED) layout produce output for a jim-trace-shaped
//! profile? (recorder/jimtrace build slices via `add_complete_slice` with
//! depth + absolute start, one track per thread, no stack ids — mirror that.)

use flame_core::{ProfileBuilder, TrackKind};
use flame_render::renderer::build_left_heavy_layout;

fn build_profile() -> flame_core::Profile {
    let mut b = ProfileBuilder::new();
    let p = b.add_process(0, "jim");
    let tid = b.add_thread(Some(p), 1, "thread 1");
    let track = b.add_track(TrackKind::Thread(tid), "thread 1", None);
    let cat = b.intern_category("pane");
    let a = b.intern_string("a");
    let c = b.intern_string("b");
    // Two "frames" at absolute timestamps, each a depth-0 root with a depth-1 child.
    // frame 1
    b.add_complete_slice(track, 0, 1_000, 100, a, cat, None);
    b.add_complete_slice(track, 1, 1_010, 40, c, cat, None);
    // frame 2 (later in time)
    b.add_complete_slice(track, 0, 2_000, 120, a, cat, None);
    b.add_complete_slice(track, 1, 2_010, 30, c, cat, None);
    b.finish()
}

/// Aggregated bar duration MUST equal the sum of the individual slice
/// durations for that name (per depth). Mirrors a realistic recorder buffer:
/// several frames, each with two depth-0 roots, one of which nests a child.
#[test]
fn left_heavy_totals_equal_sums() {
    let mut b = ProfileBuilder::new();
    let p = b.add_process(0, "jim");
    let tid = b.add_thread(Some(p), 1, "t");
    let track = b.add_track(TrackKind::Thread(tid), "t", None);
    let cat = b.intern_category("x");
    let root = b.intern_string("root");
    let work = b.intern_string("work");
    let idle = b.intern_string("idle");

    // 3 frames at absolute, well-separated times.
    let mut expect_root = 0u64;
    let mut expect_work = 0u64;
    let mut expect_idle = 0u64;
    for (k, base) in [1_000u64, 5_000, 9_000].iter().enumerate() {
        let rdur = 100 + k as u64 * 10; // root spans this frame
        let wdur = 30 + k as u64 * 5; // child work inside root
        let idur = 40 + k as u64 * 7; // sibling idle root
        b.add_complete_slice(track, 0, *base, rdur, root, cat, None);
        b.add_complete_slice(track, 1, *base + 5, wdur, work, cat, None);
        b.add_complete_slice(track, 0, *base + rdur + 1, idur, idle, cat, None);
        expect_root += rdur;
        expect_work += wdur;
        expect_idle += idur;
    }
    let profile = b.finish();

    let (table, _range, _rows) = build_left_heavy_layout(&profile);
    // Sum aggregated durations per name.
    let mut got: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for i in 0..table.len() {
        let name = profile.strings.get(table.name[i]).to_string();
        *got.entry(name).or_default() += table.dur_ns[i];
    }
    eprintln!("aggregated per-name totals: {got:?}");
    eprintln!("expected root={expect_root} work={expect_work} idle={expect_idle}");
    assert_eq!(got.get("root").copied(), Some(expect_root), "root total != sum");
    assert_eq!(got.get("work").copied(), Some(expect_work), "work total != sum");
    assert_eq!(got.get("idle").copied(), Some(expect_idle), "idle total != sum");
}

#[test]
fn left_heavy_is_non_empty() {
    let profile = build_profile();
    assert_eq!(profile.slices.len(), 4, "sanity: 4 input slices");
    assert!(
        profile.slices.rows.contains_key(&(flame_core::TrackId(0), 0)),
        "depth-0 row index must exist after finish()"
    );

    let (table, range, rows) = build_left_heavy_layout(&profile);
    eprintln!(
        "aggregated: {} slices, range {:?}, rows {:?}",
        table.len(),
        range,
        rows
    );
    assert!(table.len() > 0, "AGGREGATED produced ZERO slices");
    assert!(range.1 > 0, "AGGREGATED time range is empty (max_total=0)");
    assert!(rows.iter().any(|&r| r > 0), "AGGREGATED has no rows");
}
