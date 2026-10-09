//! Per-run vault for values an op `capture`d. Keyed by (run-id, NAME), in
//! memory only, same lifetime rules as the cookie jars: kept until the
//! run-end notice, at most [`MAX_JARS`] runs (longest idle goes first, with
//! one WARN `state dropped`). Values leave this module only as `${NAME}`
//! substitutions and as scrub-list entries.

use super::jar::MAX_JARS;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

struct Run {
    last_used: SystemTime,
    values: HashMap<String, String>,
}

#[derive(Default)]
pub(crate) struct Vault {
    inner: Mutex<HashMap<String, Run>>,
}

impl Vault {
    /// The captured value for `name` in `run_id`, if any.
    pub(crate) fn get(&self, run_id: &str, name: &str, now: SystemTime) -> Option<String> {
        let mut map = self.inner.lock().ok()?;
        let run = map.get_mut(run_id)?;
        run.last_used = now;
        run.values.get(name).cloned()
    }

    /// Every captured value of `run_id`, for an op's scrub list.
    pub(crate) fn values(&self, run_id: &str, now: SystemTime) -> Vec<String> {
        let Ok(mut map) = self.inner.lock() else {
            return Vec::new();
        };
        match map.get_mut(run_id) {
            Some(run) => {
                run.last_used = now;
                run.values.values().cloned().collect()
            }
            None => Vec::new(),
        }
    }

    /// Stores `entries` under `run_id`, replacing same-named values.
    pub(crate) fn put(&self, run_id: &str, entries: &[(String, String)], now: SystemTime) {
        if entries.is_empty() {
            return;
        }
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        if !map.contains_key(run_id) {
            if map.len() >= MAX_JARS
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, r)| r.last_used)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
                tracing::warn!(target: "runner.ops", run_id = %oldest, kind = "vault", "state dropped");
            }
            map.insert(
                run_id.to_string(),
                Run {
                    last_used: now,
                    values: HashMap::new(),
                },
            );
        }
        if let Some(run) = map.get_mut(run_id) {
            run.last_used = now;
            for (k, v) in entries {
                run.values.insert(k.clone(), v.clone());
            }
        }
    }

    /// Drops the captured values of `run_id` (the run ended).
    pub(crate) fn forget(&self, run_id: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(run_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn run_count(&self) -> usize {
        self.inner.lock().map_or(0, |m| m.len())
    }
}
