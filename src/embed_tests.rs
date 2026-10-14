//! Tests for `embed::start` and `Handle::stop`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use wiremock::matchers::{body_string_contains, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config_for(url: &str) -> Config {
    let mut runner = config::Config::default();
    runner.cloud.url = url.to_string();
    Config {
        runner,
        key: "zrk_embedded".to_string(),
        lookup: None,
        runner_id: None,
    }
}

#[tokio::test]
async fn start_connects_with_the_key_value_and_stop_ends_the_task() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer zrk_embedded"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = start(config_for(&server.uri()), move || {
        let _ = tx.send(());
    })
    .await
    .expect("start");

    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("on_connected in time")
        .expect("on_connected fired");
    assert!(!handle.is_finished());

    let stopped = tokio::time::timeout(Duration::from_secs(10), handle.stop())
        .await
        .expect("stop returns in time");
    assert_eq!(stopped, Ok(()));
    // Called once only.
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn a_refused_key_ends_the_task_and_stop_reports_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let handle = start(config_for(&server.uri()), || {})
        .await
        .expect("start");
    let stopped = tokio::time::timeout(Duration::from_secs(10), async {
        while !handle.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.stop().await
    })
    .await
    .expect("task ends");
    assert_eq!(stopped, Err(exchange::ExchangeError::Unauthorized));
}

#[tokio::test]
async fn a_403_ends_the_task_with_forbidden() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let handle = start(config_for(&server.uri()), || {})
        .await
        .expect("start");
    let stopped = tokio::time::timeout(Duration::from_secs(10), async {
        while !handle.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.stop().await
    })
    .await
    .expect("task ends");
    assert_eq!(stopped, Err(exchange::ExchangeError::Forbidden));
}

#[tokio::test]
async fn dropping_the_handle_cancels_the_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let handle = start(config_for(&server.uri()), || {})
        .await
        .expect("start");
    let before = server.received_requests().await.unwrap_or_default().len();
    drop(handle);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let a = server.received_requests().await.unwrap_or_default().len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let b = server.received_requests().await.unwrap_or_default().len();
    assert!(a >= before);
    assert_eq!(a, b, "requests continue after drop");
}

#[tokio::test]
async fn the_given_runner_id_is_in_the_exchange_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("zz-run-42"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let mut cfg = config_for(&server.uri());
    cfg.runner_id = Some("zz-run-42".to_string());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = start(cfg, move || {
        let _ = tx.send(());
    })
    .await
    .expect("start");
    // on_connected fires only after a 2xx, which only the matching mock gives.
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("connected with the given runner id")
        .expect("fired");
    assert_eq!(handle.stop().await, Ok(()));
}

#[tokio::test]
async fn secrets_are_read_through_the_given_lookup() {
    let server = MockServer::start().await;
    let mut cfg = config_for(&server.uri());
    cfg.runner.resources.insert(
        "api".to_string(),
        config::Resource {
            r#type: "http".to_string(),
            base_url: "https://api.example.com".to_string(),
            secrets: Some(vec!["EMBED_TEST_SECRET".to_string()]),
            ..Default::default()
        },
    );
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    cfg.lookup = Some(Arc::new(move |name: &str| {
        seen.lock().unwrap().push(name.to_string());
        Some("s3cret".to_string())
    }));
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let handle = start(cfg, || {}).await.expect("start");
    assert_eq!(handle.stop().await, Ok(()));
    assert!(
        asked
            .lock()
            .unwrap()
            .iter()
            .any(|n| n == "EMBED_TEST_SECRET")
    );
}

#[tokio::test]
async fn start_refuses_a_config_that_load_would_refuse() {
    let mut bad_url = config_for("http://cloud.example.com");
    assert!(matches!(
        start(bad_url, || {}).await,
        Err(StartError::Config(config::ConfigError::InsecureCloudUrl))
    ));
    bad_url = config_for("https://cloud.example.com");
    bad_url.runner.resources.insert(
        "api".to_string(),
        config::Resource {
            r#type: "http".to_string(),
            base_url: "${API_URL}".to_string(),
            ..Default::default()
        },
    );
    assert!(matches!(
        start(bad_url, || {}).await,
        Err(StartError::Config(config::ConfigError::BadBaseUrl(..)))
    ));
}

#[test]
fn the_lookup_never_returns_the_key_value() {
    let inner: ops::Lookup = Arc::new(|name: &str| match name {
        "X" => Some("zrk_embedded".to_string()),
        "Y" => Some("other".to_string()),
        _ => None,
    });
    let lookup = guarded_lookup(Some(inner), "zrk_embedded".to_string());
    assert_eq!(lookup("X"), None);
    assert_eq!(lookup("Y"), Some("other".to_string()));
    assert_eq!(lookup("Z"), None);
}

#[tokio::test]
async fn stop_reports_a_panicked_task() {
    let handle = Handle {
        cancel: super::CancelOnDrop(CancellationToken::new()),
        task: tokio::spawn(async { panic!("boom") }),
    };
    assert_eq!(handle.stop().await, Err(exchange::ExchangeError::Panicked));
}
