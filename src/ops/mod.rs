//! Dispatches a decoded [`crate::contract::Op`] to exactly one of a
//! [`crate::contract::Result`] or a [`crate::contract::Error`]:
//! http.request, sql.query, evidence.fetch.

mod announce;
mod browser;
mod capture;
mod cli;
mod contexts;
mod ended;
mod errors;
mod evidence;
mod handles;
mod http;
mod jar;
mod log;
mod sql;
mod sql_pg;
mod subst;
mod vault;
mod worker;
mod worker_map;

pub use sql::{DsnError, ParsedDsn, SqlConn, SqlOpError, parse_dsn};

use crate::config::{self, Config};
use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame};
use crate::evidence::Store as EvidenceStore;
use crate::exchange::WorkerHealth;
use crate::placeholder::{self, PlaceholderError};
use crate::scrub;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use errors::{capture_error, new_error, placeholder_substitute_error, runner_error, with_resource};

/// A boxed, `Send` future; the shape Rust needs for the small async seams
/// [`Handler`] takes as options.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Looks up an environment variable by name, for `${NAME}` placeholder
/// substitution. Injected so tests can supply a map instead of the real
/// process environment.
pub type Lookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Returns the current time. Injected so tests can control op timing and
/// evidence expiry without a real clock.
pub type NowFn = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Opens a [`SqlConn`] for one sql resource's connection DSN, already
/// substituted.
pub type OpenSql = Arc<
    dyn Fn(&str) -> BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>>
        + Send
        + Sync,
>;

const HTTP_INFLIGHT: i64 = 4;
const SQL_INFLIGHT: i64 = 2;
const BROWSER_INFLIGHT: i64 = 1;
const CLI_INFLIGHT: i64 = 2;

/// Errors opening a sql resource's connection.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SqlOpenError(pub String);

/// Errors constructing a [`Handler`].
#[derive(Debug, thiserror::Error)]
pub enum NewError {
    #[error("ops: load deny keys: {0}")]
    DenyKeys(#[source] contract::ContractError),
    #[error("ops: open resource {0}: {1}")]
    OpenSql(String, #[source] SqlOpenError),
    #[error("ops: build http client: {0}")]
    HttpClient(#[source] reqwest::Error),
}

/// The only arg keys each op kind accepts; any other key present in an op's
/// args is refused with reason "unknown-arg". `None` means the kind itself
/// is unknown.
fn closed_arg_keys(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "http.request" => Some(&[
            "method",
            "path",
            "headers",
            "query-params",
            "body",
            "redirect",
            "capture",
        ]),
        "sql.query" => Some(&["query", "params", "capture"]),
        "evidence.fetch" => Some(&["op-id"]),
        "browser.page" => Some(&["commands", "capture", "command-timeout-ms"]),
        "cli.exec" => Some(&[
            "mode", "command", "args", "handle", "until", "extract", "env", "stdout", "capture",
        ]),
        _ => None,
    }
}

/// External dependencies a [`Handler`] needs; anything left `None` gets a
/// default from [`Handler::new`].
#[derive(Default)]
pub struct Options {
    pub open_sql: Option<OpenSql>,
    pub lookup: Option<Lookup>,
    pub now: Option<NowFn>,
}

/// Executes ops against the resources declared in a runner config.
pub struct Handler {
    cfg: Config,
    http_client: reqwest::Client,
    sql_conns: HashMap<String, Arc<dyn SqlConn>>,
    lookup: Lookup,
    /// Values of every env name listed in any resource's `secrets`,
    /// resolved once at build; scrubbed from every op's result.
    listed_secrets: Vec<String>,
    now: NowFn,
    deny_keys: Vec<String>,
    evidence: EvidenceStore,
    jars: jar::Jars,
    vault: vault::Vault,
    handles: handles::Handles,
    contexts: contexts::Contexts,
    ended_runs: ended::EndedRuns,
    worker_up: AtomicBool,
    last_ping: Mutex<Option<Instant>>,
    /// The last good ping response of the worker; `None` while it is down.
    ping_info: Mutex<Option<Value>>,
}

impl Handler {
    /// Opens one sql connection per sql resource in `cfg` via
    /// `opts.open_sql` (mysql_async by default), loads the contract's
    /// deny-key list, and creates one evidence store.
    pub async fn new(cfg: Config, opts: Options) -> std::result::Result<Self, NewError> {
        let deny_keys = contract::deny_keys().map_err(NewError::DenyKeys)?;

        let open_sql = opts.open_sql.unwrap_or_else(sql::default_open_sql);
        let mut sql_conns: HashMap<String, Arc<dyn SqlConn>> = HashMap::new();
        for (id, r) in &cfg.resources {
            if r.r#type != "sql" {
                continue;
            }
            match open_sql(&r.connection).await {
                Ok(conn) => {
                    sql_conns.insert(id.clone(), conn);
                }
                Err(e) => {
                    // If a later sql
                    // resource fails to open, every resource already opened
                    // is closed before the error is returned, so a failed
                    // `Handler::new` never leaks a live pool.
                    close_all(&sql_conns).await;
                    return Err(NewError::OpenSql(id.clone(), e));
                }
            }
        }

        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(NewError::HttpClient)?;

        let worker_up = announce::startup(&cfg).await;
        let lookup = opts.lookup.unwrap_or_else(default_lookup);
        let listed_secrets = subst::listed_secret_values(&cfg, &lookup);

        Ok(Self {
            worker_up: AtomicBool::new(worker_up),
            last_ping: Mutex::new(Some(Instant::now())),
            ping_info: Mutex::new(None),
            cfg,
            http_client,
            sql_conns,
            lookup,
            listed_secrets,
            now: opts.now.unwrap_or_else(default_now),
            deny_keys,
            evidence: EvidenceStore::new(),
            jars: jar::Jars::default(),
            vault: vault::Vault::default(),
            handles: handles::Handles::default(),
            contexts: contexts::Contexts::default(),
            ended_runs: ended::EndedRuns::default(),
        })
    }

    /// Closes every sql resource's connection. Takes `&self`
    /// since a
    /// `mysql_async::Pool` is itself a cheap, cloneable handle onto shared
    /// state: disconnecting a clone marks the whole pool closed for every
    /// other clone too, so no exclusive ownership is needed to shut it
    /// down. Safe to call more than once and safe to call while ops are
    /// still in flight: like `sql.DB.Close`, it waits out any connections
    /// currently checked out rather than yanking them away.
    pub async fn close(&self) {
        for (id, conn) in &self.sql_conns {
            let ok = conn.close().await;
            let status = if ok { "pass" } else { "error" };
            tracing::info!(target: "runner.db", resource = %id, status = status, "pool closed");
        }
    }

    /// The pool size the exchange loop should use: 4 per http resource plus
    /// 2 per sql resource.
    pub fn max_inflight(&self) -> i64 {
        self.cfg
            .resources
            .values()
            .map(|r| match r.r#type.as_str() {
                "http" => HTTP_INFLIGHT,
                "sql" => SQL_INFLIGHT,
                "browser" => BROWSER_INFLIGHT,
                "cli" => CLI_INFLIGHT,
                _ => 0,
            })
            .sum()
    }

    /// Runs `op` against its resource and returns exactly one of a `Result`
    /// or an `Error`. Applies `op.timeout_ms` as a deadline around the
    /// resource call: on elapse the in-flight future is dropped (cancelling
    /// any outstanding HTTP or sql work) and a "timeout" error is returned.
    pub async fn handle(&self, op: contract::Op) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        let start = (self.now)();
        if self.ended_runs.contains(&op.run_id) {
            let e = new_error(
                &op.op_id,
                "context-lost",
                json!({"resource": op.resource, "why": "run-closed"}),
            );
            log::handled(&op, (self.now)(), start, &None, &Some(e.clone()));
            return (None, Some(e));
        }
        let millis = clamp_timeout_ms(op.timeout_ms);
        let (result, err) =
            match tokio::time::timeout(Duration::from_millis(millis), self.dispatch(&op)).await {
                Ok(pair) => pair,
                Err(_) => (
                    None,
                    Some(new_error(
                        &op.op_id,
                        "timeout",
                        json!({"timeout-ms": millis}),
                    )),
                ),
            };
        // The run ended while this op ran: nothing it stored stays.
        if self.ended_runs.contains(&op.run_id) {
            self.free_run(&op.run_id);
        }
        let err = err.map(|e| with_resource(e, &op.resource));
        log::handled(&op, (self.now)(), start, &result, &err);
        (result, err)
    }

    async fn dispatch(&self, op: &contract::Op) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        let Some(allowed) = closed_arg_keys(&op.kind) else {
            return (
                None,
                Some(new_error(
                    &op.op_id,
                    "unknown-kind",
                    json!({"kind": op.kind}),
                )),
            );
        };
        if let Some(e) = check_closed_keys(&op.op_id, &op.args, allowed) {
            return (None, Some(e));
        }

        match op.kind.as_str() {
            "http.request" => self.http_request(op).await,
            "sql.query" => self.sql_query(op).await,
            "browser.page" => self.browser_page(op).await,
            "cli.exec" => self.cli_exec(op).await,
            _ => self.evidence_fetch(op),
        }
    }

    fn resource(
        &self,
        op_id: &str,
        name: &str,
        want_type: &str,
    ) -> std::result::Result<config::Resource, ErrorFrame> {
        match self.cfg.resources.get(name) {
            Some(r) if r.r#type == want_type => Ok(r.clone()),
            _ => Err(new_error(
                op_id,
                "unknown-resource",
                json!({"resource": name}),
            )),
        }
    }
}

fn check_closed_keys(
    op_id: &str,
    args: &HashMap<String, Value>,
    allowed: &[&str],
) -> Option<ErrorFrame> {
    for key in args.keys() {
        if !allowed.contains(&key.as_str()) {
            return Some(new_error(op_id, "unknown-arg", json!({})));
        }
    }
    None
}

/// Renders `v` as a string for use in headers and query params, tolerating
/// non-string JSON values rather than dropping them.
fn string_value(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

/// The longest an op may run, and the deadline of an op that asks for none.
const MAX_OP_TIMEOUT_MS: i64 = 600_000;

/// An op always has a deadline: a value `<= 0` or above the max becomes the max.
fn clamp_timeout_ms(timeout_ms: i64) -> u64 {
    let ms = if timeout_ms <= 0 || timeout_ms > MAX_OP_TIMEOUT_MS {
        MAX_OP_TIMEOUT_MS
    } else {
        timeout_ms
    };
    u64::try_from(ms).unwrap_or(0)
}

/// Scrubs every string value and object key of `payload` with
/// [`scrub::scrub_value`]. It walks the tree, so secrets that JSON would
/// escape are still found and the structure is never corrupted. It can no
/// longer fail; the `Result` type stays so callers compile unchanged.
fn scrub_payload(
    payload: &Map<String, Value>,
    secrets: &[String],
) -> std::result::Result<(std::collections::BTreeMap<String, Value>, i64), serde_json::Error> {
    let (value, count) = scrub::scrub_value(&Value::Object(payload.clone()), secrets);
    let out = match value {
        Value::Object(m) => m.into_iter().collect(),
        _ => std::collections::BTreeMap::new(),
    };
    Ok((out, count as i64))
}

fn duration_ms(now: SystemTime, start: SystemTime) -> i64 {
    crate::logfmt::millis(now.duration_since(start).unwrap_or_default())
}

/// Closes every already-opened sql connection, used when a later resource
/// fails to open partway through [`Handler::new`]'s loop.
async fn close_all(conns: &HashMap<String, Arc<dyn SqlConn>>) {
    for conn in conns.values() {
        conn.close().await;
    }
}

fn default_lookup() -> Lookup {
    Arc::new(|name: &str| std::env::var(name).ok())
}

fn default_now() -> NowFn {
    Arc::new(SystemTime::now)
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ended_tests.rs"]
mod ended_tests;

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
