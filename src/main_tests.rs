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
        r#"{{"cloud":{{"url":"{cloud_url}","token-env":"ZRIZ_TOKEN"}},"evidence":"redacted","resources":{{}}}}"#
    )
}

#[tokio::test]
async fn run_config_env_unset() {
    let code = run(CancellationToken::new(), |_: &str| None, &mut Vec::new()).await;
    assert_eq!(code, 2);
}

#[tokio::test]
async fn run_config_file_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut env = HashMap::new();
    let missing = dir.path().join("missing.json");
    env.insert("ZRIZ_RUNNER_CONFIG", missing.to_string_lossy().to_string());

    let code = run(CancellationToken::new(), lookup_map(env), &mut Vec::new()).await;
    assert_eq!(code, 2);
}

#[tokio::test]
async fn run_token_env_unset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, &config_body("https://cloud.example"));
    let path_str = path.to_string_lossy().to_string();
    let mut env = HashMap::new();
    env.insert("ZRIZ_RUNNER_CONFIG", path_str);
    // ZRIZ_TOKEN, named by the config's token-env, is deliberately absent.

    let code = run(CancellationToken::new(), lookup_map(env), &mut Vec::new()).await;
    assert_eq!(code, 2);
}

#[tokio::test]
async fn run_unauthorized_exits_two() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, &config_body(&server.uri()));
    let path_str = path.to_string_lossy().to_string();
    let mut env = HashMap::new();
    env.insert("ZRIZ_RUNNER_CONFIG", path_str);
    env.insert("ZRIZ_TOKEN", "test-token".to_string());

    let code = run(CancellationToken::new(), lookup_map(env), &mut Vec::new()).await;
    assert_eq!(code, 2);
}

/// A `lookup` backed by a map a test can mutate while `run` is executing,
/// simulating the token env var rotating between the runner's first 401
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
                env_for_mock.set("ZRIZ_TOKEN", "new-token");
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
    env.set("ZRIZ_TOKEN", "old-token");

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn({
        let lookup = env.lookup();
        async move { run(cancel2, lookup, &mut std::io::sink()).await }
    });

    // Give the retried (clean) exchange one round trip to land, then
    // request shutdown, from inside the retried request's handler
    // before it responds.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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
    env.insert("ZRIZ_TOKEN", "test-token".to_string());
    let lookup = lookup_map(env);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cancel2, lookup, &mut std::io::sink()).await });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();

    let code = tokio::time::timeout(std::time::Duration::from_secs(2), done)
        .await
        .expect("run did not return in time")
        .expect("join");
    assert_eq!(code, 0);
}
