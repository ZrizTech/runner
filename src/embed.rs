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
    /// Resources and cloud URL. `cloud.key_env` is not read here.
    pub runner: config::Config,
    /// The runner key, sent as the Bearer value on each exchange.
    pub key: String,
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
        self.task.await.unwrap_or(Ok(()))
    }

    /// True when the loop has ended by itself (the cloud refused the key).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

/// Starts the exchange loop in a task. `on_connected` is called once, after
/// the first successful exchange.
pub async fn start(
    config: Config,
    on_connected: impl Fn() + Send + Sync + 'static,
) -> Result<Handle, ops::NewError> {
    let handler =
        Arc::new(ops::Handler::new(config.runner.clone(), ops::Options::default()).await?);
    let mut ex = exchange_config(&config.runner, config.key, handler.clone());
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

/// The exchange config for a runner config and a key. Used by the binary
/// and by [`start`], so both run the same loop.
pub fn exchange_config(
    cfg: &config::Config,
    key: String,
    handler: Arc<ops::Handler>,
) -> exchange::Config {
    let mut ex = exchange::Config::new(cfg.cloud.url.clone(), key, hostname(), handler);
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
