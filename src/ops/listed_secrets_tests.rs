//! A value listed in any resource's `secrets` is scrubbed from every op's
//! result, whether or not that op substituted it.

use super::*;
use crate::config::{Cloud, Config, Resource};
use crate::ops::sql::tests::{Expectation, FakeSqlConn};
use crate::ops::tests::test_op;
use std::collections::HashMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const A_VAL: &str = "a-secret-value-91x";

fn config(base: &str) -> Config {
    let mut resources = HashMap::new();
    resources.insert(
        "a".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{base}/a"),
            secrets: Some(vec!["A_KEY".to_string(), "UNSET_KEY".to_string()]),
            ..Default::default()
        },
    );
    resources.insert(
        "b".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{base}/b"),
            ..Default::default()
        },
    );
    resources.insert(
        "db".to_string(),
        Resource {
            r#type: "sql".to_string(),
            connection: "dsn".to_string(),
            read_only: true,
            ..Default::default()
        },
    );
    Config {
        cloud: Cloud {
            token_env: "TOK".to_string(),
            ..Default::default()
        },
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    }
}

async fn handler(base: &str, rows: Vec<Map<String, Value>>) -> Handler {
    let conn = FakeSqlConn::new(vec![Expectation {
        want_read_only: true,
        rows: Ok(rows),
    }]);
    let open_sql: OpenSql = Arc::new(move |_dsn: &str| {
        let c = Arc::clone(&conn) as Arc<dyn SqlConn>;
        Box::pin(async move { Ok(c) })
            as BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>>
    });
    let env: HashMap<&'static str, &'static str> =
        HashMap::from([("A_KEY", A_VAL), ("TOK", "tok")]);
    let opts = Options {
        open_sql: Some(open_sql),
        lookup: Some(crate::ops::tests::test_lookup(env)),
        ..Default::default()
    };
    Handler::new(config(base), opts).await.expect("handler")
}

fn args(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn project(p: &[&[&str]]) -> Vec<Vec<String>> {
    p.iter()
        .map(|x| x.iter().map(|s| s.to_string()).collect())
        .collect()
}

#[tokio::test]
async fn http_on_other_resource_scrubs_listed_value() {
    let server = MockServer::start().await;
    let cases: [(&str, String, &str); 2] = [
        (
            "plain",
            format!("hello {A_VAL} end"),
            "hello [scrubbed] end",
        ),
        ("no-secret", "hello world".to_string(), "hello world"),
    ];
    for (name, body, want) in cases {
        Mock::given(method("GET"))
            .and(path("/b/read"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let h = handler(&server.uri(), vec![]).await;
        let op = test_op(
            "op",
            "run",
            "http.request",
            "b",
            2000,
            args(&[("path", Value::from("/read"))]),
            project(&[&["body"]]),
        );
        let (res, err) = h.handle(op).await;
        assert!(err.is_none(), "{name}: {err:?}");
        let got = res.expect("result").payload;
        assert_eq!(
            got.get("body").and_then(Value::as_str),
            Some(want),
            "{name}"
        );
        server.reset().await;
    }
}

#[tokio::test]
async fn sql_row_holding_listed_http_secret_is_scrubbed() {
    let mut row = Map::new();
    row.insert("v".to_string(), Value::from(format!("x{A_VAL}y")));
    let h = handler("http://127.0.0.1:1", vec![row]).await;
    let op = test_op(
        "op",
        "run",
        "sql.query",
        "db",
        2000,
        args(&[("query", Value::from("SELECT v FROM t"))]),
        project(&[&["rows", "0", "v"]]),
    );
    let (res, err) = h.handle(op).await;
    assert!(err.is_none(), "{err:?}");
    let payload = res.expect("result").payload;
    let cell = payload
        .get("rows")
        .and_then(|r| r.get(0))
        .and_then(|r| r.get("v"))
        .and_then(Value::as_str);
    assert_eq!(cell, Some("x[scrubbed]y"));
}

#[tokio::test]
async fn unset_listed_name_scrubs_nothing_and_op_works() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a/read"))
        .respond_with(ResponseTemplate::new(200).set_body_string("plain body"))
        .mount(&server)
        .await;
    let h = handler(&server.uri(), vec![]).await;
    assert_eq!(h.listed_secrets, vec![A_VAL.to_string()]);
    let op = test_op(
        "op",
        "run",
        "http.request",
        "a",
        2000,
        args(&[("path", Value::from("/read"))]),
        project(&[&["body"]]),
    );
    let (res, err) = h.handle(op).await;
    assert!(err.is_none(), "{err:?}");
    let res = res.expect("result");
    assert_eq!(
        res.payload.get("body").and_then(Value::as_str),
        Some("plain body")
    );
    assert_eq!(res.scrubbed, 0);
}

#[tokio::test]
async fn substitution_rules_are_unchanged() {
    let server = MockServer::start().await;
    let cases: [(&str, &str, &str, Value); 3] = [
        (
            "http-other-resource",
            "http.request",
            "b",
            Value::from("Bearer ${A_KEY}"),
        ),
        (
            "sql-listed-elsewhere",
            "sql.query",
            "db",
            Value::from("SELECT '${A_KEY}'"),
        ),
        (
            "http-own-path-slot",
            "http.request",
            "a",
            Value::from("/${A_KEY}"),
        ),
    ];
    for (name, kind, res, val) in cases {
        let h = handler(&server.uri(), vec![]).await;
        let a = if kind == "sql.query" {
            args(&[("query", val)])
        } else if name == "http-own-path-slot" {
            args(&[("path", val)])
        } else {
            args(&[
                ("path", Value::from("/x")),
                ("headers", serde_json::json!({"authorization": val})),
            ])
        };
        let (r, err) = h
            .handle(test_op("op", "run", kind, res, 2000, a, vec![]))
            .await;
        assert!(r.is_none(), "{name}");
        let want = if name == "http-other-resource" {
            "placeholder-not-found"
        } else {
            "placeholder-in-disallowed-slot"
        };
        assert_eq!(err.expect("error").reason, want, "{name}");
    }
}
