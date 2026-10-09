//! Building the exchange request, dispatching the ops in a response onto
//! the bounded pool, and building the result/error frames a finished op
//! replies with.

use super::batch::{self, BATCH_CAP};
use super::health::{self, Snapshot};
use super::{Inner, StepError};
use crate::contract;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// One WARN `poll failed`; `error` is a short fixed word, never library text.
fn poll_failed(status: Option<u16>, error: &str, t0: Instant) {
    let us = crate::logfmt::micros(t0.elapsed());
    tracing::warn!(target: "runner.exchange", http_status = status, error = error, elapsed_ms = us, "poll failed");
}

/// A poll that took this long or longer is worth an INFO line.
const SLOW_POLL: Duration = Duration::from_millis(35_000);

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
    /// Takes the frames of the next request from the queue (see
    /// `batch::first_batch`); the rest stays queued.
    fn drain_queue(&self) -> Vec<contract::Frame> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let sizes: Vec<usize> = q
            .iter()
            .map(|f| serde_json::to_vec(f).map_or(0, |b| b.len()))
            .collect();
        let r = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        let lone: Vec<bool> = q.iter().map(|f| r.is_lone(&f.id)).collect();
        let n = batch::first_batch(&sizes, &lone, BATCH_CAP);
        q.drain(..n).collect()
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

    pub(super) fn push_back(&self, frame: contract::Frame) {
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

    /// The cloud did not accept the request (no answer, 4xx, 5xx, a bad 200
    /// body): its counts go back, to be sent in the next request. (A request
    /// the cloud accepted keeps them out: they start at 0 when built.)
    fn counts_unsent(&self, snap: &Snapshot) {
        self.refused.fetch_add(snap.refused, Ordering::SeqCst);
        self.errors.fetch_add(snap.errors, Ordering::SeqCst);
    }

    /// Counts the end of one op for the health of the next request.
    pub(super) fn count_reply(&self, err: Option<&contract::Error>) {
        let Some(e) = err else { return };
        let place = e.details.get("where").and_then(|v| v.as_str());
        if e.reason == "runner-at-capacity" {
            self.refused.fetch_add(1, Ordering::SeqCst);
        } else if health::is_runner_fault(&e.reason, place) {
            self.errors.fetch_add(1, Ordering::SeqCst);
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

        let result = self.handle_response(resp, sent, t0).await;
        match result {
            // The cloud did not accept the request: its counts go back.
            Err(_) => self.counts_unsent(&snap),
            Ok(()) => {
                let more = !self
                    .queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty();
                if more {
                    self.trigger_exchange();
                }
            }
        }
        result
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
                if is_refusal(status) {
                    let alone = sent.len() == 1;
                    let kept = self.drop_refused(status, sent);
                    // A frame refused alone goes behind the others.
                    if alone {
                        kept.into_iter().for_each(|f| self.push_back(f));
                    } else {
                        self.push_front(kept);
                    }
                } else {
                    self.push_front(sent);
                }
                poll_failed(Some(status), "bad-status", t0);
                Err(StepError::Failed)
            }
        }
    }

    /// The cloud took `sent`: it is not refused any more.
    fn forget_refusals(&self, sent: &[contract::Frame]) {
        let mut m = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        for f in sent {
            m.forget(&f.id);
        }
    }

    /// The cloud refused the request of `sent` (see `Refusals::refused`).
    /// One ERROR line says how many frames were dropped; the rest is
    /// returned to be queued again.
    fn drop_refused(&self, status: u16, sent: Vec<contract::Frame>) -> Vec<contract::Frame> {
        let ids: Vec<&str> = sent.iter().map(|f| f.id.as_str()).collect();
        let drop = {
            let mut m = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
            m.refused(&ids, Instant::now(), self.cfg.backoff)
        };
        let dropped = drop.iter().filter(|d| **d).count();
        if dropped > 0 {
            tracing::error!(target: "runner.exchange", http_status = status, count = dropped as u64, reason = "refused", "frames dropped");
        }
        sent.into_iter()
            .zip(drop)
            .filter_map(|(f, d)| (!d).then_some(f))
            .collect()
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
}
