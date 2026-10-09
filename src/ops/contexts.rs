//! The (run, resource) pairs that had a browser context since the last
//! `boot-id` of the worker. A new `boot-id` means the worker restarted and
//! lost them: the next `browser.page` op of each pair gets `context-lost`
//! once (`why: worker-restarted`). Memory only, next to the vault.

use std::collections::VecDeque;
use std::sync::Mutex;

/// The most pairs kept; the oldest goes first (as the vault).
pub(crate) const MAX_PAIRS: usize = 1000;

type Pair = (String, String);

#[derive(Default)]
struct Inner {
    boot: Option<String>,
    live: VecDeque<Pair>,
    lost: Vec<Pair>,
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
                i.lost.extend(live);
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

    /// The run ended: drop its pairs.
    pub(crate) fn forget_run(&self, run: &str) {
        if let Ok(mut i) = self.inner.lock() {
            i.live.retain(|(r, _)| r != run);
            i.lost.retain(|(r, _)| r != run);
        }
    }
}
