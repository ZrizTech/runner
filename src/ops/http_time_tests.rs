//! Spec 8.1, rows H1-H3: the target time of http.request holds no connect.
//! These tests use real time. The fake target answers after D = 200 ms and
//! the slow connect is S = 1000 ms; a target time passes when 200 <= t < 700.

use crate::config::{Cloud, Config, Resource};
use crate::ops::tests::{test_lookup, test_op};
use crate::ops::{Handler, Options};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const D: Duration = Duration::from_millis(200);
const S: Duration = Duration::from_millis(1000);

async fn slow_handler(base_url: &str, slow: Option<Duration>) -> Handler {
    let mut resources = HashMap::new();
    resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{base_url}/api"),
            ..Default::default()
        },
    );
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let h = Handler::new(
        cfg,
        Options {
            lookup: Some(test_lookup(HashMap::new())),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");
    *h.clients.slow_connect.lock().expect("lock") = slow;
    h
}

async fn answering(delay: Duration, p: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_delay(delay))
        .mount(&server)
        .await;
    server
}

async fn get(h: &Handler, op_id: &str, p: &str) -> (i64, i64) {
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String(p.to_string()));
    let (result, err) = h
        .handle(test_op(
            op_id,
            "run-t",
            "http.request",
            "api",
            20_000,
            args,
            vec![],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let t = result.expect("result").timing;
    (t.target_ms.expect("target-ms"), t.exec_ms)
}

fn passes(t: i64) -> bool {
    (200..700).contains(&t)
}

#[tokio::test]
async fn h1_cold_then_warm_target_has_no_connect() {
    let server = answering(D, "/api/x").await;
    let h = slow_handler(&server.uri(), Some(S)).await;
    let (target, exec) = get(&h, "op-1", "/x").await;
    eprintln!("H1 cold: target-ms={target} exec-ms={exec}");
    assert!(passes(target), "cold target {target}");
    assert!(exec >= 1200, "cold exec {exec}");
    let (target, exec) = get(&h, "op-2", "/x").await;
    eprintln!("H1 warm: target-ms={target} exec-ms={exec}");
    assert!(passes(target), "warm target {target}");
}

#[tokio::test]
async fn h2_two_hops_each_with_a_connect_sum_to_two_waits() {
    // The runner follows same-origin redirects only (origin::check_origin),
    // so the second "origin" of the spec is the same origin on a new
    // connection: the first answer closes it, so hop 2 connects (slowly) again.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/redirect"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "/api/landed")
                .insert_header("connection", "close")
                .set_delay(D),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/landed"))
        .respond_with(ResponseTemplate::new(200).set_delay(D))
        .mount(&server)
        .await;
    let h = slow_handler(&server.uri(), Some(S)).await;
    let (target, exec) = get(&h, "op-1", "/redirect").await;
    eprintln!("H2: target-ms={target} exec-ms={exec}");
    assert!((400..900).contains(&target), "target {target}");
    assert!(exec >= 2400, "exec {exec}");
}

#[tokio::test]
async fn h3_two_ops_at_once_on_a_cold_resource() {
    let server = answering(D, "/api/x").await;
    let h = slow_handler(&server.uri(), Some(S)).await;
    let ((t1, e1), (t2, e2)) = tokio::join!(get(&h, "op-1", "/x"), get(&h, "op-2", "/x"));
    eprintln!("H3: a target-ms={t1} exec-ms={e1}; b target-ms={t2} exec-ms={e2}");
    assert!(passes(t1), "first target {t1}");
    assert!(passes(t2), "second target {t2}");
}

#[tokio::test]
async fn client_returns_to_the_idle_set_after_an_error_frame() {
    // A port that nothing listens on: bind, note the port, close.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let uri = format!("http://127.0.0.1:{port}");
    let h = slow_handler(&uri, None).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/x".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-e",
            "run-t",
            "http.request",
            "api",
            5000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none(), "{result:?} {err:?}");
    assert_eq!(err.expect("error").reason, "connection-error");
    assert_eq!(h.clients.idle_count("api"), 1);
}
