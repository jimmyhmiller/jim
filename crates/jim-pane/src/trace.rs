//! Rich per-span circular trace buffer.
//!
//! Where [`crate::prof`] *aggregates* a frame into per-pane / per-subsystem
//! totals (it throws away order, nesting, and the individual span instances
//! the moment it folds them into a `HashMap`), `trace` keeps **every
//! individual span** as a flat record in a fixed-size ring. That's the data
//! you need to dissect one pathological frame: what ran, in what order, how
//! deeply nested, and for how long.
//!
//! The ring overwrites oldest-first, so steady-state cost is bounded and
//! there is no growth — it is always recording (when enabled) but never
//! kept. When a frame runs long, the consumer copies *that frame's* records
//! out of the ring ([`collect_frame`]) into an owned snapshot it can hold
//! onto / write to disk; the ring keeps churning underneath.
//!
//! Spans are fed by the same `prof::pane_span`/`sys_span` guards, so every
//! already-instrumented site contributes for free whenever tracing is on.
//! New sites that don't want the aggregate can call [`span`] / [`pane`]
//! directly.
//!
//! Cost when disabled: a single relaxed atomic load in [`begin`], same deal
//! as `prof`. Nothing else runs and the clock is never read.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Number of span records held in the ring. ~32k spans is far more than any
/// sane frame produces; at ~80 bytes/record that's ~2.5 MB resident, only
/// allocated once tracing is first enabled.
pub const CAPACITY: usize = 1 << 15;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Monotonic frame counter, bumped by [`begin_frame`]. Every span recorded
/// between two `begin_frame` calls carries the same id, so a dump can pull
/// exactly one frame's worth back out.
static FRAME: AtomicU64 = AtomicU64::new(0);
/// Process-epoch-relative nanos at which the current frame began. Lets a
/// dump express each span's `start_ns` as an offset from frame start.
static FRAME_START_NS: AtomicU64 = AtomicU64::new(0);
/// How many spans have been recorded in the current frame. Compared against
/// [`CAPACITY`] to detect whether the ring wrapped within a single frame
/// (i.e. the dump is truncated).
static FRAME_SPANS: AtomicU64 = AtomicU64::new(0);

/// Shared monotonic clock origin. One `Instant`, captured the first time
/// anything needs a timestamp, so all `start_ns` are comparable.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

#[inline]
fn now_ns() -> u64 {
    epoch().elapsed().as_nanos() as u64
}

thread_local! {
    /// Current span nesting depth on this thread. Captured at span start so
    /// a dump can reconstruct the call tree.
    static DEPTH: Cell<u16> = const { Cell::new(0) };
    /// Stable small per-thread id, assigned on first use. The OS thread id
    /// is clunky to render; this is just "thread 1, 2, 3…" in record order.
    static TID: Cell<u64> = const { Cell::new(0) };
}
static NEXT_TID: AtomicU64 = AtomicU64::new(1);

fn thread_id() -> u64 {
    TID.with(|t| {
        let v = t.get();
        if v == 0 {
            let id = NEXT_TID.fetch_add(1, Ordering::Relaxed);
            t.set(id);
            id
        } else {
            v
        }
    })
}

/// One recorded span. `Copy` and pointer-sized fields only, so the ring is a
/// flat array and recording is a memcpy under a brief lock.
#[derive(Clone, Copy)]
pub struct SpanRecord {
    /// Frame id this span belongs to (see [`begin_frame`]). 0 = empty slot.
    pub frame: u64,
    /// Stable per-thread id; main-thread work and worker-thread work are
    /// distinguishable in the dump.
    pub thread: u64,
    /// Nesting depth on its thread at span start (0 = top level).
    pub depth: u16,
    /// Span name. For pane spans this is the kind ("terminal"/"editor"/
    /// "widget"); for subsystem spans it's the subsystem name.
    pub name: &'static str,
    /// Category tag: "pane" | "sys" | "sys.nested" | a caller-chosen group.
    pub category: &'static str,
    /// Pane `Entity` bits when attributable to one pane, else 0. The
    /// consumer resolves this to a human label via the Bevy `World`.
    pub entity_bits: u64,
    /// Start time, nanos since the process epoch.
    pub start_ns: u64,
    /// Wall duration in nanos.
    pub dur_ns: u64,
}

impl SpanRecord {
    const EMPTY: SpanRecord = SpanRecord {
        frame: 0,
        thread: 0,
        depth: 0,
        name: "",
        category: "",
        entity_bits: 0,
        start_ns: 0,
        dur_ns: 0,
    };
}

struct Ring {
    buf: Vec<SpanRecord>,
    /// Next write index.
    cursor: usize,
}

static RING: Mutex<Ring> = Mutex::new(Ring {
    buf: Vec::new(),
    cursor: 0,
});

/// Is tracing on? Instrumented sites check this before reading the clock.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turn the recorder on/off. Turning on allocates the ring (once) and clears
/// it so a fresh capture session starts clean.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if on {
        if let Ok(mut r) = RING.lock() {
            if r.buf.is_empty() {
                r.buf.resize(CAPACITY, SpanRecord::EMPTY);
            } else {
                for s in r.buf.iter_mut() {
                    *s = SpanRecord::EMPTY;
                }
            }
            r.cursor = 0;
        }
    }
}

/// Open a new frame: bump the frame id, stamp its start, reset the per-frame
/// span counter. Call once at the very top of the frame (before any spans).
/// Returns the new frame id.
pub fn begin_frame() -> u64 {
    let f = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
    FRAME_START_NS.store(now_ns(), Ordering::Relaxed);
    FRAME_SPANS.store(0, Ordering::Relaxed);
    f
}

/// The frame id spans are currently being tagged with.
#[inline]
pub fn current_frame() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Spans recorded so far in the current frame. If this is >= [`CAPACITY`]
/// the ring wrapped within the frame and a dump will be missing the earliest
/// spans (see [`FrameTrace::truncated`]).
pub fn current_frame_span_count() -> u64 {
    FRAME_SPANS.load(Ordering::Relaxed)
}

fn record(rec: SpanRecord) {
    FRAME_SPANS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut r) = RING.lock() {
        if r.buf.is_empty() {
            r.buf.resize(CAPACITY, SpanRecord::EMPTY);
        }
        let i = r.cursor;
        r.buf[i] = rec;
        r.cursor = if i + 1 == CAPACITY { 0 } else { i + 1 };
    }
}

/// The captured start-state of an in-flight span. Returned by [`begin`] and
/// handed back to [`end`]; `None` means tracing was off at start, so the
/// matching `end` is a no-op (and the depth counter is never touched, so it
/// stays balanced).
pub struct Pending {
    frame: u64,
    thread: u64,
    depth: u16,
    start_ns: u64,
}

/// Start timing a span. Cheap no-op (one atomic load) when tracing is off.
/// Pair with [`end`]. Most callers should use the RAII [`span`]/[`pane`]
/// guards or go through `prof::pane_span`/`sys_span` instead.
#[inline]
pub fn begin() -> Option<Pending> {
    if !enabled() {
        return None;
    }
    let depth = DEPTH.with(|d| {
        let v = d.get();
        d.set(v.saturating_add(1));
        v
    });
    Some(Pending {
        frame: current_frame(),
        thread: thread_id(),
        depth,
        start_ns: now_ns(),
    })
}

/// Close a span opened with [`begin`] and push its record into the ring.
pub fn end(pending: Option<Pending>, name: &'static str, category: &'static str, entity_bits: u64) {
    let Some(p) = pending else { return };
    DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    let end_ns = now_ns();
    record(SpanRecord {
        frame: p.frame,
        thread: p.thread,
        depth: p.depth,
        name,
        category,
        entity_bits,
        start_ns: p.start_ns,
        dur_ns: end_ns.saturating_sub(p.start_ns),
    });
}

/// RAII trace span. Records on drop. Created as a no-op when tracing is off.
#[must_use = "the span times until it is dropped; bind it to a local"]
pub struct TraceSpan {
    pending: Option<Pending>,
    name: &'static str,
    category: &'static str,
    entity_bits: u64,
}

impl Drop for TraceSpan {
    fn drop(&mut self) {
        end(self.pending.take(), self.name, self.category, self.entity_bits);
    }
}

/// Time a named span in a caller-chosen `category`. For instrumentation that
/// wants the rich trace but not the `prof` aggregate.
#[inline]
pub fn span(name: &'static str, category: &'static str) -> TraceSpan {
    TraceSpan {
        pending: begin(),
        name,
        category,
        entity_bits: 0,
    }
}

/// Like [`span`] but attributed to a pane `Entity` (category "pane").
#[inline]
pub fn pane(name: &'static str, entity_bits: u64) -> TraceSpan {
    TraceSpan {
        pending: begin(),
        name,
        category: "pane",
        entity_bits,
    }
}

/// An owned snapshot of one frame's spans, copied out of the ring.
pub struct FrameTrace {
    pub frame: u64,
    /// Process-epoch nanos at frame start; subtract from each span's
    /// `start_ns` for a frame-relative offset.
    pub frame_start_ns: u64,
    /// Spans of this frame, sorted by start time.
    pub spans: Vec<SpanRecord>,
    /// True if more spans were recorded this frame than the ring holds, so
    /// the earliest are gone and `spans` is the tail only.
    pub truncated: bool,
}

/// Copy every record tagged with `frame` out of the ring into an owned,
/// start-sorted snapshot. Cheap and non-destructive: the ring keeps
/// recording. Call right after the frame, before its records age out.
pub fn collect_frame(frame: u64) -> FrameTrace {
    let mut spans: Vec<SpanRecord> = Vec::new();
    if let Ok(r) = RING.lock() {
        for s in r.buf.iter() {
            if s.frame == frame {
                spans.push(*s);
            }
        }
    }
    spans.sort_by_key(|s| s.start_ns);
    let recorded = current_frame_span_count();
    FrameTrace {
        frame,
        frame_start_ns: FRAME_START_NS.load(Ordering::Relaxed),
        spans,
        truncated: recorded as usize > CAPACITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test fn: the recorder is process-global, so exercising it serially
    // avoids races with a parallel test runner.
    #[test]
    fn records_nested_spans_per_frame() {
        // Disabled: no clock read, no record, depth untouched.
        set_enabled(false);
        assert!(begin().is_none());

        set_enabled(true);
        let f = begin_frame();
        assert_eq!(current_frame(), f);

        {
            let _outer = span("outer", "sys");
            let _inner = pane("inner", 42);
            // depths captured at start: outer=0, inner=1.
        }
        let extra = span("sibling", "sys");
        drop(extra);

        let ft = collect_frame(f);
        assert_eq!(ft.frame, f);
        assert_eq!(ft.spans.len(), 3, "all three spans of the frame");
        assert!(!ft.truncated);

        // Nested-by-depth: the pane span opened inside `outer` is depth 1.
        let inner = ft.spans.iter().find(|s| s.name == "inner").unwrap();
        assert_eq!(inner.depth, 1);
        assert_eq!(inner.category, "pane");
        assert_eq!(inner.entity_bits, 42);
        let outer = ft.spans.iter().find(|s| s.name == "outer").unwrap();
        assert_eq!(outer.depth, 0);
        let sibling = ft.spans.iter().find(|s| s.name == "sibling").unwrap();
        assert_eq!(sibling.depth, 0, "depth balanced after both closed");

        // A different frame id sees none of these.
        let g = begin_frame();
        assert!(collect_frame(g).spans.is_empty());

        set_enabled(false);
    }
}
