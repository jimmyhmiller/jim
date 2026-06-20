//! Reader for jim-editor's own frame-trace dumps (`~/.jim/traces/*.json`),
//! produced by jim-pane's span ring + jim-app's fps trace writer. Profile Jim
//! with Jim.
//!
//! Each file is ONE frame: a flat list of spans, each carrying a `thread`,
//! `depth`, `start_ms` (offset from frame start), `dur_ms`, `category`, and a
//! human `label` (resolved pane title) / `name`. We map each distinct thread
//! to its own track and each span to a complete slice at its recorded depth —
//! exactly the icicle the renderer expects.

use std::collections::HashMap;

use flame_core::{CategoryId, LoadError, LoadResult, ProfileBuilder, TrackId, TrackKind, TraceSource};
use serde::Deserialize;

/// Minimal view of the dump — we only need the spans (+ their per-span fields)
/// to reconstruct the flame graph. Frame/stage metadata is ignored for now.
#[derive(Deserialize)]
struct JimTrace {
    #[serde(default)]
    spans: Vec<JimSpan>,
}

#[derive(Deserialize)]
struct JimSpan {
    #[serde(default)]
    thread: u64,
    #[serde(default)]
    depth: u16,
    #[serde(default)]
    category: String,
    #[serde(default)]
    name: String,
    /// Resolved human label (e.g. a pane title); empty for non-pane spans.
    #[serde(default)]
    label: String,
    /// Start offset from frame start, milliseconds.
    #[serde(default)]
    start_ms: f64,
    #[serde(default)]
    dur_ms: f64,
}

pub struct JimTraceSource;

impl TraceSource for JimTraceSource {
    fn name(&self) -> &'static str {
        "Jim frame trace"
    }

    fn detect(&self, input: &[u8], filename: Option<&str>) -> bool {
        if let Some(f) = filename {
            if f.contains("frame-") && f.ends_with(".json") {
                return true;
            }
        }
        // Content sniff: our dumps carry these distinctive keys near the top.
        let head = &input[..input.len().min(4096)];
        let s = String::from_utf8_lossy(head);
        s.contains("\"ring_capacity\"") && s.contains("\"spans\"")
    }

    fn load(&self, input: &[u8], b: &mut ProfileBuilder) -> LoadResult<()> {
        let doc: JimTrace =
            serde_json::from_slice(input).map_err(|e| LoadError::Parse(e.to_string()))?;

        let process = b.add_process(0, "jim");
        let mut tracks: HashMap<u64, TrackId> = HashMap::new();
        let mut cats: HashMap<String, CategoryId> = HashMap::new();

        for s in &doc.spans {
            let track = match tracks.get(&s.thread) {
                Some(t) => *t,
                None => {
                    let label = format!("thread {}", s.thread);
                    let tid = b.add_thread(Some(process), s.thread as i64, &label);
                    let t = b.add_track(TrackKind::Thread(tid), &label, None);
                    tracks.insert(s.thread, t);
                    t
                }
            };
            let category = match cats.get(&s.category) {
                Some(c) => *c,
                None => {
                    let c = b.intern_category(&s.category);
                    cats.insert(s.category.clone(), c);
                    c
                }
            };
            // Prefer the resolved pane title; fall back to the static span name.
            let display = if s.label.is_empty() { &s.name } else { &s.label };
            let name_id = b.intern_string(display);
            let start_ns = (s.start_ms * 1.0e6).max(0.0) as u64;
            let dur_ns = (s.dur_ms * 1.0e6).max(0.0) as u64;
            b.add_complete_slice(track, s.depth, start_ns, dur_ns, name_id, category, None);
        }
        Ok(())
    }
}
