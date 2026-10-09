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

/// A fixed word for an error message, so `op failed` says what broke
/// without ever logging the message itself (some carry names or values).
/// An unknown message is `other`: fail closed.
fn error_word(message: &str) -> &'static str {
    const EXACT: [(&str, &str); 12] = [
        ("op timed out", "timeout"),
        ("op handler panicked", "panic"),
        ("invalid request", "invalid-request"),
        ("invalid request body", "invalid-request"),
        ("response encoding failed", "encode"),
        ("response too large", "too-large"),
        ("http request failed", "http"),
        ("query failed", "query"),
        ("invalid capture", "capture"),
        ("evidence excerpt failed", "evidence"),
        ("no capacity for this op", "capacity"),
        ("worker is at capacity", "capacity"),
    ];
    const PREFIX: [(&str, &str); 7] = [
        ("browser contexts busy ", "capacity"),
        ("context lost: ", "context-lost"),
        ("capture ", "capture"),
        ("environment variable ", "env-not-set"),
        ("secret ", "secret-not-allowed"),
        ("worker ", "worker"),
        ("cli refused: ", "cli-refused"),
    ];
    EXACT
        .iter()
        .find(|(m, _)| *m == message)
        .or_else(|| PREFIX.iter().find(|(p, _)| message.starts_with(p)))
        .map_or("other", |(_, w)| w)
}

/// The `busy` and `cap` numbers of a capacity refusal, when the message has
/// them (the worker's `browser contexts busy B of C ...`).
fn capacity_numbers(message: &str) -> (Option<u64>, Option<u64>) {
    let mut words = message
        .strip_prefix("browser contexts busy ")
        .unwrap_or("")
        .split(' ');
    let busy = words.next().and_then(|w| w.parse().ok());
    let cap = words.nth(1).and_then(|w| w.parse().ok());
    (busy, cap)
}

/// One line per op: `op done` (INFO), `op refused` (WARN, the runner said
/// no by policy) or `op failed` (ERROR, something broke). Only ids, kinds,
/// reasons and timings: `error` is a fixed word, never the message.
pub(super) fn handled(
    op: &contract::Op,
    now: SystemTime,
    start: SystemTime,
    result: &Option<ResultFrame>,
    err: &Option<ErrorFrame>,
) {
    let us = micros(now.duration_since(start).unwrap_or_default());
    let step = op.step_index;
    let (busy, cap) = err
        .as_ref()
        .map_or((None, None), |e| capacity_numbers(&e.message));
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
            reason = %e.reason, busy = busy, cap = cap, error = error_word(&e.message), elapsed_ms = us, "op failed"
        ),
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{capacity_numbers, error_word};

    #[test]
    fn capacity_numbers_from_the_message() {
        let m = "browser contexts busy 8 of 8 (max-contexts of resource web), waited 5000 ms";
        assert_eq!(capacity_numbers(m), (Some(8), Some(8)));
        assert_eq!(capacity_numbers("worker is at capacity"), (None, None));
    }

    #[test]
    fn error_words() {
        for (msg, want) in [
            ("op timed out", "timeout"),
            ("invalid request body", "invalid-request"),
            ("capture token not found in the response", "capture"),
            (
                "environment variable DB_PW is not set on the runner",
                "env-not-set",
            ),
            ("cli refused: spawn-failed", "cli-refused"),
            ("worker refused the op", "worker"),
            (
                "browser contexts busy 8 of 8 (max-contexts of resource web), waited 5000 ms",
                "capacity",
            ),
            (
                "context lost: closed to make room (max-contexts 3)",
                "context-lost",
            ),
            ("something new with sekrit", "other"),
        ] {
            assert_eq!(error_word(msg), want, "{msg}");
        }
    }
}
