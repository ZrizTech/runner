//! Command zriz-runner: runs next to a customer's systems, executing ops
//! the cloud sends over the exchange loop.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zriz_runner::{config, exchange, ops};

#[tokio::main]
async fn main() {
    zriz_runner::logfmt::init(&|name| std::env::var(name).ok());

    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());

    let code = run(
        cancel,
        |name: &str| std::env::var(name).ok(),
        &mut std::io::stderr(),
    )
    .await;
    if code == 0 {
        tracing::info!(target: "runner.main", "stopped");
    } else {
        tracing::error!(target: "runner.main", error = %format!("exit code {code}"), "stopped");
    }
    std::process::exit(code);
}

/// Cancels `cancel` on SIGINT (ctrl-c) or, on unix, SIGTERM — the
/// clean-shutdown trigger.
fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(_) => {
                        let _ = ctrl_c.await;
                        cancel.cancel();
                        return;
                    }
                };
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        cancel.cancel();
    });
}

/// Wires the runner's config, op handler and exchange loop together. Its
/// inputs are injected — `cancel` so a test can force clean shutdown
/// without a real signal, `lookup` so a test can supply a fake environment
/// without process-global env races, and `stderr` for where diagnostics
/// go — so it never has to touch process globals itself.
async fn run(
    cancel: CancellationToken,
    lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    stderr: &mut impl Write,
) -> i32 {
    let Some(path) = lookup("ZRIZ_RUNNER_CONFIG").filter(|p| !p.is_empty()) else {
        let _ = writeln!(stderr, "ZRIZ_RUNNER_CONFIG is not set");
        return 2;
    };

    let (cfg, token, code) = load_config(Path::new(&path), &lookup, stderr);
    if code != 0 {
        return code;
    }

    let handler = match ops::Handler::new(cfg.clone(), ops::Options::default()).await {
        Ok(h) => Arc::new(h),
        Err(e) => {
            let _ = writeln!(stderr, "{e}");
            return 2;
        }
    };

    // Once a handler
    // exists, close it on every exit path out of `run` (success or
    // unauthorized) after the exchange loop has stopped, before the
    // process exits. The earlier returns above never reach this point
    // because no handler (and so no sql connection) exists yet to close.
    let closer = Arc::clone(&handler);
    let code = run_exchange(cancel, cfg, token, lookup, handler).await;
    closer.close().await;
    code
}

/// Reads the runner config and its cloud token, returning a non-zero exit
/// code (and an already-printed message) on any failure.
fn load_config(
    path: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
    stderr: &mut impl Write,
) -> (config::Config, String, i32) {
    let cfg = match config::load(path, lookup) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(stderr, "{e}");
            return (config::Config::default(), String::new(), 2);
        }
    };

    match lookup(&cfg.cloud.token_env) {
        Some(token) => (cfg, token, 0),
        None => {
            let _ = writeln!(
                stderr,
                "environment variable {} is not set",
                cfg.cloud.token_env
            );
            (config::Config::default(), String::new(), 2)
        }
    }
}

async fn run_exchange(
    cancel: CancellationToken,
    cfg: config::Config,
    token: String,
    lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    handler: Arc<ops::Handler>,
) -> i32 {
    let runner_id = hostname();
    let token_env = cfg.cloud.token_env.clone();

    let mut exchange_cfg = exchange::Config::new(cfg.cloud.url.clone(), token, runner_id, handler);
    exchange_cfg.resources = sorted_resource_ids(&cfg);
    // Re-reads the token env var named by the config on every call, so a
    // rotated token takes effect on the exchange loop's next 401 retry
    // without a process restart.
    exchange_cfg.token_source = Some(Arc::new(move || {
        lookup(&token_env).ok_or_else(|| format!("environment variable {token_env} is not set"))
    }));

    tracing::info!(target: "runner.main", runner = %exchange_cfg.runner_id, cloud = %exchange_cfg.cloud_url, build = zriz_runner::exchange::VERSION, "started");
    match exchange::run(exchange_cfg, cancel).await {
        Ok(()) => 0,
        Err(exchange::ExchangeError::Unauthorized) => 2,
    }
}

fn sorted_resource_ids(cfg: &config::Config) -> Vec<String> {
    let mut ids: Vec<String> = cfg.resources.keys().cloned().collect();
    ids.sort();
    ids
}

/// The standard library has no hostname call. `$HOSTNAME`
/// is set automatically by Docker and Kubernetes (the runner's actual
/// deployment target) for every container; the `hostname` command covers
/// bare-metal dev/test machines that don't export it. Falls back to
/// `"runner"` on error.
fn hostname() -> String {
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
#[path = "main_tests.rs"]
mod main_tests;
