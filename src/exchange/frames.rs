//! Building the exchange request, dispatching the ops in a response onto
//! the bounded pool, and building the result/error frames a finished op
//! replies with.

use super::health::{self, Snapshot};
use super::reply::{error_frame, reply_frame};
use super::{ErrorFrame, Inner, StepError};
use crate::contract;
use futures::FutureExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::Instrument;

/// One WARN `poll failed`; `error` is a short fixed word, never library text.
fn poll_failed(status: Option<u16>, error: &str, t0: Instant) {
    let us = crate::logfmt::micros(t0.elapsed());
    tracing::warn!(target: "runner.exchange", http_status = status, error = error, elapsed_ms = us, "poll failed");
}

/// A poll that took this long or longer is worth an INFO line.
const SLOW_POLL: Duration = Duration::from_millis(35_000);

/// The most refusals of one frame before the runner drops it.
const MAX_REFUSALS: u32 = 3;

/// One `poll done` line: INFO when frames came or the poll was slow, else
/// DEBUG. `status` is `None` when no HTTP answer came.
fn poll_done(status: Option<u16>, sent: usize, received: usize, t0: Instant) {
    let elapsed = t0.elapsed();
    let us = crate::logfmt::micros(elapsed);
    if received > 0 || elapsed >= SLOW_POLL {
        tracing::info!(target: "runner.exchange", http_status = status, sent = sent, received = received, elapsed_ms = us, "poll done");
    } else {
        tracing::debug!(target: "runner.exchange", http_status = status, sent = sent, received = received, elapsed_ms = us, "poll done");
    }
}

/// A refusal is a 4xx other than 401 (token) and 429 (rate limit).
fn is_refusal(status: u16) -> bool {
    (400..500).contains(&status) && status != 401 && status != 429
}

impl Inner {
    fn drain_queue(&self) -> Vec<contract::Frame> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *q)
    }

    /// Puts `frames` back at the head of the queue, ahead of anything
    /// completed since — used to requeue a request's frames after it
    /// fails, so nothing is lost and order is preserved.
    fn push_front(&self, frames: Vec<contract::Frame>) {
        if frames.is_empty() {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut merged = frames;
        merged.append(&mut q);
        *q = merged;
    }

    fn push_back(&self, frame: contract::Frame) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.push(frame);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            inflight: (self.capacity - self.semaphore.available_permits()) as i64,
            max_inflight: self.declared_max_inflight,
            worker: self.cfg.handler.worker_health(),
            refused: self.refused.swap(0, Ordering::SeqCst),
            errors: self.errors.swap(0, Ordering::SeqCst),
        }
    }

    fn build_request(
        &self,
        frames: Vec<contract::Frame>,
        snap: &Snapshot,
    ) -> contract::ExchangeRequest {
        contract::ExchangeRequest {
            v: 1,
            runner: contract::Runner {
                runner_id: self.cfg.runner_id.clone(),
                version: super::VERSION.to_string(),
                ops: self.cfg.handler.op_kinds(),
                resources: self.cfg.handler.resources(&self.cfg.resources),
                max_inflight: self.declared_max_inflight,
                env: self.cfg.env.clone(),
            },
            inflight: snap.inflight,
            health: health::health(&self.boot_id, snap),
            frames,
        }
    }

    /// The request got no HTTP answer: its counts go back, to be sent in
    /// the next request. (A request that got an answer keeps them out: they
    /// start over at 0 when the request is built.)
    fn counts_unsent(&self, snap: &Snapshot) {
        self.refused.fetch_add(snap.refused, Ordering::SeqCst);
        self.errors.fetch_add(snap.errors, Ordering::SeqCst);
    }

    /// Counts the end of one op for the health of the next request.
    fn count_reply(&self, reason: Option<&str>) {
        match reason {
            Some("runner-at-capacity") => self.refused.fetch_add(1, Ordering::SeqCst),
            Some("runner-error" | "worker-error") => self.errors.fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
    }

    fn current_token(&self) -> String {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn post(
        &self,
        req: &contract::ExchangeRequest,
    ) -> std::result::Result<reqwest::Response, reqwest::Error> {
        self.http_client
            .post(&self.endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.current_token()),
            )
            .header(
                reqwest::header::USER_AGENT,
                self.cfg.user_agent.as_deref().unwrap_or(super::USER_AGENT),
            )
            .json(req)
            .send()
            .await
    }

    /// Drains the completed-frame queue into one request, posts it, and
    /// dispatches whatever ops come back. Safe to call from several tasks
    /// at once: the main loop calls it in a tight cycle, and each op
    /// completion calls it once more so results never wait behind a long
    /// poll.
    pub(super) async fn exchange(self: &Arc<Self>) -> std::result::Result<(), StepError> {
        self.cfg.handler.refresh().await;
        let sent = self.drain_queue();
        let snap = self.snapshot();
        let req = self.build_request(sent.clone(), &snap);

        let t0 = Instant::now();
        let post_result = tokio::select! {
            r = self.post(&req) => Some(r),
            () = self.cancel.cancelled() => None,
        };

        let resp = match post_result {
            None => {
                self.counts_unsent(&snap);
                self.push_front(sent);
                return Err(StepError::Cancelled);
            }
            Some(Ok(r)) => r,
            Some(Err(e)) => {
                self.counts_unsent(&snap);
                self.push_front(sent);
                let why = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else {
                    "request"
                };
                poll_failed(None, why, t0);
                return Err(StepError::Failed);
            }
        };

        self.handle_response(resp, sent, t0).await
    }

    async fn handle_response(
        self: &Arc<Self>,
        resp: reqwest::Response,
        sent: Vec<contract::Frame>,
        t0: Instant,
    ) -> std::result::Result<(), StepError> {
        match resp.status() {
            reqwest::StatusCode::OK => self.handle_ok(resp, sent, t0).await,
            reqwest::StatusCode::NO_CONTENT => {
                self.forget_refusals(&sent);
                poll_done(Some(204), sent.len(), 0, t0);
                self.connected();
                Ok(())
            }
            reqwest::StatusCode::UNAUTHORIZED => {
                let n = sent.len();
                self.push_front(sent);
                poll_done(Some(401), n, 0, t0);
                tracing::error!(target: "runner.exchange", http_status = 401u16, "poll refused");
                Err(StepError::Unauthorized)
            }
            other => {
                let status = other.as_u16();
                let kept = if is_refusal(status) {
                    self.drop_refused(status, sent)
                } else {
                    sent
                };
                self.push_front(kept);
                poll_failed(Some(status), "bad-status", t0);
                Err(StepError::Failed)
            }
        }
    }

    /// The cloud took `sent`: it is not refused any more.
    fn forget_refusals(&self, sent: &[contract::Frame]) {
        let mut m = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        for f in sent {
            m.remove(&f.id);
        }
    }

    /// Counts one refusal for each of `sent`. A frame refused for the third
    /// time is dropped, with one ERROR line (`count` is the number of tries) for all that drop; the rest is
    /// returned to be queued again.
    fn drop_refused(&self, status: u16, sent: Vec<contract::Frame>) -> Vec<contract::Frame> {
        let mut m = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        let mut kept = Vec::new();
        let mut dropped = false;
        for f in sent {
            let n = m.entry(f.id.clone()).or_insert(0);
            *n += 1;
            if *n >= MAX_REFUSALS {
                m.remove(&f.id);
                dropped = true;
            } else {
                kept.push(f);
            }
        }
        if dropped {
            tracing::error!(target: "runner.exchange", http_status = status, count = MAX_REFUSALS, reason = "refused", "frames dropped");
        }
        kept
    }

    async fn handle_ok(
        self: &Arc<Self>,
        resp: reqwest::Response,
        sent: Vec<contract::Frame>,
        t0: Instant,
    ) -> std::result::Result<(), StepError> {
        let body = match resp.json::<contract::ExchangeResponse>().await {
            Ok(b) => b,
            Err(_) => {
                self.push_front(sent);
                poll_failed(Some(200), "bad-body", t0);
                return Err(StepError::Failed);
            }
        };
        self.forget_refusals(&sent);
        poll_done(Some(200), sent.len(), body.frames.len(), t0);
        self.connected();
        for run in body.ended_runs {
            self.run_ended(run);
        }
        self.dispatch(body.frames);
        Ok(())
    }

    /// One id of a run-end notice: one DEBUG line with the trace of the
    /// notice, then the handler frees the resources off the loop.
    fn run_ended(self: &Arc<Self>, run: contract::EndedRun) {
        let trace = if run.trace_id.len() == 36 {
            run.trace_id.as_str()
        } else {
            "-"
        };
        tracing::debug!(target: "runner.exchange", run_id = %run.run_id, trace_id = trace, "run closed");
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner
                .cfg
                .handler
                .run_ended(&run.run_id, &run.trace_id)
                .await;
        });
    }

    /// Runs every op frame in the response, one task per op, on the bounded
    /// pool. Anything that isn't a decodable op is logged and skipped: no
    /// reply is ever sent for it.
    fn dispatch(self: &Arc<Self>, frames: Vec<contract::Frame>) {
        for f in frames {
            self.dispatch_one(f);
        }
    }

    fn dispatch_one(self: &Arc<Self>, f: contract::Frame) {
        if f.t != "op" {
            tracing::warn!(target: "runner.exchange", frame = %f.t, "frame skipped");
            return;
        }

        // An op with no kind is treated as undecodable even though Op
        // itself decodes fine with an empty (but present) kind field.
        let op = match serde_json::from_value::<contract::Op>(f.d.clone()) {
            Ok(op) if !op.kind.is_empty() => op,
            _ => {
                tracing::warn!(target: "runner.exchange", frame = %f.t, "frame skipped");
                return;
            }
        };

        match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(permit) => {
                let inner = Arc::clone(self);
                tokio::spawn(async move { inner.run_op(f.id, op, permit).await });
            }
            Err(_) => self.reject(&f.id, &op),
        }
    }

    fn reject(&self, frame_id: &str, op: &contract::Op) {
        let op_id = op.op_id.as_str();
        let busy = self.capacity as u64;
        self.count_reply(Some("runner-at-capacity"));
        tracing::error!(target: "runner.ops", run_id = %op.run_id, step = op.step_index, op_id = %op_id, kind = %op.kind, resource = %op.resource, status = "error", reason = "runner-at-capacity", busy = busy, cap = busy, error = "capacity", trace_id = op.valid_trace_id().unwrap_or("-"), "op failed");
        match error_frame(
            frame_id,
            contract::Error {
                op_id: op_id.to_string(),
                reason: "runner-at-capacity".to_string(),
                message: "no capacity for this op".to_string(),
            },
        ) {
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
    /// `runner-error` result with message `"op handler panicked"`.
    async fn run_op(
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
                    Some(ErrorFrame {
                        op_id,
                        reason: "runner-error".to_string(),
                        message: "op handler panicked".to_string(),
                    }),
                ),
            };
        self.count_reply(err.as_ref().map(|e| e.reason.as_str()));
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
    fn trigger_exchange(self: &Arc<Self>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            if inner.exchange().await == Err(StepError::Failed) {
                inner.apply_backoff(true).await;
            }
        });
    }
}
