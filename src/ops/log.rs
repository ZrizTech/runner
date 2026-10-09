//! The one line each op logs when it ends.

use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame};
use crate::logfmt::micros;
use std::time::SystemTime;

/// Reasons that mean the runner refused by policy (WARN `op refused`);
/// any other error reason is a failure (ERROR `op failed`).
const REFUSED: [&str; 10] = [
    "unknown-arg",
    "unknown-kind",
    "unknown-resource",
    "host-not-allowed",
    "read-only",
    "placeholder-in-disallowed-slot",
    "evidence-disabled",
    "evidence-expired",
    "command-not-allowed",
    "arg-not-allowed",
];

/// One line per op: `op done` (INFO), `op refused` (WARN, the runner said
/// no by policy) or `op failed` (ERROR, something broke). Only ids, kinds,
/// reasons and timings: `error` is the fixed `where` word of a `runner-error`.
pub(super) fn handled(
    op: &contract::Op,
    now: SystemTime,
    start: SystemTime,
    result: &Option<ResultFrame>,
    err: &Option<ErrorFrame>,
) {
    let us = micros(now.duration_since(start).unwrap_or_default());
    let step = op.step_index;
    let num = |k: &str| err.as_ref().and_then(|e| e.details.get(k)?.as_u64());
    let (busy, cap) = (num("busy"), num("limit"));
    let place = err
        .as_ref()
        .and_then(|e| e.details.get("where")?.as_str().map(str::to_string));
    match (result, err) {
        (Some(r), _) => tracing::info!(
            target: "runner.ops", run_id = %op.run_id, step = step, op_id = %op.op_id,
            kind = %op.kind, resource = %op.resource, status = %r.status,
            elapsed_ms = us, "op done"
        ),
        (None, Some(e)) if REFUSED.contains(&e.reason.as_str()) => tracing::warn!(
            target: "runner.ops", run_id = %op.run_id, step = step, op_id = %op.op_id,
            kind = %op.kind, resource = %op.resource, status = "error",
            reason = %e.reason, busy = busy, cap = cap, elapsed_ms = us, "op refused"
        ),
        (None, Some(e)) => tracing::error!(
            target: "runner.ops", run_id = %op.run_id, step = step, op_id = %op.op_id,
            kind = %op.kind, resource = %op.resource, status = "error",
            reason = %e.reason, busy = busy, cap = cap, error = place.as_deref(), elapsed_ms = us, "op failed"
        ),
        (None, None) => {}
    }
}
