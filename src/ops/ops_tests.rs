use super::*;
use crate::config::{Cloud, Resource};
use std::collections::HashMap as StdHashMap;

pub(crate) fn test_lookup(env: StdHashMap<&'static str, &'static str>) -> Lookup {
    Arc::new(move |name: &str| env.get(name).map(|v| v.to_string()))
}

pub(crate) fn fixed_now(t: SystemTime) -> NowFn {
    Arc::new(move || t)
}

pub(crate) fn test_op(
    op_id: &str,
    run_id: &str,
    kind: &str,
    resource: &str,
    timeout_ms: i64,
    args: HashMap<String, Value>,
    project: Vec<Vec<String>>,
) -> contract::Op {
    contract::Op {
        op_id: op_id.to_string(),
        run_id: run_id.to_string(),
        step_index: 1,
        kind: kind.to_string(),
        resource: resource.to_string(),
        timeout_ms,
        args,
        project,
        trace_id: None,
    }
}

fn http_test_config(base_url: &str, evidence: &str) -> Config {
    let mut resources = StdHashMap::new();
    resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: base_url.to_string(),
            connection: String::new(),
            read_only: false,
            cookies: false,
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

async fn new_test_handler(cfg: Config, opts: Options) -> Handler {
    Handler::new(cfg, opts).await.expect("Handler::new")
}

#[tokio::test]
async fn dispatch_unknowns() {
    let mut env = StdHashMap::new();
    env.insert("TOKEN", "s3cret-token");
    let h = new_test_handler(
        http_test_config("http://example.invalid", "redacted"),
        Options {
            lookup: Some(test_lookup(env)),
            ..Default::default()
        },
    )
    .await;

    let (result, err) = h
        .handle(test_op(
            "op-1",
            "run-1",
            "shell.exec",
            "api",
            1000,
            HashMap::new(),
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "unknown-kind");

    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/x".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-2",
            "run-1",
            "http.request",
            "nope",
            1000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "unknown-resource");

    let mut args = HashMap::new();
    args.insert("foo".to_string(), Value::String("bar".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-3",
            "run-1",
            "http.request",
            "api",
            1000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "unknown-arg");
}

#[tokio::test]
async fn max_inflight_sums_per_resource_type() {
    let mut resources = StdHashMap::new();
    resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: "http://example.invalid".to_string(),
            ..Default::default()
        },
    );
    resources.insert(
        "db".to_string(),
        Resource {
            r#type: "sql".to_string(),
            connection: "dsn-a".to_string(),
            read_only: true,
            cookies: false,
            ..Default::default()
        },
    );
    resources.insert(
        "db-rw".to_string(),
        Resource {
            r#type: "sql".to_string(),
            connection: "dsn-b".to_string(),
            read_only: false,
            cookies: false,
            ..Default::default()
        },
    );
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let h = new_test_handler(
        cfg,
        Options {
            open_sql: Some(sql::tests::fake_open_sql()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(h.max_inflight(), 8);
}

/// `Handler::close` closes every sql connection the handler opened. It must reach every sql
/// resource, not just the first, and must be safe to call from `&self`.
#[tokio::test]
async fn close_closes_every_sql_resource() {
    let mut resources = StdHashMap::new();
    resources.insert(
        "db-a".to_string(),
        Resource {
            r#type: "sql".to_string(),
            connection: "dsn-a".to_string(),
            ..Default::default()
        },
    );
    resources.insert(
        "db-b".to_string(),
        Resource {
            r#type: "sql".to_string(),
            connection: "dsn-b".to_string(),
            ..Default::default()
        },
    );
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let close_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = new_test_handler(
        cfg,
        Options {
            open_sql: Some(sql::tests::fake_open_sql_counting_closes(Arc::clone(
                &close_count,
            ))),
            ..Default::default()
        },
    )
    .await;

    h.close().await;

    assert_eq!(close_count.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// `Handler::new` closes every already-opened sql connection
/// if a later resource fails to open. It must
/// instead of leaking the earlier connections it already opened.
#[tokio::test]
async fn new_closes_already_opened_sql_resources_on_later_failure() {
    let mut resources = StdHashMap::new();
    for name in ["db-a", "db-b", "db-c"] {
        resources.insert(
            name.to_string(),
            Resource {
                r#type: "sql".to_string(),
                connection: format!("dsn-{name}"),
                ..Default::default()
            },
        );
    }
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let close_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let result = Handler::new(
        cfg,
        Options {
            // 3 sql resources, the fake fails from the 3rd call onward: the
            // first 2 opens succeed and must be closed once the 3rd fails.
            open_sql: Some(sql::tests::fake_open_sql_fail_after(
                2,
                Arc::clone(&close_count),
            )),
            ..Default::default()
        },
    )
    .await;

    match result {
        Err(NewError::OpenSql(_, _)) => {}
        Err(_) => panic!("expected NewError::OpenSql"),
        Ok(_) => panic!("third sql resource must fail to open"),
    }
    assert_eq!(close_count.load(std::sync::atomic::Ordering::SeqCst), 2);
}

fn payload_of(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

#[test]
fn scrub_payload_catches_json_escaped_secret() {
    let secret = "a\"b\nc\td\\e".to_string();
    let payload = payload_of(serde_json::json!({
        "status": 200,
        "body": format!("echo {secret} end"),
        "items": [{"k": secret.clone(), "n": 1}],
    }));
    let (out, count) = scrub_payload(&payload, std::slice::from_ref(&secret)).expect("scrub");
    let text = serde_json::to_string(&out).expect("encode");
    assert!(!text.contains("a\\\"b"), "leaked: {text}");
    assert!(!text.contains("c\\td"), "leaked: {text}");
    assert_eq!(count, 2);
    assert_eq!(out["status"], 200);
}

#[test]
fn scrub_payload_syntax_and_key_secrets_keep_structure() {
    for secret in ["{", "\"", "status"] {
        let payload = payload_of(serde_json::json!({
            "status": 200,
            "body": "{\"status\":\"ok\"}",
            "list": [true, null, 1.5],
        }));
        let (out, _) = scrub_payload(&payload, &[secret.to_string()])
            .unwrap_or_else(|e| panic!("secret {secret:?} errored: {e}"));
        assert_eq!(out.len(), 3, "secret {secret:?}: {out:?}");
        assert_eq!(out["list"], serde_json::json!([true, null, 1.5]));
        if secret != "status" {
            assert_eq!(out["status"], 200);
        }
    }
}

#[test]
fn scrub_payload_nested_array_of_objects() {
    let payload = payload_of(serde_json::json!({
        "rows": [{"a": [{"b": "tok-9 here"}]}, {"c": "clean"}],
    }));
    let (out, count) = scrub_payload(&payload, &["tok-9".to_string()]).expect("scrub");
    assert_eq!(count, 1);
    assert_eq!(out["rows"][0]["a"][0]["b"], "[scrubbed] here");
    assert_eq!(out["rows"][1]["c"], "clean");
}

#[tokio::test(start_paused = true)]
async fn zero_timeout_still_has_a_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    let h = new_test_handler(
        http_test_config(&format!("http://{addr}"), "redacted"),
        Options::default(),
    )
    .await;
    for t in [0, -5] {
        let mut args = HashMap::new();
        args.insert("path".to_string(), Value::String("/hang".to_string()));
        let (result, err) = h
            .handle(test_op(
                "op-t",
                "run-t",
                "http.request",
                "api",
                t,
                args,
                vec![],
            ))
            .await;
        assert!(result.is_none());
        assert_eq!(err.expect("error").reason, "timeout", "timeout-ms {t}");
    }
}
