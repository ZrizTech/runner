//! The map from a word of the worker to the error frame (spec 7.1). The
//! worker text is never passed on; its numbers travel as `details`.

use super::{ErrorFrame, new_error, runner_error};
use crate::contract;
use serde_json::{Map, Value, json};

/// The worker words that the runner reports as `worker-mismatch`.
const MISMATCH: [&str; 6] = [
    "bad-json",
    "bad-version",
    "unknown-kind",
    "bad-request",
    "request-too-large",
    "not-implemented",
];

/// The frame for a refusal `resp` of the worker to `op`.
pub(super) fn from_worker(op: &contract::Op, resp: &Value, idle_ms: u64) -> ErrorFrame {
    let id = op.op_id.as_str();
    let num = |k: &str| resp.get(k).and_then(Value::as_u64);
    let mut d = Map::new();
    let word = resp.get("reason").and_then(Value::as_str).unwrap_or("");
    match word {
        "at-capacity" => {
            put(&mut d, "limit", num("max-contexts"));
            put(&mut d, "busy", num("busy"));
            put(&mut d, "waited-ms", num("waited-ms"));
            d.insert("resource".into(), json!(op.resource));
            d.insert("limit-name".into(), json!("max-contexts"));
            new_error(id, "runner-at-capacity", Value::Object(d))
        }
        "context-lost" => {
            d.insert("resource".into(), json!(op.resource));
            if resp.get("why").and_then(Value::as_str) == Some("run-closed") {
                d.insert("why".into(), json!("run-closed"));
            } else {
                put(&mut d, "idle-ms", num("idle-ms"));
                d.insert("why".into(), json!("idle"));
            }
            new_error(id, "context-lost", Value::Object(d))
        }
        "no-handle" if resp.get("why").and_then(Value::as_str) == Some("idle") => new_error(
            id,
            "handle-lost",
            json!({"handle": handle_of(op), "why": "idle", "idle-ms": idle_ms}),
        ),
        "handle-busy" | "no-handle" => new_error(id, word, json!({"handle": handle_of(op)})),
        "too-many-handles" => {
            put(&mut d, "limit", num("limit"));
            new_error(id, "too-many-handles", Value::Object(d))
        }
        "spawn-failed" => new_error(id, "spawn-failed", json!({})),
        "internal" => new_error(id, "worker-error", json!({})),
        w if MISMATCH.contains(&w) => new_error(id, "worker-mismatch", json!({"worker-reason": w})),
        _ => runner_error(id, "worker-word"),
    }
}

/// Puts a number into `d` when the worker gave one.
fn put(d: &mut Map<String, Value>, k: &str, v: Option<u64>) {
    if let Some(n) = v {
        d.insert(k.into(), json!(n));
    }
}

fn handle_of(op: &contract::Op) -> &str {
    op.args.get("handle").and_then(Value::as_str).unwrap_or("-")
}

/// The frame for a handle the runner knows and the worker lost in a restart.
pub(super) fn handle_restarted(op: &contract::Op) -> ErrorFrame {
    new_error(
        &op.op_id,
        "handle-lost",
        json!({"handle": handle_of(op), "why": "worker-restarted"}),
    )
}

/// The frame for a handle the run never started (or that is finished).
pub(super) fn no_handle(op: &contract::Op) -> ErrorFrame {
    new_error(&op.op_id, "no-handle", json!({"handle": handle_of(op)}))
}

/// The frame for a context lost after a restart of the worker.
pub(super) fn restarted(op: &contract::Op) -> ErrorFrame {
    new_error(
        &op.op_id,
        "context-lost",
        json!({"resource": op.resource, "why": "worker-restarted"}),
    )
}
