//! The one home for target-time helpers (`timing.target-ms`).

use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// The `target-ms` the worker measured: a top-level integer >= 0 of its ok
/// answer. Anything else, or absent, is `None`.
pub(super) fn worker_target_ms(resp: &Value) -> Option<i64> {
    resp.get("target-ms")
        .and_then(Value::as_i64)
        .filter(|n| *n >= 0)
}

/// A sum of monotonic waits. Only the futures given to [`Span::time`] count.
#[derive(Debug, Default)]
pub(super) struct Span {
    total: Duration,
    last_start: Option<Instant>,
}

impl Span {
    /// Awaits `fut` and adds the time it took to the span.
    pub(super) async fn time<F: Future>(&mut self, fut: F) -> F::Output {
        let start = Instant::now();
        self.last_start = Some(start);
        let out = fut.await;
        self.total += start.elapsed();
        out
    }

    /// The sum so far, in whole milliseconds.
    #[cfg(test)]
    pub(super) fn ms(&self) -> i64 {
        crate::logfmt::millis(self.total)
    }

    /// The sum so far.
    pub(super) fn total(&self) -> Duration {
        self.total
    }

    /// When the last timed future started, if any.
    pub(super) fn last_start(&self) -> Option<Instant> {
        self.last_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn span_adds_only_the_timed_futures() {
        let mut span = Span::default();
        assert_eq!(span.ms(), 0);
        assert!(span.last_start().is_none());
        for (ms, want) in [(100u64, 100), (50, 150)] {
            let out = span
                .time(async {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    7
                })
                .await;
            assert_eq!(out, 7);
            tokio::time::sleep(Duration::from_millis(1000)).await; // not timed
            assert_eq!(span.ms(), want);
        }
        assert!(span.last_start().is_some());
    }
}
