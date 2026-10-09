//! Running one op on the bounded pool and queueing its reply; the refusal
//! when the pool is full.

use super::reply::{error_frame, reply_frame};
use super::{ErrorFrame, Inner, StepError};
use crate::contract;
use futures::FutureExt;
use serde_json::json;
use std::sync::Arc;
use tracing::Instrument;

impl Inner {
    pub(super) fn reject(&self, frame_id: &str, op: &contract::Op) {
        let op_id = op.op_id.as_str();
        let busy = self.capacity as u64;
        let frame = ErrorFrame::new(
            op_id,
            "runner-at-capacity",
            json!({"limit-name": "max-inflight", "limit": busy, "busy": busy, "waited-ms": 0}),
        );
        self.count_reply(Some(&frame));
        tracing::error!(target: "runner.ops", run_id = %op.run_id, step = op.step_index, op_id = %op_id, kind = %op.kind, resource = %op.resource, status = "error", reason = "runner-at-capacity", busy = busy, cap = busy, trace_id = op.valid_trace_id().unwrap_or("-"), "op failed");
        match error_frame(frame_id, frame) {
            Ok(frame) => self.push_back(frame),
            Err(e) => {
                tracing::error!(target: "runner.exchange", frame = "capacity", error = %e, "frame failed")
            }
        }
    }

    /// Executes one op and queues its reply. Releasing the pool permit and
    /// firing an extra exchange both happen after the reply is queued (not
    /// before), so a result never waits behind the main loop's long poll.
    ///
    /// The handler future is run under `catch_unwind`, so a
    /// panicking handler still replies instead of silently dropping the op.
    /// On panic the reply is a
    /// `runner-error` result with `where` `op-handler`.
    pub(super) async fn run_op(
        self: Arc<Self>,
        frame_id: String,
        op: contract::Op,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        let op_id = op.op_id.clone();
        // The op's trace lives in a span: every line the op causes, and a
        // panic inside it, carries it. logfmt keeps every span with a
        // `trace_id` field whatever `ZRIZ_LOG` says; it must be error level
        // (the level hint logfmt gives for these spans).
        let span = match op.valid_trace_id() {
            Some(t) => tracing::error_span!(target: "runner.ops", "op", trace_id = %t),
            None => tracing::error_span!(target: "runner.ops", "op"),
        };
        let (result, err) =
            match std::panic::AssertUnwindSafe(self.cfg.handler.handle(op).instrument(span))
                .catch_unwind()
                .await
            {
                Ok(pair) => pair,
                Err(_) => (
                    None,
                    Some(ErrorFrame::new(
                        &op_id,
                        "runner-error",
                        json!({"where": "op-handler"}),
                    )),
                ),
            };
        self.count_reply(err.as_ref());
        match reply_frame(&frame_id, result, err) {
            Ok(frame) => self.push_back(frame),
            Err(e) => {
                tracing::error!(target: "runner.exchange", frame = "reply", error = %e, "frame failed")
            }
        }
        drop(permit);
        self.trigger_exchange();
    }

    /// Fires one exchange, unconditionally, on its own task, so a completed
    /// op's result leaves on a new request instead of waiting behind
    /// whichever requests are already parked in the cloud's hold. There is
    /// no cap here: at most `handler.max_inflight()` ops run at once, each
    /// firing one exchange on completion, so at most `max_inflight() + 1`
    /// requests are ever open (the main loop's plus one per finishing op).
    ///
    /// A failure here is not swallowed: it feeds the same shared backoff
    /// the main loop uses, so a cloud outage discovered on this path slows
    /// every request path down, not just the main loop's. It is never
    /// treated as authorization failure here — the main loop's own next
    /// scheduled exchange will observe the same 401 and stop the loop.
    pub(super) fn trigger_exchange(self: &Arc<Self>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            if inner.exchange().await == Err(StepError::Failed) {
                inner.apply_backoff(true).await;
            }
        });
    }
}
