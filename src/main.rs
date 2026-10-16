//! Command zriz-runner: runs next to a customer's systems, executing ops
//! the cloud sends over the exchange loop.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zriz_runner::{config, embed, exchange, ops};

/// Exit code: the config is bad. systemd (`RestartPreventExitStatus=`) does not restart.
const EXIT_CONFIG: i32 = 2;
/// Exit code: the cloud refused the key. Same: no restart loop.
const EXIT_REFUSED: i32 = 3;

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
    } else if code != EXIT_CONFIG && code != EXIT_REFUSED {
        // A config error or a refused key has already printed its one line.
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
        let _ = writeln!(stderr, "{}", config_unset_line());
        return EXIT_CONFIG;
    };

    let (cfg, key, code) = load_config(Path::new(&path), &lookup, stderr);
    if code != 0 {
        return code;
    }

    let handler = match ops::Handler::new(cfg.clone(), ops::Options::default()).await {
        Ok(h) => Arc::new(h),
        Err(e) => {
            let _ = writeln!(
                stderr,
                "config error: {e}; fix this field in the config file and start the runner again (exit {EXIT_CONFIG}: it is not restarted)"
            );
            return EXIT_CONFIG;
        }
    };

    // Once a handler
    // exists, close it on every exit path out of `run` (success or
    // unauthorized) after the exchange loop has stopped, before the
    // process exits. The earlier returns above never reach this point
    // because no handler (and so no sql connection) exists yet to close.
    let closer = Arc::clone(&handler);
    let code = run_exchange(cancel, cfg, key, lookup, handler).await;
    closer.close().await;
    code
}

/// Reads the runner config and its cloud key, returning a non-zero exit
/// code (and an already-printed message) on any failure.
fn load_config(
    path: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
    stderr: &mut impl Write,
) -> (config::Config, String, i32) {
    let cfg = match config::load(path, lookup) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(stderr, "{}", config_error_line(path, &e.to_string()));
            return (config::Config::default(), String::new(), EXIT_CONFIG);
        }
    };

    match lookup(&cfg.cloud.key_env) {
        Some(key) => (cfg, key, 0),
        None => {
            let _ = writeln!(stderr, "{}", key_unset_line(&cfg.cloud.key_env));
            (config::Config::default(), String::new(), EXIT_CONFIG)
        }
    }
}

async fn run_exchange(
    cancel: CancellationToken,
    cfg: config::Config,
    key: String,
    lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    handler: Arc<ops::Handler>,
) -> i32 {
    let key_env = cfg.cloud.key_env.clone();

    let mut exchange_cfg = embed::exchange_config(&cfg, key, None, handler);
    // Re-reads the key env var named by the config on every call, so a
    // rotated key takes effect on the exchange loop's next 401 retry
    // without a process restart.
    exchange_cfg.key_source = Some(Arc::new(move || {
        lookup(&key_env).ok_or_else(|| format!("environment variable {key_env} is not set"))
    }));

    let env = exchange_cfg.env.clone();
    tracing::info!(target: "runner.main", runner = %exchange_cfg.runner_id, cloud = %exchange_cfg.cloud_url, build = zriz_runner::exchange::VERSION, "started");
    match exchange::run(exchange_cfg, cancel.clone()).await {
        Ok(()) => 0,
        Err(
            e @ (exchange::ExchangeError::Unauthorized(_) | exchange::ExchangeError::Forbidden(_)),
        ) => refused(&e, &env),
        Err(exchange::ExchangeError::Panicked) => EXIT_CONFIG,
    }
}

/// The cloud refused the key (401) or the key has no right (403). Writes one
/// error line (cause and fix; never the key), then returns `EXIT_REFUSED`. The
/// units do not restart on that code (systemd `RestartPreventExitStatus=2 3`,
/// Docker `restart: on-failure:3`), and the cloud sees no more bad requests.
fn refused(e: &exchange::ExchangeError, env: &str) -> i32 {
    tracing::error!(target: "runner.main", error = %refused_line(e, env), "refused");
    EXIT_REFUSED
}

fn refused_line(e: &exchange::ExchangeError, env: &str) -> String {
    let (status, code) = match e {
        exchange::ExchangeError::Forbidden(c) => (403, *c),
        exchange::ExchangeError::Unauthorized(c) => (401, *c),
        exchange::ExchangeError::Panicked => (0, ""),
    };
    let cause = match code {
        "key-revoked" => "the key was revoked",
        "key-unknown" => "the cloud does not know the key",
        "key-expired" => "the key expired",
        "key-missing" => "the runner sent no key",
        "no-right" => "the key has no right to run ops (it is not a runner key)",
        _ if status == 403 => "the key has no right to run ops (it is not a runner key)",
        _ => "the key is wrong or was revoked",
    };
    let env = if env.is_empty() { "<env>" } else { env };
    format!(
        "the cloud refused the runner key ({status}): {cause}; make a new key with `zz runners init --env {env}` (delete the old runner.env first), then restart the runner (systemctl restart zriz-runner; Docker: docker compose up -d --force-recreate runner); exit {EXIT_REFUSED}, not restarted by itself"
    )
}

fn config_unset_line() -> String {
    format!(
        "config error: ZRIZ_RUNNER_CONFIG is not set; set it to the path of config.json (systemd: Environment= in the unit; Docker: environment: in docker-compose.yml) and start the runner again (exit {EXIT_CONFIG}: it is not restarted)"
    )
}

fn config_error_line(path: &Path, why: &str) -> String {
    format!(
        "config error in {}: {why}; fix this field and start the runner again (exit {EXIT_CONFIG}: it is not restarted)",
        path.display()
    )
}

fn key_unset_line(key_env: &str) -> String {
    format!(
        "config error: {key_env} (cloud.key-env in the config) is not set; put it in /etc/zriz-runner/runner.env (Docker: the .env file next to docker-compose.yml) and start the runner again (exit {EXIT_CONFIG}: it is not restarted); a new key: `zz runners init --env <env>`"
    )
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod main_tests;
