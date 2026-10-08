use super::*;
use crate::config::{Cloud, Config, Resource};
use crate::ops::Options;
use crate::ops::tests::{fixed_now, test_lookup, test_op};
use serde_json::json;
use std::collections::{HashMap, HashMap as StdHashMap};
use std::time::{Duration, SystemTime};

fn config(evidence: &str) -> Config {
    let mut resources = StdHashMap::new();
    resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: "http://example.invalid".to_string(),
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

#[tokio::test]
async fn fetch_before_any_op_is_expired() {
    let mut env = StdHashMap::new();
    env.insert("TOKEN", "s3cret-token");
    let h = Handler::new(
        config("redacted"),
        Options {
            lookup: Some(test_lookup(env)),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");

    let mut args = HashMap::new();
    args.insert("op-id".to_string(), Value::String("whatever".to_string()));
    let (result, err) = h
        .handle(test_op(
            "fetch-1",
            "run-empty",
            "evidence.fetch",
            "",
            1000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "evidence-expired");
}

#[tokio::test]
async fn disabled_when_config_says_none() {
    let mut env = StdHashMap::new();
    env.insert("TOKEN", "s3cret-token");
    let h = Handler::new(
        config("none"),
        Options {
            lookup: Some(test_lookup(env)),
            now: Some(fixed_now(SystemTime::UNIX_EPOCH)),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");

    h.evidence.put(
        "run-1",
        crate::evidence::Entry {
            op_id: "op-seed".to_string(),
            status: 200,
            body: json!({"status": "ok"}),
            secrets: vec![],
        },
        SystemTime::UNIX_EPOCH,
    );

    let mut args = HashMap::new();
    args.insert("op-id".to_string(), Value::String("op-seed".to_string()));
    let (result, err) = h
        .handle(test_op(
            "fetch-5",
            "run-1",
            "evidence.fetch",
            "",
            1000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "evidence-disabled");
}

#[tokio::test]
async fn op_id_mismatch_is_expired() {
    let mut env = StdHashMap::new();
    env.insert("TOKEN", "s3cret-token");
    let h = Handler::new(
        config("redacted"),
        Options {
            lookup: Some(test_lookup(env)),
            now: Some(fixed_now(SystemTime::UNIX_EPOCH)),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");

    h.evidence.put(
        "run-1",
        crate::evidence::Entry {
            op_id: "op-seed".to_string(),
            status: 200,
            body: json!({"status": "ok"}),
            secrets: vec![],
        },
        SystemTime::UNIX_EPOCH,
    );

    let mut args = HashMap::new();
    args.insert(
        "op-id".to_string(),
        Value::String("not-the-seed".to_string()),
    );
    let (result, err) = h
        .handle(test_op(
            "fetch-2",
            "run-1",
            "evidence.fetch",
            "",
            1000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "evidence-expired");
}

#[tokio::test]
async fn happy_path_redacts_and_scrubs() {
    let mut env = StdHashMap::new();
    env.insert("TOKEN", "s3cret-token");
    let h = Handler::new(
        config("redacted"),
        Options {
            lookup: Some(test_lookup(env)),
            now: Some(fixed_now(SystemTime::UNIX_EPOCH)),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");

    h.evidence.put(
            "run-1",
            crate::evidence::Entry {
                op_id: "op-seed".to_string(),
                status: 200,
                body: json!({"status": "ok", "echo": "Bearer s3cret-token", "nested": {"set-cookie": "abc123"}}),
                secrets: vec!["s3cret-token".to_string()],
            },
            SystemTime::UNIX_EPOCH,
        );

    let mut args = HashMap::new();
    args.insert("op-id".to_string(), Value::String("op-seed".to_string()));
    let (result, err) = h
        .handle(test_op(
            "fetch-3",
            "run-1",
            "evidence.fetch",
            "",
            1000,
            args,
            vec![],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    let excerpt = result
        .payload
        .get("excerpt")
        .and_then(|v| v.as_str())
        .expect("excerpt");
    assert!(excerpt.len() <= 4096);
    assert!(
        excerpt.contains(r#""set-cookie":"[redacted]""#),
        "excerpt: {excerpt}"
    );
    assert!(excerpt.contains("[scrubbed]"), "excerpt: {excerpt}");
    assert!(!excerpt.contains("s3cret-token"), "excerpt: {excerpt}");
    assert_eq!(
        result.payload.get("status").and_then(|v| v.as_i64()),
        Some(200)
    );
}

#[tokio::test]
async fn expired_after_advance() {
    let mut env = StdHashMap::new();
    env.insert("TOKEN", "s3cret-token");
    let h = Handler::new(
        config("redacted"),
        Options {
            lookup: Some(test_lookup(env)),
            now: Some(fixed_now(SystemTime::UNIX_EPOCH + Duration::from_secs(121))),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");

    h.evidence.put(
        "run-1",
        crate::evidence::Entry {
            op_id: "op-seed".to_string(),
            status: 200,
            body: json!({"status": "ok"}),
            secrets: vec![],
        },
        SystemTime::UNIX_EPOCH,
    );

    let mut args = HashMap::new();
    args.insert("op-id".to_string(), Value::String("op-seed".to_string()));
    let (result, err) = h
        .handle(test_op(
            "fetch-4",
            "run-1",
            "evidence.fetch",
            "",
            1000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "evidence-expired");
}
