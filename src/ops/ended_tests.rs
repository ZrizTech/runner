//! An op of an ended run stores nothing, and a new op of it is refused.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::config::{Cloud, Config, Resource};
use crate::ops::tests::test_op;
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn handler(server: &MockServer) -> Arc<Handler> {
    let mut resources = HashMap::new();
    resources.insert(
        "web".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{}/", server.uri()),
            cookies: true,
            ..Default::default()
        },
    );
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let opts = Options {
        lookup: Some(Arc::new(|_| None)),
        ..Default::default()
    };
    Arc::new(Handler::new(cfg, opts).await.expect("handler"))
}

fn op(run: &str, args: Value) -> crate::contract::Op {
    let args: HashMap<String, Value> = serde_json::from_value(args).expect("args");
    test_op("op", run, "http.request", "web", 5000, args, vec![])
}

const NOTICE_TRACE: &str = "5d0c9a52-7c1e-4b1f-9a55-0d6f3a2b8c11";

#[tokio::test(flavor = "multi_thread")]
async fn a_capture_in_flight_when_the_notice_comes_leaves_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", "sid=abcdef; Path=/")
                .set_body_json(json!({"token": "tok-value-77aa"}))
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let h = handler(&server).await;
    let h2 = Arc::clone(&h);
    let running = tokio::spawn(async move {
        h2.handle(op(
            "run-z",
            json!({"path": "/login", "capture": {"TOKEN": ["body", "token"]}}),
        ))
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.run_ended("run-z", NOTICE_TRACE).await;
    let (r, e) = running.await.unwrap();
    assert!(e.is_none(), "{e:?}");
    assert!(r.is_some());
    assert_eq!(h.vault.run_count(), 0, "no capture kept");
    assert_eq!(h.jars.len(), 0, "no jar kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_op_of_an_ended_run_is_refused_and_the_target_is_not_called() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let h = handler(&server).await;
    h.run_ended("run-z", NOTICE_TRACE).await;
    let (r, e) = h.handle(op("run-z", json!({"path": "/x"}))).await;
    assert!(r.is_none());
    let e = e.expect("error");
    assert_eq!(e.reason, "context-lost");
    assert_eq!(e.details["why"], "run-closed");
    assert_eq!(e.details["resource"], "web");
    assert!(server.received_requests().await.unwrap().is_empty());
    // another run is not touched
    let (r, e) = h.handle(op("run-y", json!({"path": "/x"}))).await;
    assert!(r.is_some() && e.is_none());
}

#[tokio::test]
async fn the_set_of_ended_runs_keeps_1000_and_drops_the_oldest() {
    let s = super::ended::EndedRuns::default();
    for i in 0..1001 {
        s.mark(&format!("r{i}"));
    }
    assert!(!s.contains("r0"));
    assert!(s.contains("r1") && s.contains("r1000"));
}
