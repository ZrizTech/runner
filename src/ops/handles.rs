//! Per-run store of cli process handles: which declared command a `start`
//! opened under (run-id, resource, handle), so `read`/`wait`/`stop` need only
//! the handle. Same lifetime rules as the cookie jars: memory only, dropped
//! after [`IDLE_TTL`] without use, at most [`MAX_JARS`] entries (longest idle
//! goes first).

use super::jar::{IDLE_TTL, MAX_JARS};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

type Key = (String, String, String);

struct Entry {
    command: String,
    last_used: SystemTime,
}

#[derive(Default)]
pub(crate) struct Handles {
    inner: Mutex<HashMap<Key, Entry>>,
}

fn key(run: &str, resource: &str, handle: &str) -> Key {
    (run.into(), resource.into(), handle.into())
}

impl Handles {
    /// The command name remembered for the handle, if any.
    pub(crate) fn get(
        &self,
        run: &str,
        resource: &str,
        handle: &str,
        now: SystemTime,
    ) -> Option<String> {
        let mut map = self.inner.lock().ok()?;
        evict_idle(&mut map, now);
        let e = map.get_mut(&key(run, resource, handle))?;
        e.last_used = now;
        Some(e.command.clone())
    }

    pub(crate) fn put(
        &self,
        run: &str,
        resource: &str,
        handle: &str,
        command: &str,
        now: SystemTime,
    ) {
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        evict_idle(&mut map, now);
        let k = key(run, resource, handle);
        if !map.contains_key(&k)
            && map.len() >= MAX_JARS
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(
            k,
            Entry {
                command: command.to_string(),
                last_used: now,
            },
        );
    }

    pub(crate) fn forget(&self, run: &str, resource: &str, handle: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&key(run, resource, handle));
        }
    }
}

fn evict_idle(map: &mut HashMap<Key, Entry>, now: SystemTime) {
    map.retain(|_, e| {
        now.duration_since(e.last_used)
            .map_or(true, |d| d <= IDLE_TTL)
    });
}
