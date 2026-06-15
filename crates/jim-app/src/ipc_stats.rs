//! Bridges the IPC listener's [`IpcMetrics`](crate::ipc::IpcMetrics) (updated
//! from the acceptor + worker threads) onto the widget message bus, so a
//! `ipc_monitor.ft` pane can visualize the thread pool and request activity
//! live. Published retained per-project on [`TOPIC`], throttled to changes
//! plus a slow heartbeat.

use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use jim_widget::{PendingMsg, WidgetMsgBus};

use crate::ipc::IpcMetricsHandle;
use crate::projects::Projects;

/// Bus topic the monitor widget subscribes to.
pub const TOPIC: &str = "ipc/stats";

/// Resource wrapper around the shared IPC metrics handle (inserted at startup
/// iff the listener came up).
#[derive(Resource)]
pub struct IpcMetricsRes(pub IpcMetricsHandle);

/// Throttle state for [`publish_ipc_stats`].
#[derive(Default)]
pub struct PubState {
    last_sig: u64,
    last_emit: f64,
}

/// Snapshot the metrics and, when they've changed (or every few seconds),
/// publish them to every project's bus so a monitor pane anywhere updates.
pub fn publish_ipc_stats(
    metrics: Option<Res<IpcMetricsRes>>,
    mut bus: ResMut<WidgetMsgBus>,
    projects: Res<Projects>,
    time: Res<Time>,
    mut state: Local<PubState>,
) {
    let Some(metrics) = metrics else { return };
    let m = &metrics.0;

    let accepted = m.accepted.load(Ordering::Relaxed);
    let completed = m.completed.load(Ordering::Relaxed);
    let parse_errors = m.parse_errors.load(Ordering::Relaxed);
    let timeouts = m.timeouts.load(Ordering::Relaxed);
    let read_errors = m.read_errors.load(Ordering::Relaxed);
    let busy = m.busy.load(Ordering::Relaxed);
    let queued = m.queued.load(Ordering::Relaxed);

    let recent: Vec<serde_json::Value> = m
        .recent
        .lock()
        .map(|q| {
            q.iter()
                .map(|e| {
                    serde_json::json!({
                        "ts": e.ts_ms,
                        "action": e.action,
                        "outcome": e.outcome,
                        "dur_ms": e.dur_ms,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let newest = recent
        .first()
        .and_then(|v| v.get("ts"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let sig = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            accepted, completed, parse_errors, timeouts, read_errors, busy, queued, newest,
        )
            .hash(&mut h);
        h.finish()
    };

    let now = time.elapsed_secs_f64();
    // Emit on change, plus a slow heartbeat so gauges stay fresh even idle.
    if sig == state.last_sig && now - state.last_emit < 3.0 {
        return;
    }
    state.last_sig = sig;
    state.last_emit = now;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let payload = serde_json::json!({
        "kind": "ipc_stats",
        "workers": m.workers,
        "busy": busy,
        "queued": queued,
        "accepted": accepted,
        "completed": completed,
        "parse_errors": parse_errors,
        "timeouts": timeouts,
        "read_errors": read_errors,
        "recent": recent,
        "now": now_ms,
    });

    // The bus is project-scoped (nothing crosses projects), so a monitor pane
    // could live anywhere — publish to all of them. Retained so a pane spawned
    // later gets the latest on init.
    for p in &projects.list {
        bus.push_external(PendingMsg {
            project: Some(p.id),
            topic: TOPIC.to_string(),
            payload: payload.clone(),
            sender: "ipc".to_string(),
            retain: true,
        });
    }
}
