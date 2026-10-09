//! The (run, resource) pairs that had a browser context since the last
//! `boot-id` of the worker. A new `boot-id` means the worker restarted and
//! lost them: the next `browser.page` op of each pair gets `context-lost`
//! once (`why: worker-restarted`). The pair is (run, name): a browser
//! resource for the contexts, `resource/handle` for the cli handles (one
//! instance for each). Memory only, next to the vault.

use std::collections::VecDeque;
use std::sync::Mutex;

/// The most pairs kept; the oldest goes first (as the vault).
pub(crate) const MAX_PAIRS: usize = 1000;

type Pair = (String, String);

#[derive(Default)]
struct Inner {
    boot: Option<String>,
    live: VecDeque<Pair>,
    lost: VecDeque<Pair>,
}

#[derive(Default)]
pub(crate) struct Contexts {
    inner: Mutex<Inner>,
}

impl Contexts {
    /// True when no `boot-id` is known yet, or the pair is live: the caller
    /// asks the worker for a fresh `boot-id` before the op.
    pub(crate) fn needs_boot(&self, run: &str, resource: &str) -> bool {
        self.inner.lock().is_ok_and(|i| {
            i.boot.is_none() || i.live.iter().any(|(r, s)| r == run && s == resource)
        })
    }

    /// Records the `boot-id` of a ping. A different one moves every live
    /// pair to the lost ones.
    pub(crate) fn note_boot(&self, boot: &str) {
        let Ok(mut i) = self.inner.lock() else {
            return;
        };
        match i.boot.as_deref() {
            Some(b) if b == boot => return,
            Some(_) => {
                let live: Vec<Pair> = i.live.drain(..).collect();
                for p in live {
                    if i.lost.len() >= MAX_PAIRS {
                        i.lost.pop_front();
                    }
                    i.lost.push_back(p);
                }
            }
            None => {}
        }
        i.boot = Some(boot.to_string());
    }

    /// True one time for a lost pair.
    pub(crate) fn take_lost(&self, run: &str, resource: &str) -> bool {
        let Ok(mut i) = self.inner.lock() else {
            return false;
        };
        let before = i.lost.len();
        i.lost.retain(|(r, s)| !(r == run && s == resource));
        i.lost.len() != before
    }

    /// The pair got a context from the worker.
    pub(crate) fn add(&self, run: &str, resource: &str) {
        let Ok(mut i) = self.inner.lock() else {
            return;
        };
        if i.live.iter().any(|(r, s)| r == run && s == resource) {
            return;
        }
        if i.live.len() >= MAX_PAIRS {
            i.live.pop_front();
        }
        i.live.push_back((run.to_string(), resource.to_string()));
    }

    /// The pair is finished (a stopped handle): it is not live any more.
    pub(crate) fn forget(&self, run: &str, name: &str) {
        if let Ok(mut i) = self.inner.lock() {
            i.live.retain(|(r, s)| !(r == run && s == name));
        }
    }

    /// The run ended: drop its pairs.
    pub(crate) fn forget_run(&self, run: &str) {
        if let Ok(mut i) = self.inner.lock() {
            i.live.retain(|(r, _)| r != run);
            i.lost.retain(|(r, _)| r != run);
        }
    }
}

impl super::Handler {
    /// Asks the worker for its `boot-id` when `lives` holds the pair (or knows
    /// no boot yet), then reports whether the pair was lost in a restart. True
    /// one time for a lost pair.
    pub(super) async fn pair_lost(&self, lives: &Contexts, run: &str, name: &str) -> bool {
        if lives.needs_boot(run, name)
            && let Some(boot) = super::worker::ping_info(&self.cfg.worker_socket)
                .await
                .and_then(|p| {
                    p.get("boot-id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
        {
            lives.note_boot(&boot);
        }
        lives.take_lost(run, name)
    }
}
