//! `context-lost` after a restart of the worker (new `boot-id`), the
//! placeholder words, and the shape of the frames.

use crate::config::{Config, Resource};
use crate::ops::tests::test_op;
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

type Boot = Arc<Mutex<String>>;

/// A fake worker: the ping has the `boot-id` in `boot`; any other request is
/// answered `ok`.
fn fake_worker(dir: &tempfile::TempDir) -> (String, Boot) {
    let sock = dir.path().join("w.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    let boot: Boot = Arc::new(Mutex::new("boot-1".to_string()));
    let b = boot.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(rd).read_line(&mut line).await.is_err() {
                continue;
            }
            let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let resp = if req["kind"] == "ping" {
                let id = b.lock().expect("lock").clone();
                json!({"v": 1, "kind": "ping", "ok": true, "browser": [], "cli": {"busy": 0, "limit": 2}, "boot-id": id})
            } else if req["kind"] == "run.close" {
                json!({"v": 1, "kind": "run.close", "ok": true, "closed": 1})
            } else {
                json!({"v": 1, "op-id": req["op-id"], "ok": true, "out": {"ok": true}})
            };
            let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
        }
    });
    (sock.to_str().expect("path").to_string(), boot)
}

async fn handler(socket: &str, secrets: Vec<&str>, env: &'static [(&str, &str)]) -> Handler {
    let mut resources = HashMap::new();
    resources.insert(
        "web".to_string(),
        Resource {
            r#type: "browser".to_string(),
            base_url: "https://app.example.com".to_string(),
            secrets: Some(secrets.into_iter().map(String::from).collect()),
            ..Default::default()
        },
    );
    let cfg = Config {
        resources,
        worker_socket: socket.to_string(),
        ..Default::default()
    };
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let opts = Options {
        lookup: Some(Arc::new(move |n: &str| env.get(n).cloned())),
        ..Default::default()
    };
    Handler::new(cfg, opts).await.expect("handler")
}

async fn page(
    h: &Handler,
    run: &str,
    value: &str,
) -> (
    Option<crate::contract::Result>,
    Option<crate::contract::Error>,
) {
    let cmd = json!({"do": "fill", "target": {"label": "Password"}, "value": value});
    let args: HashMap<String, Value> =
        serde_json::from_value(json!({"commands": [cmd]})).expect("args");
    h.handle(test_op(
        "op",
        run,
        "browser.page",
        "web",
        3000,
        args,
        vec![],
    ))
    .await
}

fn failed(
    r: (
        Option<crate::contract::Result>,
        Option<crate::contract::Error>,
    ),
) -> crate::contract::Error {
    r.1.expect("error frame")
}

#[tokio::test]
async fn context_lost_once_after_a_new_boot_id() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, boot) = fake_worker(&dir);
    let h = handler(&sock, vec![], &[]).await;
    assert!(page(&h, "run1", "x").await.0.is_some());
    assert!(page(&h, "run2", "x").await.0.is_some());
    *boot.lock().expect("lock") = "boot-2".to_string();
    // run2 got its notice: it is not lost.
    h.run_ended("run2", "").await;
    assert!(page(&h, "run2", "x").await.0.is_some());
    let e = failed(page(&h, "run1", "x").await);
    assert_eq!(e.reason, "context-lost");
    assert_eq!(
        Value::Object(e.details),
        json!({"resource": "web", "why": "worker-restarted"})
    );
    // One time only.
    assert!(page(&h, "run1", "x").await.0.is_some());
    assert!(page(&h, "run1", "x").await.0.is_some());
}

#[tokio::test]
async fn same_boot_id_loses_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, _boot) = fake_worker(&dir);
    let h = handler(&sock, vec![], &[]).await;
    for _ in 0..3 {
        assert!(page(&h, "run1", "x").await.0.is_some());
    }
}

#[tokio::test]
async fn placeholder_in_no_list_is_not_found() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, _boot) = fake_worker(&dir);
    let h = handler(&sock, vec!["PW"], &[("PW", "v"), ("OTHER", "o")]).await;
    let e = failed(page(&h, "run1", "${OTHER}").await);
    assert_eq!(e.reason, "placeholder-not-found");
    assert_eq!(
        Value::Object(e.details),
        json!({"name": "OTHER", "resource": "web"})
    );
}

#[tokio::test]
async fn listed_secret_with_no_value_is_not_set() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, _boot) = fake_worker(&dir);
    let h = handler(&sock, vec!["PW"], &[]).await;
    let e = failed(page(&h, "run1", "${PW}").await);
    assert_eq!(e.reason, "secret-not-set");
    assert_eq!(Value::Object(e.details), json!({"name": "PW"}));
}
