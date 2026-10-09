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
pub(super) fn from_worker(op: &contract::Op, resp: &Value) -> ErrorFrame {
    let id = op.op_id.as_str();
    let num = |k: &str| resp.get(k).and_then(Value::as_u64);
    let mut d = Map::new();
    let mut put = |k: &str, v: Option<u64>| {
        if let Some(n) = v {
            d.insert(k.into(), json!(n));
        }
    };
    let word = resp.get("reason").and_then(Value::as_str).unwrap_or("");
    match word {
        "at-capacity" => {
            put("limit", num("max-contexts"));
            put("busy", num("busy"));
            put("waited-ms", num("waited-ms"));
            d.insert("resource".into(), json!(op.resource));
            d.insert("limit-name".into(), json!("max-contexts"));
            new_error(id, "runner-at-capacity", Value::Object(d))
        }
        "context-lost" => {
            put("idle-ms", num("idle-ms"));
            d.insert("resource".into(), json!(op.resource));
            d.insert("why".into(), json!("idle"));
            new_error(id, "context-lost", Value::Object(d))
        }
        "handle-busy" | "no-handle" => {
            let h = op.args.get("handle").and_then(Value::as_str);
            new_error(id, word, json!({"handle": h.unwrap_or("")}))
        }
        "too-many-handles" => {
            put("limit", num("limit"));
            new_error(id, "too-many-handles", Value::Object(d))
        }
        "spawn-failed" => new_error(id, "spawn-failed", json!({})),
        "internal" => new_error(id, "worker-error", json!({})),
        w if MISMATCH.contains(&w) => new_error(id, "worker-mismatch", json!({"worker-reason": w})),
        _ => runner_error(id, "worker-word"),
    }
}

/// The frame for a context lost after a restart of the worker.
pub(super) fn restarted(op: &contract::Op) -> ErrorFrame {
    new_error(
        &op.op_id,
        "context-lost",
        json!({"resource": op.resource, "why": "worker-restarted"}),
    )
}
