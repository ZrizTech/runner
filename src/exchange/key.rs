//! The one 401 retry: re-read the key from `cfg.key_source` and try the
//! exchange once more after a backoff.

use super::{Inner, StepError};
use std::sync::Arc;

impl Inner {
    /// Re-reads the key from `cfg.key_source` for the one retry `run`
    /// allows after a 401. Returns `false` — no retry — when there is no
    /// `key_source` to re-read from, or the source itself errors; either
    /// way `run` fails closed on the 401 it already has.
    fn refresh_key(&self) -> bool {
        let Some(source) = &self.cfg.key_source else {
            return false;
        };
        match source() {
            Ok(key) => {
                let mut t = self.key.write().unwrap_or_else(|e| e.into_inner());
                *t = key;
                true
            }
            Err(e) => {
                tracing::error!(target: "runner.exchange", error = %e, "refresh failed");
                false
            }
        }
    }

    /// Handles one 401 from the main loop: re-reads the key and, only if
    /// that succeeds, waits out one backoff interval and retries the
    /// exchange once more.
    ///
    /// Returns `None` only when cancellation fired during that wait,
    /// telling `run` to shut down cleanly instead of interpreting the
    /// cancellation as a failed retry. Otherwise returns the retried
    /// exchange's own result — which `run` treats as: unauthorized again
    /// means fail closed, anything else (including success) means keep
    /// going, same as any other exchange outcome.
    pub(super) async fn retry_unauthorized(
        self: &Arc<Self>,
        code: &'static str,
    ) -> Option<std::result::Result<(), StepError>> {
        if !self.refresh_key() {
            return Some(Err(StepError::Unauthorized(code)));
        }
        if !self.apply_backoff(true).await {
            return None;
        }
        Some(self.exchange().await)
    }
}
