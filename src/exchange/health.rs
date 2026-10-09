//! The health the runner sends in each exchange request: a small pure
//! function of a snapshot, so the state rules need no I/O to test.

use crate::contract::{BrowserLoad, CliLoad, Health};

/// What the handler knows about the worker, from its last ping and the
/// config. `needed` is false when no resource needs the worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerHealth {
    pub needed: bool,
    pub up: bool,
    /// One item for each `browser` resource; empty with none.
    pub browser: Vec<BrowserLoad>,
    /// `None` with no `cli` resource.
    pub cli: Option<CliLoad>,
}

/// Everything the state rules read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub inflight: i64,
    pub max_inflight: i64,
    pub worker: WorkerHealth,
    /// Ops refused with `runner-at-capacity` after the last request.
    pub refused: u64,
    /// Ops ended with `runner-error` or `worker-error` after the last request.
    pub errors: u64,
}

fn full(busy: u64, limit: u64) -> bool {
    limit > 0 && busy >= limit
}

/// The state, by the first rule that applies: `degraded`, `busy`, `ok`.
pub fn state(s: &Snapshot) -> &'static str {
    if (s.worker.needed && !s.worker.up) || s.errors > 0 {
        return "degraded";
    }
    let ops_full = s.max_inflight > 0 && s.inflight >= s.max_inflight;
    let browser_full = s.worker.browser.iter().any(|b| full(b.busy, b.limit));
    let cli_full = s.worker.cli.as_ref().is_some_and(|c| full(c.busy, c.limit));
    if ops_full || browser_full || cli_full || s.refused > 0 {
        return "busy";
    }
    "ok"
}

/// The health object of one request.
pub fn health(boot_id: &str, s: &Snapshot) -> Health {
    Health {
        boot_id: boot_id.to_string(),
        state: state(s).to_string(),
        browser: (!s.worker.browser.is_empty()).then(|| s.worker.browser.clone()),
        cli: s.worker.cli.clone(),
        worker: s
            .worker
            .needed
            .then(|| if s.worker.up { "up" } else { "down" }.to_string()),
        refused: s.refused,
    }
}

/// A new `b-<12 hex>` id, made once when the process starts.
pub fn new_boot_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("b-{}", &u[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(busy: u64, limit: u64) -> BrowserLoad {
        BrowserLoad {
            resource: "web".into(),
            busy,
            limit,
        }
    }

    fn base() -> Snapshot {
        Snapshot {
            inflight: 1,
            max_inflight: 4,
            worker: WorkerHealth {
                needed: true,
                up: true,
                browser: vec![load(1, 3)],
                cli: Some(CliLoad { busy: 0, limit: 2 }),
            },
            ..Snapshot::default()
        }
    }

    #[test]
    fn state_rules_in_order() {
        let mut down = base();
        down.worker.up = false;
        let mut errors = base();
        errors.errors = 1;
        let mut ops_full = base();
        ops_full.inflight = 4;
        let mut browser_full = base();
        browser_full.worker.browser = vec![load(3, 3)];
        let mut cli_full = base();
        cli_full.worker.cli = Some(CliLoad { busy: 2, limit: 2 });
        let mut refused = base();
        refused.refused = 2;
        let mut both = ops_full.clone();
        both.worker.up = false;
        let mut no_worker = base();
        no_worker.worker = WorkerHealth::default();
        for (name, snap, want) in [
            ("all quiet", base(), "ok"),
            ("worker down", down, "degraded"),
            ("error after last request", errors, "degraded"),
            ("ops full", ops_full, "busy"),
            ("browser full", browser_full, "busy"),
            ("cli full", cli_full, "busy"),
            ("refused", refused, "busy"),
            ("degraded before busy", both, "degraded"),
            ("no resource needs the worker", no_worker, "ok"),
        ] {
            assert_eq!(state(&snap), want, "{name}");
        }
    }

    #[test]
    fn health_keys_follow_the_resources() {
        let h = health("b-0123456789ab", &base());
        assert_eq!(h.worker.as_deref(), Some("up"));
        assert_eq!(h.browser.as_ref().map(Vec::len), Some(1));
        assert!(h.cli.is_some());
        let bare = health("b-0123456789ab", &Snapshot::default());
        assert!(bare.browser.is_none() && bare.cli.is_none() && bare.worker.is_none());
    }

    #[test]
    fn boot_id_shape() {
        let id = new_boot_id();
        assert_eq!(id.len(), 14);
        assert!(id.starts_with("b-") && id[2..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(id, new_boot_id());
    }
}
