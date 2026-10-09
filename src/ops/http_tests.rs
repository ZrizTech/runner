use super::*;
use crate::config::{Cloud, Config};
use crate::contract::{Error as ErrorFrame, Result as ResultFrame};
use crate::ops::tests::{test_lookup, test_op};
use crate::ops::{Handler, Options};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn http_config(base_url: &str, evidence: &str) -> Config {
    let mut resources = HashMap::new();
    resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{base_url}/api"),
            connection: String::new(),
            read_only: false,
            cookies: false,
            secrets: Some(vec![
                "TOKEN".to_string(),
                "ZRIZ_CANARY".to_string(),
                "MISSING_KEY".to_string(),
            ]),
            ..Default::default()
        },
    );
    Config {
        cloud: Cloud::default(),
        evidence: evidence.to_string(),
        resources,
        ..Default::default()
    }
}

async fn fixture(server_url: &str) -> Handler {
    let mut env = HashMap::new();
    env.insert("TOKEN", "s3cret-token");
    env.insert("ZRIZ_CANARY", "canary-value");
    Handler::new(
        http_config(server_url, "redacted"),
        Options {
            lookup: Some(test_lookup(env)),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new")
}

#[tokio::test]
async fn health_with_query_and_projection() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/health"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"status":"ok","extra":1}"#),
        )
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/health?x=1".to_string()));
    let project = vec![
        vec!["status".to_string()],
        vec!["body".to_string(), "status".to_string()],
    ];
    let (result, err) = h
        .handle(test_op(
            "op-health",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            project,
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    assert_eq!(result.status, "pass");
    assert_eq!(
        result.payload.get("status").and_then(|v| v.as_i64()),
        Some(200)
    );
    let body = result
        .payload
        .get("body")
        .and_then(|v| v.as_object())
        .expect("body");
    assert_eq!(body.get("status").and_then(|v| v.as_str()), Some("ok"));
    assert!(!body.contains_key("extra"));
    assert_eq!(result.scrubbed, 0);
}

#[tokio::test]
async fn auth_register_scrubs_secret() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth/register"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"status":"ok","echo":"Bearer s3cret-token"}"#),
        )
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut headers = Map::new();
    headers.insert(
        "Authorization".to_string(),
        Value::String("Bearer ${TOKEN}".to_string()),
    );
    let mut body = Map::new();
    body.insert("email".to_string(), Value::String("a@b".to_string()));
    let mut args = HashMap::new();
    args.insert("method".to_string(), Value::String("POST".to_string()));
    args.insert(
        "path".to_string(),
        Value::String("/auth/register".to_string()),
    );
    args.insert("body".to_string(), Value::Object(body));
    args.insert("headers".to_string(), Value::Object(headers));
    let project = vec![
        vec!["status".to_string()],
        vec!["body".to_string(), "echo".to_string()],
    ];
    let (result, err) = h
        .handle(test_op(
            "op-register",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            project,
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    let body = result
        .payload
        .get("body")
        .and_then(|v| v.as_object())
        .expect("body");
    assert_eq!(
        body.get("echo").and_then(|v| v.as_str()),
        Some("Bearer [scrubbed]")
    );
    assert_eq!(result.scrubbed, 1);
}

#[tokio::test]
async fn placeholder_in_disallowed_slot() {
    let server = MockServer::start().await;
    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/${TOKEN}".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-bad-path",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "placeholder-in-disallowed-slot");
}

#[tokio::test]
async fn unknown_placeholder_names_key() {
    let server = MockServer::start().await;
    let h = fixture(&server.uri()).await;
    let mut body = Map::new();
    body.insert("x".to_string(), Value::String("${MISSING_KEY}".to_string()));
    let mut args = HashMap::new();
    args.insert("body".to_string(), Value::Object(body));
    let (result, err) = h
        .handle(test_op(
            "op-missing-key",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    let err = err.expect("error");
    assert_eq!(err.reason, "secret-not-set");
    assert_eq!(err.details["name"], "MISSING_KEY");
}

#[tokio::test]
async fn host_not_allowed() {
    let server = MockServer::start().await;
    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert(
        "path".to_string(),
        Value::String(".evil.example/x".to_string()),
    );
    let (result, err) = h
        .handle(test_op(
            "op-evil",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "host-not-allowed");
}

#[tokio::test]
async fn server_error_is_fail_result() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/err500"))
        .respond_with(
            ResponseTemplate::new(500)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"error":"x"}"#),
        )
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/err500".to_string()));
    let project = vec![
        vec!["status".to_string()],
        vec!["body".to_string(), "error".to_string()],
    ];
    let (result, err) = h
        .handle(test_op(
            "op-500",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            project,
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    assert_eq!(result.status, "fail");
    assert_eq!(
        result.payload.get("status").and_then(|v| v.as_i64()),
        Some(500)
    );
}

#[tokio::test]
async fn slow_server_times_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(300)))
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/slow".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-slow",
            "run-h",
            "http.request",
            "api",
            50,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "timeout");
}

#[tokio::test]
async fn status_only_projection_nulls_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/health"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"status":"ok"}"#),
        )
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/health".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-status-only",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![vec!["status".to_string()]],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    assert!(result.payload.get("body").is_none_or(Value::is_null));
}

#[tokio::test]
async fn text_response_kept_as_string() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/text"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .set_body_string("hello"),
        )
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/text".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-text",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![vec!["status".to_string()], vec!["body".to_string()]],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    assert_eq!(
        result.payload.get("body").and_then(|v| v.as_str()),
        Some("hello")
    );
}

#[tokio::test]
async fn redirect_same_origin_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/redirect"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/api/landed"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/landed"))
        .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/redirect".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-redirect-same",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![vec!["status".to_string()], vec!["body".to_string()]],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    assert_eq!(
        result.payload.get("status").and_then(|v| v.as_i64()),
        Some(200)
    );
    assert_eq!(
        result.payload.get("body").and_then(|v| v.as_str()),
        Some("landed")
    );
}

#[tokio::test]
async fn redirect_different_host_refused() {
    let evil = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&evil)
        .await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/redirect"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/x", evil.uri())),
        )
        .mount(&server)
        .await;

    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/redirect".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-redirect-evil",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "host-not-allowed");
}

async fn header_op(server: &MockServer, project: Vec<Vec<String>>) -> Map<String, Value> {
    Mock::given(method("GET"))
        .and(path("/api/hdr"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("location", "https://elsewhere.example/x")
                .append_header(
                    "set-cookie",
                    "sid=abc123; HttpOnly; Secure; SameSite=Lax; Path=/",
                )
                .append_header("set-cookie", "theme=dark; Max-Age=60")
                .insert_header("x-echo", "Bearer s3cret-token")
                .insert_header("x-other", "keep-out"),
        )
        .mount(server)
        .await;
    let h = fixture(&server.uri()).await;
    let mut headers = Map::new();
    headers.insert(
        "Authorization".to_string(),
        Value::String("Bearer ${TOKEN}".to_string()),
    );
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/hdr".to_string()));
    args.insert("headers".to_string(), Value::Object(headers));
    let (result, err) = h
        .handle(test_op(
            "op-hdr",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            project,
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let payload = result.expect("result").payload;
    payload
        .get("headers")
        .and_then(|v| v.as_object())
        .cloned()
        .expect("headers")
}

fn p(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn response_header_projected_only_when_asked() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/loc"))
        .respond_with(ResponseTemplate::new(200).insert_header("x-a", "1"))
        .mount(&server)
        .await;
    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/loc".to_string()));
    let (r, _) = h
        .handle(test_op(
            "o1",
            "run-h",
            "http.request",
            "api",
            2000,
            args.clone(),
            vec![p(&["headers", "x-a"])],
        ))
        .await;
    let hd = r.expect("r").payload["headers"].clone();
    assert_eq!(hd, serde_json::json!({"x-a": "1"}));
    let (r, _) = h
        .handle(test_op(
            "o2",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![p(&["status"])],
        ))
        .await;
    assert_eq!(r.expect("r").payload["headers"], serde_json::json!({}));
}

#[tokio::test]
async fn set_cookie_masked_attributes_kept() {
    let server = MockServer::start().await;
    let hd = header_op(&server, vec![p(&["headers", "set-cookie"])]).await;
    assert_eq!(
        hd["set-cookie"],
        serde_json::json!([
            "sid=[cookie]; HttpOnly; Secure; SameSite=Lax; Path=/",
            "theme=[cookie]; Max-Age=60"
        ])
    );
    assert!(!serde_json::to_string(&hd).unwrap().contains("abc123"));
    assert!(!hd.contains_key("x-other"));
    assert!(!hd.contains_key("x-echo"));
}

#[tokio::test]
async fn header_echoing_secret_is_scrubbed_and_unrequested_absent() {
    let server = MockServer::start().await;
    let hd = header_op(&server, vec![p(&["headers", "x-echo"])]).await;
    assert_eq!(hd.len(), 1);
    assert_eq!(hd["x-echo"], "Bearer [scrubbed]");
}

/// A handler whose lookup records every name it is asked for. The http
/// resource `api` carries `secrets`; the runner token env is `RUNNER_TOK`.
async fn recording_handler(
    base_url: &str,
    secrets: Option<Vec<&str>>,
) -> (Handler, Arc<Mutex<Vec<String>>>) {
    let mut cfg = http_config(base_url, "redacted");
    cfg.cloud.token_env = "RUNNER_TOK".to_string();
    if let Some(r) = cfg.resources.get_mut("api") {
        r.secrets = secrets.map(|v| v.into_iter().map(String::from).collect());
    }
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let lookup: crate::ops::Lookup = Arc::new(move |name: &str| {
        seen.lock().expect("lock").push(name.to_string());
        Some("env-value".to_string())
    });
    let h = Handler::new(
        cfg,
        Options {
            lookup: Some(lookup),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");
    // Building the handler resolves the listed names once; tests below
    // watch only what ops ask for.
    asked.lock().expect("lock").clear();
    (h, asked)
}

async fn sub_header_op(h: &Handler, value: &str) -> (Option<ResultFrame>, Option<ErrorFrame>) {
    let mut headers = Map::new();
    headers.insert("X-Key".to_string(), Value::String(value.to_string()));
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/x".to_string()));
    args.insert("headers".to_string(), Value::Object(headers));
    h.handle(test_op(
        "op-hdr-sub",
        "run-sub",
        "http.request",
        "api",
        2000,
        args,
        vec![],
    ))
    .await
}

#[tokio::test]
async fn no_secrets_list_means_no_env_names() {
    let (h, asked) = recording_handler("http://127.0.0.1:1", None).await;
    let (result, err) = sub_header_op(&h, "${SOME_ENV}").await;
    assert!(result.is_none());
    assert_eq!(err.expect("error").reason, "placeholder-not-found");
    assert!(asked.lock().expect("lock").is_empty(), "lookup was asked");
}

#[tokio::test]
async fn secrets_list_allows_only_listed_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/x"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (h, asked) = recording_handler(&server.uri(), Some(vec!["A"])).await;
    let (_, err) = sub_header_op(&h, "${A}").await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    assert_eq!(*asked.lock().expect("lock"), vec!["A".to_string()]);

    let (result, err) = sub_header_op(&h, "${B}").await;
    assert!(result.is_none());
    assert_eq!(err.expect("error").reason, "placeholder-not-found");
    assert_eq!(asked.lock().expect("lock").len(), 1, "B was looked up");
}

#[tokio::test]
async fn runner_token_name_is_never_substitutable() {
    let (h, asked) = recording_handler("http://127.0.0.1:1", Some(vec!["RUNNER_TOK"])).await;
    let (result, err) = sub_header_op(&h, "${RUNNER_TOK}").await;
    assert!(result.is_none());
    assert_eq!(err.expect("error").reason, "placeholder-not-found");
    assert!(asked.lock().expect("lock").is_empty(), "lookup was asked");
}

#[tokio::test]
async fn routing_and_hop_by_hop_headers_are_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let h = fixture(&server.uri()).await;
    for name in [
        "Host",
        "hOsT",
        "content-length",
        "Transfer-Encoding",
        "CONNECTION",
        "upgrade",
        "te",
        "trailer",
        "proxy-authorization",
        "Proxy-Connection",
        "keep-alive",
    ] {
        let mut args = HashMap::new();
        args.insert("path".to_string(), Value::String("/h".to_string()));
        args.insert("headers".to_string(), serde_json::json!({ name: "evil" }));
        let (result, err) = h
            .handle(test_op(
                "op-h",
                "run-h",
                "http.request",
                "api",
                2000,
                args,
                vec![],
            ))
            .await;
        assert!(result.is_none(), "{name}: got a result");
        assert_eq!(err.expect("error").reason, "runner-error", "{name}");
    }
    let got = server.received_requests().await.unwrap_or_default();
    assert!(got.is_empty(), "stub got {} requests", got.len());
}

#[tokio::test]
async fn ordinary_header_still_works() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/h"))
        .and(wiremock::matchers::header("x-foo", "bar"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let h = fixture(&server.uri()).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/h".to_string()));
    args.insert("headers".to_string(), serde_json::json!({"X-Foo": "bar"}));
    let (result, err) = h
        .handle(test_op(
            "op-h",
            "run-h",
            "http.request",
            "api",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(result.expect("result").status, "pass");
}
