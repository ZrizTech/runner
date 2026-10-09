use crate::config::{Cloud, Config, Resource};
use crate::ops::tests::test_op;
use crate::ops::{Handler, NowFn, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOK: &str = "tok-value-77aa";

async fn handler(server: &MockServer) -> (Handler, Arc<AtomicU64>) {
    let mut resources = HashMap::new();
    resources.insert(
        "web".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{}/", server.uri()),
            ..Default::default()
        },
    );
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let c = clock.clone();
    let now: NowFn = Arc::new(move || UNIX_EPOCH + Duration::from_secs(c.load(Ordering::SeqCst)));
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let opts = Options {
        now: Some(now),
        lookup: Some(Arc::new(|_| None)),
        ..Default::default()
    };
    (Handler::new(cfg, opts).await.expect("handler"), clock)
}

async fn run_op(
    h: &Handler,
    run: &str,
    args: Value,
    project: &[&[&str]],
) -> (
    Option<crate::contract::Result>,
    Option<crate::contract::Error>,
) {
    let args: HashMap<String, Value> = serde_json::from_value(args).expect("args");
    let project = project
        .iter()
        .map(|p| p.iter().map(|s| s.to_string()).collect())
        .collect();
    h.handle(test_op(
        "op",
        run,
        "http.request",
        "web",
        2000,
        args,
        project,
    ))
    .await
}

async fn mount(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-key", "key-header-55")
                .set_body_json(json!({"token": TOK, "user": "bob"})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/echo"))
        .and(header("authorization", format!("Bearer {TOK}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"seen": TOK})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn capture_masks_stores_and_scrubs_later() {
    let server = MockServer::start().await;
    mount(&server).await;
    let (h, _) = handler(&server).await;

    let (r, e) = run_op(
        &h,
        "run-a",
        json!({"path": "/login", "capture": {"TOKEN": ["body", "token"]}}),
        &[&["body", "user"]],
    )
    .await;
    assert!(e.is_none(), "{e:?}");
    let r = r.expect("result");
    let text = serde_json::to_string(&r.payload).expect("json");
    assert!(!text.contains(TOK), "{text}");
    assert_eq!(r.payload["body"]["token"], json!("[captured]"));
    assert_eq!(r.payload["body"]["user"], json!("bob"));

    let (r, e) = run_op(
        &h,
        "run-a",
        json!({"path": "/echo", "headers": {"Authorization": "Bearer ${TOKEN}"}}),
        &[&["body", "seen"]],
    )
    .await;
    assert!(e.is_none(), "{e:?}");
    let r = r.expect("result");
    assert_eq!(r.payload["body"]["seen"], json!("[scrubbed]"));
    assert!(r.scrubbed > 0);
}

#[tokio::test]
async fn other_run_cannot_resolve() {
    let server = MockServer::start().await;
    mount(&server).await;
    let (h, _) = handler(&server).await;
    run_op(
        &h,
        "run-a",
        json!({"path": "/login", "capture": {"TOKEN": ["body", "token"]}}),
        &[],
    )
    .await;
    let (r, e) = run_op(
        &h,
        "run-b",
        json!({"path": "/echo", "headers": {"Authorization": "Bearer ${TOKEN}"}}),
        &[],
    )
    .await;
    assert!(r.is_none());
    let e = e.expect("error");
    assert_eq!(e.reason, "placeholder-in-disallowed-slot");
    assert!(e.message.contains("TOKEN"));
    assert!(!e.message.contains(TOK));
}

#[tokio::test]
async fn capture_from_header() {
    let server = MockServer::start().await;
    mount(&server).await;
    let (h, _) = handler(&server).await;
    let (r, e) = run_op(
        &h,
        "run-a",
        json!({"path": "/login", "capture": {"KEY": ["headers", "x-key"]}}),
        &[],
    )
    .await;
    assert!(e.is_none(), "{e:?}");
    assert_eq!(
        r.expect("r").payload["headers"]["x-key"],
        json!("[captured]")
    );
    assert_eq!(
        h.vault
            .get("run-a", "KEY", UNIX_EPOCH + Duration::from_secs(1_000_000)),
        Some("key-header-55".to_string())
    );
}

#[tokio::test]
async fn missing_path_fails_and_stores_nothing() {
    let server = MockServer::start().await;
    mount(&server).await;
    let (h, _) = handler(&server).await;
    let (r, e) = run_op(
        &h,
        "run-a",
        json!({"path": "/login", "capture": {"A": ["body", "token"], "B": ["body", "nope"]}}),
        &[],
    )
    .await;
    assert!(r.is_none());
    let e = e.expect("error");
    assert_eq!(e.reason, "runner-error");
    assert!(!e.message.contains(TOK));
    assert_eq!(h.vault.run_count(), 0);
}

#[tokio::test]
async fn idle_vault_is_kept() {
    let server = MockServer::start().await;
    mount(&server).await;
    let (h, clock) = handler(&server).await;
    run_op(
        &h,
        "run-a",
        json!({"path": "/login", "capture": {"TOKEN": ["body", "token"]}}),
        &[],
    )
    .await;
    assert_eq!(h.vault.run_count(), 1);
    clock.fetch_add(31 * 60, Ordering::SeqCst);
    let (r, e) = run_op(
        &h,
        "run-a",
        json!({"path": "/echo", "headers": {"Authorization": "Bearer ${TOKEN}"}}),
        &[],
    )
    .await;
    assert!(e.is_none(), "{e:?}");
    assert!(r.is_some());
    assert_eq!(h.vault.run_count(), 1);
}
