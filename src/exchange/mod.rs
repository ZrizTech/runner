//! The long-poll loop that talks to the cloud: it posts the runner's state
//! and any completed op results, dispatches the op frames the cloud sends
//! back onto a bounded pool of tasks, and repeats forever until cancelled or
//! the cloud rejects the token.

mod frames;
mod token;

use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Once, RwLock};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// The runner's build id (`<semver>+<sha>`, `+dev` for a local build). Sent as
/// `runner.version` and in the `User-Agent`.
pub const VERSION: &str = env!("ZRIZ_BUILD");

/// The default `User-Agent` on exchange requests.
pub const USER_AGENT: &str = concat!("runner/", env!("ZRIZ_BUILD"));

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(40);
const DEFAULT_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(10);

/// A boxed, `Send` future, the shape a few of [`Config`]'s optional hooks
/// need since Rust has no dyn-safe `async fn` in traits yet.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Executes a decoded op and reports how many it can run at once. The
/// production implementation is [`crate::ops::Handler`]; tests implement
/// this directly with fakes that don't need a real resource.
pub trait Handler: Send + Sync {
    fn handle(&self, op: contract::Op) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)>;
    fn max_inflight(&self) -> i64;

    /// The op kinds to announce now. Defaults to the three built-in kinds.
    fn op_kinds(&self) -> Vec<String> {
        ["http.request", "sql.query", "evidence.fetch"]
            .map(String::from)
            .to_vec()
    }

    /// The resource ids to announce, given the configured ones.
    fn resources(&self, configured: &[String]) -> Vec<String> {
        configured.to_vec()
    }

    /// Called before each announcement so the handler can re-check
    /// anything that changes which kinds it offers.
    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

impl Handler for crate::ops::Handler {
    fn handle(&self, op: contract::Op) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
        Box::pin(async move { crate::ops::Handler::handle(self, op).await })
    }

    fn max_inflight(&self) -> i64 {
        crate::ops::Handler::max_inflight(self)
    }

    fn op_kinds(&self) -> Vec<String> {
        crate::ops::Handler::op_kinds(self)
    }

    fn resources(&self, configured: &[String]) -> Vec<String> {
        crate::ops::Handler::announced_resources(self, configured)
    }

    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move { crate::ops::Handler::refresh_worker(self).await })
    }
}

/// Waits out one backoff interval, returning `false` if `cancel` fired
/// first. Config's default is a real timer; tests inject a fake to assert
/// on backoff durations without waiting on them.
pub type SleepFn =
    Arc<dyn Fn(CancellationToken, Duration) -> BoxFuture<'static, bool> + Send + Sync>;

/// Called exactly once: after the loop's first successful exchange (HTTP
/// 200 or 204), the point at which the cloud has recorded this runner's
/// presence. Never called on a failed exchange, never called more than
/// once. Called synchronously from whichever task's exchange succeeds
/// first, so it should return quickly.
pub type OnConnected = Arc<dyn Fn() + Send + Sync>;

/// Re-reads the runner's token. Called when the main loop's exchange gets a
/// 401: `run` calls it once and, if it returns a token, waits out one
/// backoff interval before retrying with it.
pub type TokenSource = Arc<dyn Fn() -> std::result::Result<String, String> + Send + Sync>;

/// Configures [`run`]. `cloud_url`, `token`, `runner_id` and `handler` are
/// required; the rest default when left at [`Config::new`]'s values or, for
/// a struct built by hand with a zero `Duration`, when [`run`] applies its
/// defaults.
pub struct Config {
    pub cloud_url: String,
    pub token: String,
    pub runner_id: String,
    pub env: String,
    pub resources: Vec<String>,
    pub handler: Arc<dyn Handler>,
    /// Overrides the HTTP client `run` builds by default; tests use this to
    /// point at an in-process fake cloud with a short timeout.
    pub http_client: Option<reqwest::Client>,
    pub backoff: Duration,
    pub max_backoff: Duration,
    pub sleep: Option<SleepFn>,
    pub on_connected: Option<OnConnected>,
    pub token_source: Option<TokenSource>,
    /// The `User-Agent` sent on exchange requests; `None` means
    /// [`USER_AGENT`]. A caller that embeds this library may set its own value.
    pub user_agent: Option<String>,
}

impl Config {
    /// Builds a `Config` with sane defaults for everything but the
    /// required fields; callers set `resources`, `env`, `on_connected`,
    /// `token_source` etc. afterward.
    pub fn new(
        cloud_url: impl Into<String>,
        token: impl Into<String>,
        runner_id: impl Into<String>,
        handler: Arc<dyn Handler>,
    ) -> Self {
        Self {
            cloud_url: cloud_url.into(),
            token: token.into(),
            runner_id: runner_id.into(),
            env: String::new(),
            resources: Vec::new(),
            handler,
            http_client: None,
            backoff: DEFAULT_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            sleep: None,
            on_connected: None,
            token_source: None,
            user_agent: None,
        }
    }
}

/// Errors [`run`] returns. The only case: the cloud answered 401 twice in a
/// row (or once with no way to refresh the token), so the runner fails
/// closed instead of retrying forever.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone, Copy)]
pub enum ExchangeError {
    #[error("exchange: unauthorized")]
    Unauthorized,
}

/// What one call to [`Inner::exchange`] can end in, internally. Never
/// exposed: [`run`] turns it into backoff behavior or an [`ExchangeError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepError {
    /// The cloud answered 401.
    Unauthorized,
    /// A transport error, a non-2xx/401/204 status, or an undecodable body.
    Failed,
    /// `cancel` fired while the request was in flight.
    Cancelled,
}

fn apply_defaults(cfg: &mut Config) {
    if cfg.backoff.is_zero() {
        cfg.backoff = DEFAULT_BACKOFF;
    }
    if cfg.max_backoff.is_zero() {
        cfg.max_backoff = DEFAULT_MAX_BACKOFF;
    }
    if cfg.sleep.is_none() {
        cfg.sleep = Some(default_sleep());
    }
}

fn default_sleep() -> SleepFn {
    Arc::new(|cancel: CancellationToken, d: Duration| {
        Box::pin(async move {
            tokio::select! {
                () = tokio::time::sleep(d) => true,
                () = cancel.cancelled() => false,
            }
        })
    })
}

fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(DEFAULT_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Holds the state one [`run`] call carries across exchanges: the queue of
/// completed frames waiting to go out, the semaphore that bounds how many
/// ops run at once, and the backoff shared between the main poll and every
/// op-completion-triggered exchange.
struct Inner {
    cfg: Config,
    cancel: CancellationToken,
    endpoint: String,
    http_client: reqwest::Client,
    queue: Mutex<Vec<contract::Frame>>,
    semaphore: Arc<Semaphore>,
    /// The pool capacity the semaphore was built with (`handler.max_inflight()`
    /// clamped to `>= 0`), used to compute the in-flight count for requests.
    capacity: usize,
    /// The raw, unclamped value `handler.max_inflight()` reported, sent to
    /// the cloud as `runner.max-inflight` exactly as declared.
    declared_max_inflight: i64,
    backoff: Mutex<Duration>,
    token: RwLock<String>,
    connect_once: Once,
}

impl Inner {
    fn new(cfg: Config, cancel: CancellationToken) -> Self {
        let declared_max_inflight = cfg.handler.max_inflight();
        let capacity = declared_max_inflight.max(0) as usize;
        let http_client = cfg.http_client.clone().unwrap_or_else(default_http_client);
        let endpoint = format!("{}/runner/v1/exchange", cfg.cloud_url.trim_end_matches('/'));
        let token = cfg.token.clone();
        let backoff = cfg.backoff;
        Self {
            endpoint,
            http_client,
            queue: Mutex::new(Vec::new()),
            semaphore: Arc::new(Semaphore::new(capacity)),
            capacity,
            declared_max_inflight,
            backoff: Mutex::new(backoff),
            token: RwLock::new(token),
            connect_once: Once::new(),
            cfg,
            cancel,
        }
    }

    /// Fires `cfg.on_connected` the first time any exchange succeeds, and
    /// never again. Safe to call from several tasks at once.
    fn connected(&self) {
        self.connect_once.call_once(|| {
            let env = (!self.cfg.env.is_empty()).then_some(self.cfg.env.as_str());
            tracing::info!(target: "runner.exchange", runner = %self.cfg.runner_id, env = env, cloud = %self.cfg.cloud_url, "runner connected");
            if let Some(f) = &self.cfg.on_connected {
                f();
            }
        });
    }

    /// Resets the shared backoff on success, or sleeps out its current
    /// value and doubles it (capped at `cfg.max_backoff`) on failure.
    /// Returns `false` only when the sleep was cut short by cancellation.
    ///
    /// Two callers can race here — the main loop and an op-completion
    /// trigger — and each takes its own lock/sleep/lock round trip rather
    /// than holding the lock across the sleep, trading a little backoff
    /// precision for never blocking one caller on the other's sleep.
    async fn apply_backoff(&self, failed: bool) -> bool {
        if !failed {
            let mut b = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
            *b = self.cfg.backoff;
            return true;
        }

        let d = {
            let b = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
            *b
        };

        let Some(sleep) = &self.cfg.sleep else {
            return true;
        };
        if !sleep(self.cancel.clone(), d).await {
            return false;
        }

        let mut b = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
        *b = next_backoff(d, self.cfg.max_backoff);
        true
    }
}

fn next_backoff(current: Duration, limit: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > limit { limit } else { doubled }
}

/// Polls the cloud until `cancel` fires (returning `Ok(())`) or the cloud
/// fails closed on a 401 (returning [`ExchangeError::Unauthorized`]). Any
/// other failure is logged, its frames are requeued, and the loop retries
/// after an exponential backoff capped at `cfg.max_backoff`. A failure on
/// the extra exchange an op completion triggers drives the same backoff, so
/// a cloud outage slows every request path, not just this one.
///
/// A 401 gets one retry: `run` tries `cfg.token_source` and, if that yields
/// a fresh token, waits out one backoff interval and retries with it. A
/// second consecutive 401 — or no `token_source` to retry with — fails
/// closed with [`ExchangeError::Unauthorized`], exit code 2 at the bin
/// layer.
pub async fn run(
    mut cfg: Config,
    cancel: CancellationToken,
) -> std::result::Result<(), ExchangeError> {
    apply_defaults(&mut cfg);
    let inner = Arc::new(Inner::new(cfg, cancel.clone()));

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        let mut result = inner.exchange().await;
        if result == Err(StepError::Unauthorized) {
            match inner.retry_unauthorized().await {
                None => return Ok(()),
                Some(retry_result) => {
                    if retry_result == Err(StepError::Unauthorized) {
                        return Err(ExchangeError::Unauthorized);
                    }
                    result = retry_result;
                }
            }
        }

        if cancel.is_cancelled() {
            return Ok(());
        }
        if result == Err(StepError::Cancelled) {
            return Ok(());
        }

        if !inner.apply_backoff(result.is_err()).await {
            return Ok(());
        }
    }
}

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod conformance_tests;
#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;

#[cfg(test)]
#[path = "exchange_tests.rs"]
mod exchange_tests;
#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;

#[cfg(test)]
mod build_tests;
