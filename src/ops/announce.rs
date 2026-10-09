//! What the runner announces to the cloud (op kinds, resources), the worker
//! liveness check behind it, and the startup warning for http resources
//! without a `secrets` allowlist.

use super::*;

/// How long a worker ping result is trusted before the next request re-checks.
/// The numbers of the health are as old as this at most.
const PING_EVERY: Duration = Duration::from_secs(1);

/// Contexts a browser resource may hold when its config sets no `max-contexts`.
const DEFAULT_MAX_CONTEXTS: u64 = 3;

/// Handles a cli resource may hold when its config sets no `max-handles`.
const DEFAULT_MAX_HANDLES: u64 = 2;

/// Startup checks. Warns about secretless http resources and pings the
/// worker; returns whether the worker is up.
pub(super) async fn startup(cfg: &Config) -> bool {
    for id in secretless_http_resources(cfg) {
        tracing::warn!(target: "runner.ops", resource = %id, "no allowlist");
    }
    let up = needs_worker(cfg) && worker::ping(&cfg.worker_socket).await;
    if needs_worker(cfg) {
        if up {
            tracing::info!(target: "runner.ops", "worker up");
        } else {
            tracing::warn!(target: "runner.ops", "worker down");
        }
    }
    up
}

impl Handler {
    /// The op kinds to announce: the built-in three, plus `browser.page`
    /// / `cli.exec` when such a resource exists and the worker answered its last ping.
    pub fn op_kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = ["http.request", "sql.query", "evidence.fetch"]
            .map(String::from)
            .to_vec();
        if self.worker_up.load(Ordering::SeqCst) {
            if has_type(&self.cfg, "browser") {
                kinds.push("browser.page".to_string());
            }
            if has_type(&self.cfg, "cli") {
                kinds.push("cli.exec".to_string());
            }
        }
        kinds
    }

    /// Pings the worker (at most every few seconds) and records whether it
    /// is up. Called before each announcement.
    pub async fn refresh_worker(&self) {
        if !needs_worker(&self.cfg) {
            return;
        }
        {
            let Ok(mut last) = self.last_ping.lock() else {
                return;
            };
            if last.is_some_and(|t| t.elapsed() < PING_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let info = worker::ping_info(&self.cfg.worker_socket).await;
        let up = info.is_some();
        if let Some(boot) = info
            .as_ref()
            .and_then(|p| p.get("boot-id"))
            .and_then(Value::as_str)
        {
            self.contexts.note_boot(boot);
        }
        if let Ok(mut last) = self.ping_info.lock() {
            *last = info;
        }
        let was = self.worker_up.swap(up, Ordering::SeqCst);
        if up && !was {
            tracing::info!(target: "runner.ops", "worker up");
        } else if !up && was {
            tracing::warn!(target: "runner.ops", "worker lost");
        }
    }

    /// The worker numbers for the health: `busy` from the last ping, the
    /// limits from the config (a browser resource has no limit in the ping).
    pub fn worker_health(&self) -> WorkerHealth {
        let needed = needs_worker(&self.cfg);
        let up = self.worker_up.load(Ordering::SeqCst);
        let ping = self.ping_info.lock().ok().and_then(|p| p.clone());
        let busy_of = |id: &str| {
            ping.as_ref()
                .and_then(|p| {
                    p.get("browser")?.as_array()?.iter().find_map(|b| {
                        (b.get("resource")?.as_str()? == id).then(|| b.get("busy")?.as_u64())?
                    })
                })
                .unwrap_or(0)
        };
        let mut ids: Vec<&String> = self
            .cfg
            .resources
            .iter()
            .filter(|(_, r)| r.r#type == "browser")
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        let browser = ids
            .into_iter()
            .map(|id| contract::BrowserLoad {
                resource: id.clone(),
                busy: busy_of(id),
                limit: self.cfg.resources[id]
                    .max_contexts
                    .map_or(DEFAULT_MAX_CONTEXTS, u64::from),
            })
            .collect();
        let cli_limit: u64 = self
            .cfg
            .resources
            .values()
            .filter(|r| r.r#type == "cli")
            .map(|r| r.max_handles.map_or(DEFAULT_MAX_HANDLES, u64::from))
            .sum();
        let cli_ping = ping.as_ref().and_then(|p| p.get("cli"));
        let num = |k: &str| cli_ping.and_then(|c| c.get(k)?.as_u64());
        let cli = has_type(&self.cfg, "cli").then(|| contract::CliLoad {
            busy: num("busy").unwrap_or(0),
            limit: num("limit").unwrap_or(cli_limit),
        });
        WorkerHealth {
            needed,
            up,
            browser,
            cli,
        }
    }

    /// The resource ids to announce: `configured` minus browser and cli resources
    /// while the worker is down.
    pub fn announced_resources(&self, configured: &[String]) -> Vec<String> {
        let up = self.worker_up.load(Ordering::SeqCst);
        configured
            .iter()
            .filter(|id| {
                up || self
                    .cfg
                    .resources
                    .get(id.as_str())
                    .is_none_or(|r| !matches!(r.r#type.as_str(), "browser" | "cli"))
            })
            .cloned()
            .collect()
    }
}

fn has_type(cfg: &Config, ty: &str) -> bool {
    !cfg.worker_socket.is_empty() && cfg.resources.values().any(|r| r.r#type == ty)
}

/// True when some resource needs the worker sidecar.
fn needs_worker(cfg: &Config) -> bool {
    has_type(cfg, "browser") || has_type(cfg, "cli")
}

/// Ids of http resources that declare no `secrets` allowlist, sorted.
pub(super) fn secretless_http_resources(cfg: &Config) -> Vec<String> {
    let mut ids: Vec<String> = cfg
        .resources
        .iter()
        .filter(|(_, r)| r.r#type == "http" && r.secrets.is_none())
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    ids
}
