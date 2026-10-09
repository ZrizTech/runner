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

/// The `where` words of a `runner-error` that is a fault of the runner
/// itself. The others (a bad pipeline, a target fault) say nothing about the
/// health of the runner.
const RUNNER_FAULTS: [&str; 5] = [
    "op-handler",
    "response-encoding",
    "worker-word",
    "evidence-excerpt",
    "worker-deadline",
];

/// True when an error frame with this reason (and `where` detail) counts
/// toward `degraded`: a `worker-error`, or a `runner-error` with a runner
/// fault word.
pub fn is_runner_fault(reason: &str, place: Option<&str>) -> bool {
    match reason {
        "worker-error" => true,
        "runner-error" => place.is_some_and(|w| RUNNER_FAULTS.contains(&w)),
        _ => false,
    }
}

/// True when `now` differs from the health last sent. With nothing sent yet
/// it is false: the loop sends its first request anyway.
pub fn changed(last_sent: Option<&Health>, now: &Health) -> bool {
    last_sent.is_some_and(|l| l != now)
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

#[cfg(test)]
mod fault_tests {
    use super::is_runner_fault;

    #[test]
    fn only_real_runner_faults_count() {
        let cases: [(&str, Option<&str>, bool); 14] = [
            ("worker-error", None, true),
            ("runner-error", Some("op-handler"), true),
            ("runner-error", Some("response-encoding"), true),
            ("runner-error", Some("worker-word"), true),
            ("runner-error", Some("evidence-excerpt"), true),
            ("runner-error", Some("worker-deadline"), true),
            ("runner-error", Some("sql-driver"), false),
            ("runner-error", Some("http-client"), false),
            ("runner-error", Some("http-request"), false),
            ("runner-error", Some("browser-args"), false),
            ("runner-error", None, false),
            ("connection-error", None, false),
            ("timeout", None, false),
            ("context-lost", None, false),
        ];
        for (reason, place, want) in cases {
            assert_eq!(is_runner_fault(reason, place), want, "{reason} {place:?}");
        }
    }
}

#[cfg(test)]
mod changed_tests {
    use super::*;

    fn h(busy: u64) -> Health {
        let s = Snapshot {
            worker: WorkerHealth {
                needed: true,
                up: true,
                browser: vec![BrowserLoad {
                    resource: "web".into(),
                    busy,
                    limit: 8,
                }],
                cli: None,
            },
            max_inflight: 4,
            ..Default::default()
        };
        health("b-1", &s)
    }

    #[test]
    fn changed_is_a_plain_comparison() {
        assert!(!changed(None, &h(8)), "nothing sent yet: the loop sends");
        assert!(!changed(Some(&h(8)), &h(8)));
        assert!(changed(Some(&h(8)), &h(0)));
        assert!(changed(Some(&h(0)), &h(1)));
    }
}
