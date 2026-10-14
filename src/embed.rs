//! Runs the runner inside another process (for example `zz run`). The
//! caller builds the [`config::Config`] in memory and passes the key as a
//! value. There is no process variable and no worker sidecar. The runner
//! opens no port: it only makes outbound exchange requests.

use crate::{config, exchange, ops};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// What [`start`] needs.
pub struct Config {
    /// Resources and cloud URL, checked by [`config::check`]. `cloud.key_env`
    /// is not read here.
    pub runner: config::Config,
    /// The runner key, sent as the Bearer value on each exchange.
    pub key: String,
    /// Reads a secret value by name. `None` reads the process environment.
    pub lookup: Option<ops::Lookup>,
    /// The runner id in the exchange. `None` uses [`hostname`].
    pub runner_id: Option<String>,
}

/// A running embedded runner.
pub struct Handle {
    cancel: CancellationToken,
    task: JoinHandle<Result<(), exchange::ExchangeError>>,
}

impl Handle {
    /// Stops the exchange loop, closes the op handler, and waits for the
    /// task to end. Returns `Err` if the cloud had already refused the key.
    pub async fn stop(self) -> Result<(), exchange::ExchangeError> {
        self.cancel.cancel();
        match self.task.await {
            Ok(result) => result,
            Err(_) => Err(exchange::ExchangeError::Panicked),
        }
    }

    /// True when the loop has ended by itself (the cloud refused the key).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

/// Why [`start`] refused.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Handler(#[from] ops::NewError),
}

/// Starts the exchange loop in a task. `on_connected` is called once, after
/// the first successful exchange.
pub async fn start(
    config: Config,
    on_connected: impl Fn() + Send + Sync + 'static,
) -> Result<Handle, StartError> {
    let insecure = std::env::var("ZRIZ_RUNNER_INSECURE_CLOUD").is_ok_and(|v| v == "1");
    config::check(&config.runner, insecure)?;
    // The key is never substitutable: no lookup may return it as a value.
    let inner = config.lookup;
    let key_value = config.key.clone();
    let lookup: ops::Lookup = Arc::new(move |name: &str| {
        let v = match &inner {
            Some(f) => f(name),
            None => std::env::var(name).ok(),
        };
        v.filter(|v| *v != key_value)
    });
    let opts = ops::Options {
        lookup: Some(lookup),
        ..Default::default()
    };
    let handler = Arc::new(ops::Handler::new(config.runner.clone(), opts).await?);
    let mut ex = exchange_config(
        &config.runner,
        config.key,
        config.runner_id,
        handler.clone(),
    );
    ex.on_connected = Some(Arc::new(on_connected));
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let task = tokio::spawn(async move {
        let result = exchange::run(ex, token).await;
        handler.close().await;
        result
    });
    Ok(Handle { cancel, task })
}

/// The exchange config for a runner config, a key and an optional runner id. Used by the binary
/// and by [`start`], so both run the same loop.
pub fn exchange_config(
    cfg: &config::Config,
    key: String,
    runner_id: Option<String>,
    handler: Arc<ops::Handler>,
) -> exchange::Config {
    let runner_id = runner_id.unwrap_or_else(hostname);
    let mut ex = exchange::Config::new(cfg.cloud.url.clone(), key, runner_id, handler);
    let mut ids: Vec<String> = cfg.resources.keys().cloned().collect();
    ids.sort();
    ex.resources = ids;
    ex
}

/// The standard library has no hostname call. `$HOSTNAME`
/// is set automatically by Docker and Kubernetes (the runner's actual
/// deployment target) for every container; the `hostname` command covers
/// bare-metal dev/test machines that don't export it. Falls back to
/// `"runner"` on error.
pub fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    if let Ok(output) = std::process::Command::new("hostname").output()
        && output.status.success()
        && let Ok(s) = String::from_utf8(output.stdout)
    {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "runner".to_string()
}

#[cfg(test)]
#[path = "embed_tests.rs"]
mod embed_tests;
