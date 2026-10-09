//! The run-end notice: free every resource a run got on the runner.

use super::{Handler, worker};
use std::sync::atomic::Ordering;

impl Handler {
    /// Frees the cookie jars, the captured values, the cli handle names and
    /// the evidence of `run_id`, whatever its status, then tells the worker
    /// to close its contexts and handles. A worker that is down is not an
    /// error: it closes them when its idle sweep runs.
    pub async fn run_ended(&self, run_id: &str, trace_id: &str) {
        self.jars.forget_run(run_id);
        self.vault.forget(run_id);
        self.handles.forget_run(run_id);
        self.evidence.forget(run_id);
        if self.cfg.worker_socket.is_empty() || !self.worker_up.load(Ordering::SeqCst) {
            return;
        }
        let _ = worker::run_close(&self.cfg.worker_socket, run_id, trace_id).await;
    }
}
