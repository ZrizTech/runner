//! Per-run store of cli process handles: which declared command a `start`
//! opened under (run-id, resource, handle), so `read`/`wait`/`stop` need only
//! the handle. Memory only. No idle limit (the worker's `idle-ms` can be an
//! hour): the run-end notice frees the names. At most [`MAX_HANDLES`] entries;
//! the oldest goes first, with one WARN `state dropped` (kind `handles`).

use std::collections::HashMap;
use std::sync::Mutex;

/// The most handle names kept.
pub(crate) const MAX_HANDLES: usize = 1000;

type Key = (String, String, String);

struct Entry {
    command: String,
    /// Order of `put`: the smallest is the oldest.
    seq: u64,
}

#[derive(Default)]
struct State {
    map: HashMap<Key, Entry>,
    seq: u64,
}

#[derive(Default)]
pub(crate) struct Handles {
    inner: Mutex<State>,
}

fn key(run: &str, resource: &str, handle: &str) -> Key {
    (run.into(), resource.into(), handle.into())
}

impl Handles {
    /// The command name remembered for the handle, if any.
    pub(crate) fn get(&self, run: &str, resource: &str, handle: &str) -> Option<String> {
        let st = self.inner.lock().ok()?;
        st.map
            .get(&key(run, resource, handle))
            .map(|e| e.command.clone())
    }

    pub(crate) fn put(&self, run: &str, resource: &str, handle: &str, command: &str) {
        let Ok(mut st) = self.inner.lock() else {
            return;
        };
        let k = key(run, resource, handle);
        if !st.map.contains_key(&k)
            && st.map.len() >= MAX_HANDLES
            && let Some(oldest) = st
                .map
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(k, _)| k.clone())
        {
            tracing::warn!(target: "runner.ops", run_id = %oldest.0, kind = "handles", "state dropped");
            st.map.remove(&oldest);
        }
        st.seq += 1;
        let seq = st.seq;
        st.map.insert(
            k,
            Entry {
                command: command.to_string(),
                seq,
            },
        );
    }

    /// Drops every handle name of `run_id` (the run ended).
    pub(crate) fn forget_run(&self, run_id: &str) {
        if let Ok(mut st) = self.inner.lock() {
            st.map.retain(|(run, _, _), _| run != run_id);
        }
    }

    pub(crate) fn forget(&self, run: &str, resource: &str, handle: &str) {
        if let Ok(mut st) = self.inner.lock() {
            st.map.remove(&key(run, resource, handle));
        }
    }
}
