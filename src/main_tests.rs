//! Tests for the binary's `run`: every exit path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn lookup_map(env: HashMap<&'static str, String>) -> impl Fn(&str) -> Option<String> {
    move |name: &str| env.get(name).cloned()
}

fn write_config(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = dir.path().join("runner-config.json");
    std::fs::write(&path, body).expect("write config");
    path
}

fn config_body(cloud_url: &str) -> String {
    format!(
        r#"{{"cloud":{{"url":"{cloud_url}","key-env":"ZRIZ_KEY"}},"evidence":"redacted","resources":{{}}}}"#
    )
}

#[tokio::test]
async fn run_config_env_unset() {
    let mut err = Vec::new();
    let code = run(CancellationToken::new(), |_: &str| None, &mut err).await;
    assert_eq!(code, 2);
    let text = String::from_utf8(err).unwrap();
    assert!(
        text.contains("ZRIZ_RUNNER_CONFIG is not set") && text.contains("set it to the path"),
        "{text}"
    );
}

#[tokio::test]
async fn run_config_file_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut env = HashMap::new();
    let missing = dir.path().join("missing.json");
    env.insert("ZRIZ_RUNNER_CONFIG", missing.to_string_lossy().to_string());

    let mut err = Vec::new();
    let code = run(CancellationToken::new(), lookup_map(env), &mut err).await;
    assert_eq!(code, 2);
    let text = String::from_utf8(err).unwrap();
    assert!(
        text.contains("missing.json") && text.contains("fix this field"),
        "{text}"
    );
}

#[tokio::test]
async fn run_key_env_unset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, &config_body("https://cloud.example"));
    let path_str = path.to_string_lossy().to_string();
    let mut env = HashMap::new();
    env.insert("ZRIZ_RUNNER_CONFIG", path_str);
    // ZRIZ_KEY, named by the config's key-env, is deliberately absent.

    let mut err = Vec::new();
    let code = run(CancellationToken::new(), lookup_map(env), &mut err).await;
    assert_eq!(code, 2);
    let text = String::from_utf8(err).unwrap();
    assert!(
        text.contains("ZRIZ_KEY")
            && text.contains("runner.env")
            && text.contains("zz runners init"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 1, "one line");
}

#[tokio::test]
async fn run_unauthorized_exits_with_the_refused_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, &config_body(&server.uri()));
    let mut env = HashMap::new();
    env.insert("ZRIZ_RUNNER_CONFIG", path.to_string_lossy().to_string());
    env.insert("ZRIZ_KEY", "test-token".to_string());
    let lookup = lookup_map(env);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let mut done = tokio::spawn(async move { run(cancel2, lookup, &mut Vec::new()).await });

    // The loop ends after the retry (2 requests). Then no more requests.
    wait_for_requests(&server, 2).await;
    let pause = std::time::Duration::from_millis(300);
    tokio::time::sleep(pause).await;
    let seen = server.received_requests().await.map_or(0, |r| r.len());
    tokio::time::sleep(pause).await;
    assert_eq!(
        server.received_requests().await.map_or(0, |r| r.len()),
        seen
    );
    let code = tokio::time::timeout(std::time::Duration::from_secs(2), &mut done)
        .await
        .expect("a refused key must end the runner")
        .expect("join");
    assert_eq!(code, EXIT_REFUSED, "a refused key exits 3, not restarted");
    drop(cancel);
}

/// A `lookup` backed by a map a test can mutate while `run` is executing,
/// simulating the key env var rotating between the runner's first 401
/// and its retry.
#[derive(Clone)]
struct MutableEnv {
    values: Arc<Mutex<HashMap<String, String>>>,
}

impl MutableEnv {
    fn new() -> Self {
        Self {
            values: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn set(&self, name: &str, value: &str) {
        self.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), value.to_string());
    }

    fn lookup(&self) -> impl Fn(&str) -> Option<String> + Send + Sync + 'static {
        let values = Arc::clone(&self.values);
        move |name: &str| {
            values
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
                .cloned()
        }
    }
}

/// Waits (up to 30 s) until the mock server has seen `n` requests.
async fn wait_for_requests(server: &MockServer, n: usize) {
    let seen = async {
        loop {
            if server.received_requests().await.map_or(0, |r| r.len()) >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), seen)
        .await
        .expect("the server did not see the requests in time");
}

#[tokio::test]
async fn run_recovers_from_unauthorized_after_token_rotates() {
    let env = MutableEnv::new();
    let env_for_mock = env.clone();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(move |req: &wiremock::Request| {
            let auth = req
                .headers
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if auth == "Bearer old-token" {
                env_for_mock.set("ZRIZ_KEY", "new-token");
                ResponseTemplate::new(401)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_raw(r#"{"v":1,"frames":[],"ended-runs":[]}"#, "application/json")
            }
        })
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, &config_body(&server.uri()));
    env.set("ZRIZ_RUNNER_CONFIG", &path.to_string_lossy());
    env.set("ZRIZ_KEY", "old-token");

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn({
        let lookup = env.lookup();
        async move { run(cancel2, lookup, &mut std::io::sink()).await }
    });

    // Wait until the retried (clean) exchange has landed, then request
    // shutdown.
    wait_for_requests(&server, 2).await;
    cancel.cancel();

    let code = tokio::time::timeout(std::time::Duration::from_secs(2), done)
        .await
        .expect("run did not return in time")
        .expect("join");
    assert_eq!(code, 0, "want 0 (recovered after token rotation)");
}

#[tokio::test]
async fn run_clean_shutdown_exits_zero() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_raw(r#"{"v":1,"frames":[],"ended-runs":[]}"#, "application/json"),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, &config_body(&server.uri()));
    let path_str = path.to_string_lossy().to_string();
    let mut env = HashMap::new();
    env.insert("ZRIZ_RUNNER_CONFIG", path_str);
    env.insert("ZRIZ_KEY", "test-token".to_string());
    let lookup = lookup_map(env);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cancel2, lookup, &mut std::io::sink()).await });

    wait_for_requests(&server, 1).await;
    cancel.cancel();

    let code = tokio::time::timeout(std::time::Duration::from_secs(2), done)
        .await
        .expect("run did not return in time")
        .expect("join");
    assert_eq!(code, 0);
}

#[test]
fn the_refused_line_names_cause_and_fix_and_never_a_key() {
    use exchange::ExchangeError::{Forbidden, Unauthorized};
    for (e, cause) in [
        (Unauthorized("key-revoked"), "the key was revoked"),
        (Unauthorized("key-unknown"), "does not know the key"),
        (Unauthorized("key-expired"), "expired"),
        (Unauthorized(""), "wrong or was revoked"),
        (Forbidden("no-right"), "no right"),
        (Forbidden(""), "no right"),
    ] {
        let line = refused_line(&e, "uat", false);
        assert!(line.contains(cause), "{line}");
        assert!(line.contains("zz runners init --env uat"), "{line}");
        assert!(line.contains("systemctl restart zriz-runner"), "{line}");
        assert!(line.contains("exit 3"), "{line}");
        assert!(!line.contains('\n'), "{line}");
    }
    assert!(refused_line(&Unauthorized(""), "", false).contains("--env <env>"));
}

#[test]
fn the_wait_line_says_idle_and_never_a_key() {
    let line = refused_line(
        &exchange::ExchangeError::Unauthorized("key-revoked"),
        "uat",
        true,
    );
    assert!(line.contains("waits idle"), "{line}");
    assert!(line.contains("zz runners init --env uat"), "{line}");
    assert!(!line.contains("exit 3"), "{line}");
}

#[tokio::test]
async fn on_refused_wait_stays_up_after_a_config_error_until_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(&dir, "{ not json");
    let env = std::collections::HashMap::from([
        ("ZRIZ_RUNNER_CONFIG", path.display().to_string()),
        ("ZRIZ_RUNNER_ON_REFUSED", "wait".to_string()),
    ]);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let mut err = Vec::new();
    let task = tokio::spawn(async move { run(c2, lookup_map(env), &mut err).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "the runner must wait, not exit");
    cancel.cancel();
    let code = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("stops on cancel")
        .expect("join");
    assert_eq!(code, 0);
}
