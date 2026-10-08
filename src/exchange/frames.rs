//! Building the exchange request, dispatching the ops in a response onto
//! the bounded pool, and building the result/error frames a finished op
//! replies with.

use super::{ErrorFrame, Inner, ResultFrame, StepError};
use crate::contract;
use futures::FutureExt;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::Instrument;

/// One WARN `poll failed`; `error` is a short fixed word, never library text.
fn poll_failed(status: Option<u16>, error: &str, t0: Instant) {
    let us = crate::logfmt::micros(t0.elapsed());
    tracing::warn!(target: "runner.exchange", http_status = status, error = error, elapsed_ms = us, "poll failed");
}

/// One DEBUG `poll done` line. `status` is `None` when no HTTP answer came.
fn poll_done(status: Option<u16>, sent: usize, received: usize, t0: Instant) {
    let us = crate::logfmt::micros(t0.elapsed());
    tracing::debug!(target: "runner.exchange", http_status = status, sent = sent, received = received, elapsed_ms = us, "poll done");
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

    fn build_request(&self, frames: Vec<contract::Frame>) -> contract::ExchangeRequest {
        let inflight = (self.capacity - self.semaphore.available_permits()) as i64;
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
            inflight,
            frames,
        }
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
        let req = self.build_request(sent.clone());

        let t0 = Instant::now();
        let post_result = tokio::select! {
            r = self.post(&req) => Some(r),
            () = self.cancel.cancelled() => None,
        };

        let resp = match post_result {
            None => {
                self.push_front(sent);
                return Err(StepError::Cancelled);
            }
            Some(Ok(r)) => r,
            Some(Err(e)) => {
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
                self.push_front(sent);
                poll_failed(Some(other.as_u16()), "bad-status", t0);
                Err(StepError::Failed)
            }
        }
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
        poll_done(Some(200), sent.len(), body.frames.len(), t0);
        self.connected();
        self.dispatch(body.frames);
        Ok(())
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
        tracing::error!(target: "runner.ops", run_id = %op.run_id, step = op.step_index, op_id = %op_id, kind = %op.kind, resource = %op.resource, status = "error", reason = "runner-at-capacity", error = "capacity", trace_id = op.valid_trace_id().unwrap_or("-"), "op failed");
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

/// Errors building a reply frame: either the handler broke its contract
/// (returned neither a result nor an error), or the reply body failed to
/// encode. Either way the op's reply is dropped and only logged, never sent
/// as a synthesized frame.
#[derive(Debug, thiserror::Error)]
enum ReplyError {
    #[error("exchange: handler returned neither result nor error")]
    NeitherResultNorError,
    #[error("exchange: marshal frame body: {0}")]
    Marshal(#[source] serde_json::Error),
}

fn reply_frame(
    frame_id: &str,
    result: Option<ResultFrame>,
    op_err: Option<ErrorFrame>,
) -> std::result::Result<contract::Frame, ReplyError> {
    if let Some(e) = op_err {
        return error_frame(frame_id, e).map_err(ReplyError::Marshal);
    }
    if let Some(r) = result {
        return result_frame(frame_id, r).map_err(ReplyError::Marshal);
    }
    Err(ReplyError::NeitherResultNorError)
}

fn result_frame(
    frame_id: &str,
    result: ResultFrame,
) -> std::result::Result<contract::Frame, serde_json::Error> {
    new_frame("result", frame_id, result)
}

fn error_frame(
    frame_id: &str,
    op_err: ErrorFrame,
) -> std::result::Result<contract::Frame, serde_json::Error> {
    new_frame("error", frame_id, op_err)
}

fn new_frame(
    t: &str,
    re: &str,
    d: impl serde::Serialize,
) -> std::result::Result<contract::Frame, serde_json::Error> {
    let value = serde_json::to_value(d)?;
    Ok(contract::Frame {
        v: 1,
        t: t.to_string(),
        id: uuid::Uuid::new_v4().to_string(),
        re: Some(re.to_string()),
        ts: now_millis(),
        d: value,
    })
}

fn now_millis() -> i64 {
    crate::logfmt::millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    )
}
