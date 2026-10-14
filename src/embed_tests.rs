//! Tests for `embed::start` and `Handle::stop`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::time::Duration;
use tokio::sync::mpsc;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config_for(url: &str) -> Config {
    let mut runner = config::Config::default();
    runner.cloud.url = url.to_string();
    Config {
        runner,
        key: "zrk_embedded".to_string(),
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
