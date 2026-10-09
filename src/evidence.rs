//! Holds recent op results in memory so the cloud can fetch the full,
//! unprojected body of a prior op with `evidence.fetch`.

use crate::scrub;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const MAX_ENTRIES: usize = 1000;
const ENTRY_TTL: Duration = Duration::from_secs(120);
const MAX_EXCERPT_LEN: usize = 4096;

/// One stored op result, kept in full so it can be redacted and excerpted
/// on demand.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub op_id: String,
    pub status: i32,
    pub body: Value,
    pub secrets: Vec<String>,
}

struct Record {
    entry: Entry,
    stored: SystemTime,
    seq: u64,
}

/// Holds recent evidence entries, keyed by run ID, evicting the oldest
/// entry once it holds more than 1000. `now` is injected on every call, so
/// the clock is never read inside the store.
pub struct Store {
    inner: Mutex<Inner>,
}

struct Inner {
    by_run: HashMap<String, Record>,
    next_seq: u64,
}

impl Store {
    /// Returns an empty, ready-to-use Store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                by_run: HashMap::new(),
                next_seq: 0,
            }),
        }
    }

    /// Stores `entry` under `run_id`, evicting the oldest entry first if
    /// the store is already at capacity.
    pub fn put(&self, run_id: &str, entry: Entry, now: SystemTime) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.by_run.contains_key(run_id) && inner.by_run.len() >= MAX_ENTRIES {
            evict_oldest(&mut inner.by_run);
        }
        inner.next_seq += 1;
        let seq = inner.next_seq;
        inner.by_run.insert(
            run_id.to_string(),
            Record {
                entry,
                stored: now,
                seq,
            },
        );
    }

    /// Drops the entry of `run_id` (the run ended).
    pub fn forget(&self, run_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.by_run.remove(run_id);
    }

    /// Returns the entry stored under `run_id`, or `None` if absent or
    /// older than 120s (in which case it is also deleted).
    pub fn get(&self, run_id: &str, now: SystemTime) -> Option<Entry> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let is_expired = {
            let rec = inner.by_run.get(run_id)?;
            now.duration_since(rec.stored)
                .map(|age| age > ENTRY_TTL)
                .unwrap_or(false)
        };
        if is_expired {
            inner.by_run.remove(run_id);
            return None;
        }
        inner.by_run.get(run_id).map(|rec| rec.entry.clone())
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

fn evict_oldest(by_run: &mut HashMap<String, Record>) {
    let oldest_run = by_run
        .iter()
        .min_by_key(|(_, rec)| rec.seq)
        .map(|(run, _)| run.clone());
    if let Some(run) = oldest_run {
        by_run.remove(&run);
    }
}

/// Errors excerpting an evidence entry.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    #[error("evidence: encode excerpt: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Masks every value whose key is in `deny_keys` (case-insensitive, at any
/// depth), scrubs `entry.secrets` out of every string value and key,
/// JSON-encodes the result, and cuts it to at most 4096 bytes without
/// splitting a UTF-8 sequence. Scrubbing comes first so a secret that
/// straddles the cut cannot leave a prefix behind.
pub fn excerpt(entry: &Entry, deny_keys: &[String]) -> std::result::Result<String, EvidenceError> {
    let deny: std::collections::HashSet<String> =
        deny_keys.iter().map(|k| k.to_lowercase()).collect();

    let masked = mask_value(&entry.body, &deny);
    let (scrubbed, _) = scrub::scrub_value(&masked, &entry.secrets);
    let encoded = serde_json::to_string(&scrubbed)?;

    Ok(truncate_utf8(&encoded, MAX_EXCERPT_LEN).to_string())
}

fn mask_value(v: &Value, deny: &std::collections::HashSet<String>) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::with_capacity(m.len());
            for (k, child) in m {
                if deny.contains(&k.to_lowercase()) {
                    out.insert(k.clone(), Value::String("[redacted]".to_string()));
                } else {
                    out.insert(k.clone(), mask_value(child, deny));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|x| mask_value(x, deny)).collect()),
        other => other.clone(),
    }
}

fn truncate_utf8(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut cut = limit;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(secs_from_epoch: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs_from_epoch)
    }

    #[test]
    fn store_put_get() {
        let now = at(1_704_067_200); // 2024-01-01T00:00:00Z
        let s = Store::new();
        s.put(
            "run-1",
            Entry {
                op_id: "op-1".to_string(),
                ..Default::default()
            },
            now,
        );

        assert!(s.get("run-1", now + Duration::from_secs(119)).is_some());
        assert!(s.get("run-1", now + Duration::from_secs(121)).is_none());
        assert!(s.get("run-1", now + Duration::from_secs(121)).is_none());
    }

    #[test]
    fn store_evicts_oldest_over_capacity() {
        let now = at(1_704_067_200);
        let s = Store::new();
        for i in 0..1001 {
            s.put(
                &format!("run-{i}"),
                Entry {
                    op_id: "op".to_string(),
                    ..Default::default()
                },
                now,
            );
        }
        assert!(
            s.get("run-0", now).is_none(),
            "run-0 still present, want evicted"
        );
        assert!(
            s.get("run-1000", now).is_some(),
            "run-1000 missing, want present"
        );
    }

    #[test]
    fn excerpt_redacts_deny_keys() {
        let e = Entry {
            body: json!({"a": {"b": {"Set-Cookie": "x", "keep": "y"}}}),
            ..Default::default()
        };
        let got = excerpt(&e, &["set-cookie".to_string()]).expect("excerpt");
        assert!(got.contains(r#""Set-Cookie":"[redacted]""#), "got: {got}");
        assert!(got.contains(r#""keep":"y""#), "got: {got}");
    }

    #[test]
    fn excerpt_truncates_to_valid_utf8() {
        let e = Entry {
            body: json!({"text": "é".repeat(10000)}),
            ..Default::default()
        };
        let got = excerpt(&e, &[]).expect("excerpt");
        assert!(got.len() <= MAX_EXCERPT_LEN);
        assert!(std::str::from_utf8(got.as_bytes()).is_ok());
    }

    #[test]
    fn excerpt_scrubs_secrets() {
        let e = Entry {
            body: json!({"note": "leaked s3cret here"}),
            secrets: vec!["s3cret".to_string()],
            ..Default::default()
        };
        let got = excerpt(&e, &[]).expect("excerpt");
        assert!(!got.contains("s3cret"));
        assert!(got.contains("[scrubbed]"));
    }

    #[test]
    fn excerpt_scrubs_before_it_cuts() {
        let secret = "QZXWVUTSRP";
        let e = Entry {
            body: json!({"pad": "x".repeat(4075), "t": secret}),
            secrets: vec![secret.to_string()],
            ..Default::default()
        };
        let got = excerpt(&e, &[]).expect("excerpt");
        assert!(got.len() <= MAX_EXCERPT_LEN);
        assert!(!got.contains("QZXW"), "prefix leaked: {}", &got[4080..]);
    }
}
