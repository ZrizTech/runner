//! The run-end notice: free every resource a run got on the runner.

use super::{Handler, worker};
use std::collections::VecDeque;
use std::sync::Mutex;

/// The most ended run ids kept; the oldest goes first.
const MAX_ENDED: usize = 1000;

/// The most `run.close` calls kept for a retry.
const MAX_RETRY: usize = 64;

/// The `run.close` calls the worker did not take (run id, trace id).
#[derive(Default)]
pub(crate) struct CloseRetry {
    list: Mutex<VecDeque<(String, String)>>,
}

impl CloseRetry {
    fn add(&self, run: &str, trace: &str) {
        let Ok(mut l) = self.list.lock() else { return };
        if l.iter().any(|(r, _)| r == run) {
            return;
        }
        if l.len() >= MAX_RETRY {
            l.pop_front();
        }
        l.push_back((run.to_string(), trace.to_string()));
    }

    fn take_all(&self) -> Vec<(String, String)> {
        self.list
            .lock()
            .map(|mut l| l.drain(..).collect())
            .unwrap_or_default()
    }
}

/// The runs that got a run-end notice. An op of such a run is refused, and
/// an op of it that ends later keeps no state.
#[derive(Default)]
pub(crate) struct EndedRuns {
    ids: Mutex<VecDeque<String>>,
}

impl EndedRuns {
    pub(crate) fn mark(&self, run: &str) {
        let Ok(mut ids) = self.ids.lock() else { return };
        if ids.iter().any(|r| r == run) {
            return;
        }
        if ids.len() >= MAX_ENDED {
            ids.pop_front();
        }
        ids.push_back(run.to_string());
    }

    pub(crate) fn contains(&self, run: &str) -> bool {
        self.ids
            .lock()
            .is_ok_and(|ids| ids.iter().any(|r| r == run))
    }
}

impl Handler {
    /// Frees every piece of state of `run_id` on the runner.
    pub(super) fn free_run(&self, run_id: &str) {
        self.jars.forget_run(run_id);
        self.vault.forget(run_id);
        self.handles.forget_run(run_id);
        self.contexts.forget_run(run_id);
        self.handle_lives.forget_run(run_id);
        self.evidence.forget(run_id);
    }

    /// Frees the cookie jars, the captured values, the cli handle names and
    /// the evidence of `run_id`, whatever its status, then tells the worker
    /// to close its contexts and handles. A worker that is down is not an
    /// error: it closes them when its idle sweep runs.
    pub async fn run_ended(&self, run_id: &str, trace_id: &str) {
        // First the mark, then the free: an op that ends after the mark frees
        // its own state (`free_ended`).
        self.ended_runs.mark(run_id);
        self.free_run(run_id);
        if self.cfg.worker_socket.is_empty() {
            return;
        }
        // Whatever the flag says: it may be down for a second only.
        if worker::run_close(&self.cfg.worker_socket, run_id, trace_id)
            .await
            .is_ok()
        {
            self.flush_closes().await;
        } else {
            self.close_retry.add(run_id, trace_id);
        }
    }

    /// Sends the `run.close` of the runs the worker did not take before.
    /// Called after a good worker call. A failure puts the run back.
    pub(super) async fn flush_closes(&self) {
        for (run, trace) in self.close_retry.take_all() {
            if worker::run_close(&self.cfg.worker_socket, &run, &trace)
                .await
                .is_err()
            {
                self.close_retry.add(&run, &trace);
            }
        }
    }
}
