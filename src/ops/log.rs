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

/// The step number of an op in logs: from 1 (the wire `step-index` is from 0).
pub(crate) fn step_no(op: &contract::Op) -> i64 {
    op.step_index + 1
}

/// One line per op: `op done` (INFO), `op refused` (WARN, the runner said
/// no by policy) or `op failed` (ERROR, something broke). Only ids, kinds,
/// reasons and timings: `error` is the fixed `stage` word of a `runner-error`.
/// `step` counts from 1, as in the cloud logs.
pub(super) fn handled(
    op: &contract::Op,
    now: SystemTime,
    start: SystemTime,
    result: &Option<ResultFrame>,
    err: &Option<ErrorFrame>,
) {
    let us = micros(now.duration_since(start).unwrap_or_default());
    let step = step_no(op);
    let num = |k: &str| err.as_ref().and_then(|e| e.details.get(k)?.as_u64());
    let (busy, cap) = (num("busy"), num("limit"));
    let stage = err
        .as_ref()
        .and_then(|e| e.details.get("stage")?.as_str().map(str::to_string));
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
            reason = %e.reason, busy = busy, cap = cap, error = stage.as_deref(), elapsed_ms = us, "op failed"
        ),
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::tests::test_op;
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut v) = self.0.lock() {
                v.extend_from_slice(b);
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    #[test]
    fn log_lines_count_steps_from_one() {
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let mut op = test_op("o1", "r1", "sql.query", "db", 1000, HashMap::new(), vec![]);
        op.step_index = 0;
        let now = SystemTime::now();
        tracing::subscriber::with_default(sub, || {
            handled(
                &op,
                now,
                now,
                &None,
                &Some(ErrorFrame::new("o1", "arg-not-allowed", serde_json::json!({}))),
            )
        });
        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("step=1"), "{logs}");
    }
}
