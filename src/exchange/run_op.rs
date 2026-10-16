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
    pub(super) fn reject(self: &Arc<Self>, frame_id: &str, op: &contract::Op) {
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
            Ok(frame) => self.push_back(super::reply::fit_frame(frame, frame_id, op_id)),
            Err(e) => {
                tracing::error!(target: "runner.exchange", frame = "capacity", error = %e, "frame failed")
            }
        }
        // The main poll never carries results: the sender sends this one.
        self.trigger_exchange();
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
            Ok(frame) => self.push_back(super::reply::fit_frame(frame, &frame_id, &op_id)),
            Err(e) => {
                tracing::error!(target: "runner.exchange", frame = "reply", error = %e, "frame failed")
            }
        }
        drop(permit);
        self.trigger_exchange();
    }

    /// Sends the queued results, with at most one result request in flight.
    /// If one is in flight, returns at once: its sender loop takes the
    /// results that finish meanwhile, all together, in its next request.
    /// Idle, the request starts at once. A failure feeds the shared backoff
    /// and the loop tries again while results wait (at-least-once delivery).
    /// 401, 403 and a stop end the loop; the main loop sees them itself.
    /// With nothing queued (a health request) one empty request goes out.
    pub(super) fn trigger_exchange(self: &Arc<Self>) {
        if !self
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_start(true)
        {
            return;
        }
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            // A panic in the loop frees the gate (Drop); a normal end has
            // freed it already under the lock and disarms the guard.
            let mut guard = GateGuard {
                inner: Arc::clone(&inner),
                armed: true,
            };
            loop {
                let r = inner.exchange_with(true).await;
                let failed = r == Err(StepError::Failed);
                if r.is_err() && !failed {
                    guard.armed = false;
                    inner.gate_stop();
                    return;
                }
                if failed && !inner.apply_backoff(true).await {
                    guard.armed = false;
                    inner.gate_stop();
                    return;
                }
                if !inner.gate_next() {
                    guard.armed = false;
                    return;
                }
            }
        });
    }
}

/// Frees the sender gate if the sender task ends without doing it (panic).
struct GateGuard {
    inner: Arc<Inner>,
    armed: bool,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        if self.armed {
            self.inner.gate_stop();
        }
    }
}
