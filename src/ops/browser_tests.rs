use super::super::announce::secretless_http_resources;
use crate::config::{Config, Resource};
use crate::ops::tests::test_op;
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

type Seen = Arc<Mutex<Vec<Value>>>;

/// A fake worker: answers ping, and every other request with `out`.
fn fake_worker(dir: &tempfile::TempDir, out: Value) -> (PathBuf, Seen) {
    fake_worker_out(dir, "w.sock", out)
}

fn fake_worker_named(dir: &tempfile::TempDir, name: &str) -> (PathBuf, Seen) {
    fake_worker_out(dir, name, json!({"ok": true}))
}

fn fake_worker_out(dir: &tempfile::TempDir, name: &str, out: Value) -> (PathBuf, Seen) {
    let sock = dir.path().join(name);
    let listener = UnixListener::bind(&sock).expect("bind");
    let seen: Seen = Arc::default();
    let log = seen.clone();
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
                json!({"v": 1, "kind": "ping", "ok": true})
            } else {
                log.lock().expect("lock").push(req.clone());
                json!({"v": 1, "op-id": req["op-id"], "ok": true, "out": out})
            };
            let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
        }
    });
    (sock, seen)
}

fn config(socket: &str, secrets: Option<Vec<&str>>) -> Config {
    let mut resources = HashMap::new();
    resources.insert(
        "web".to_string(),
        Resource {
            r#type: "browser".to_string(),
            base_url: "https://app.example.com".to_string(),
            origins: vec!["https://cdn.example.com".to_string()],
            secrets: secrets.map(|v| v.into_iter().map(String::from).collect()),
            ..Default::default()
        },
    );
    Config {
        resources,
        worker_socket: socket.to_string(),
        ..Default::default()
    }
}

async fn handler(
    socket: &str,
    secrets: Option<Vec<&str>>,
    env: &'static [(&str, &str)],
) -> Handler {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let opts = Options {
        lookup: Some(Arc::new(move |n: &str| env.get(n).cloned())),
        ..Default::default()
    };
    Handler::new(config(socket, secrets), opts)
        .await
        .expect("handler")
}

async fn run(
    h: &Handler,
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
        "run1",
        "browser.page",
        "web",
        3000,
        args,
        project,
    ))
    .await
}

fn fill(value: &str) -> Value {
    json!({"do": "fill", "target": {"label": "Password"}, "value": value})
}

const ENV: &[(&str, &str)] = &[("PW", "hunter2-pw"), ("OTHER", "other-secret")];

fn reason(
    r: &(
        Option<crate::contract::Result>,
        Option<crate::contract::Error>,
    ),
) -> String {
    r.1.as_ref().map(|e| e.reason.clone()).unwrap_or_default()
}

#[tokio::test]
async fn closed_arg_keys() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, seen) = fake_worker(&dir, json!({"ok": true}));
    let h = handler(sock.to_str().expect("p"), Some(vec!["PW"]), ENV).await;
    let r = run(&h, json!({"commands": [fill("x")], "shell": "ls"}), &[]).await;
    assert_eq!(reason(&r), "unknown-arg");
    assert!(seen.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn placeholder_refused_outside_fill_value() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, seen) = fake_worker(&dir, json!({"ok": true}));
    let h = handler(sock.to_str().expect("p"), Some(vec!["PW"]), ENV).await;
    let cases = [
        json!({"commands": [{"do": "goto", "path": "/${PW}"}]}),
        json!({"commands": [{"do": "click", "target": {"css": "${PW}"}}]}),
        json!({"commands": [fill("x")], "capture": {"K": ["${PW}"]}}),
    ];
    for args in cases {
        let r = run(&h, args, &[]).await;
        assert_eq!(reason(&r), "placeholder-in-disallowed-slot");
    }
    assert!(seen.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn secret_not_in_allowlist_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, seen) = fake_worker(&dir, json!({"ok": true}));
    let h = handler(sock.to_str().expect("p"), Some(vec!["PW"]), ENV).await;
    let r = run(&h, json!({"commands": [fill("${OTHER}")]}), &[]).await;
    assert_eq!(reason(&r), "placeholder-not-found");
    // No `secrets` at all means none allowed.
    let h2 = handler(sock.to_str().expect("p"), None, ENV).await;
    let r = run(&h2, json!({"commands": [fill("${PW}")]}), &[]).await;
    assert_eq!(reason(&r), "placeholder-not-found");
    assert!(seen.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn allowed_secret_reaches_worker_but_not_result() {
    let dir = tempfile::tempdir().expect("dir");
    let out = json!({"ok": true, "reads": {"echo": "typed hunter2-pw ok"}});
    let (sock, seen) = fake_worker(&dir, out);
    let h = handler(sock.to_str().expect("p"), Some(vec!["PW"]), ENV).await;
    let (res, err) = run(
        &h,
        json!({"commands": [{"do": "goto", "path": "/login"}, fill("${PW}")]}),
        &[&["ok"], &["reads", "echo"]],
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    let res = res.expect("result");
    assert_eq!(res.status, "pass");
    let sent = seen.lock().expect("lock")[0].clone();
    assert_eq!(sent["args"]["commands"][1]["value"], "hunter2-pw");
    assert_eq!(sent["policy"]["base-url"], "https://app.example.com");
    assert_eq!(
        sent["policy"]["origins"],
        json!(["https://app.example.com", "https://cdn.example.com"])
    );
    assert_eq!(sent["kind"], "browser.page");
    let text = serde_json::to_string(&res.payload).expect("json");
    assert!(!text.contains("hunter2-pw"), "{text}");
    assert!(res.scrubbed >= 1);
}

#[tokio::test]
async fn capture_from_reads_then_use_in_fill() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, seen) = fake_worker(&dir, json!({"ok": true, "reads": {"x": "code-4711"}}));
    let h = handler(sock.to_str().expect("p"), Some(vec![]), ENV).await;
    let (res, err) = run(
        &h,
        json!({"commands": [fill("a")], "capture": {"CODE": ["reads", "x"]}}),
        &[&["reads", "x"]],
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    let res = res.expect("result");
    assert_eq!(res.payload["reads"]["x"], "[captured]");
    // The vault value is usable in a later fill without being in `secrets`.
    let (_, err) = run(&h, json!({"commands": [fill("${CODE}")]}), &[]).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(
        seen.lock().expect("lock")[1]["args"]["commands"][0]["value"],
        "code-4711"
    );
}

#[tokio::test]
async fn failed_command_list_is_fail_status() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, _) = fake_worker(
        &dir,
        json!({"ok": false, "failed-at": 0, "error": "not-found"}),
    );
    let h = handler(sock.to_str().expect("p"), Some(vec![]), ENV).await;
    let (res, _) = run(&h, json!({"commands": [fill("a")]}), &[&["error"]]).await;
    let res = res.expect("result");
    assert_eq!(res.status, "fail");
    assert_eq!(res.payload["error"], "not-found");
}

#[tokio::test]
async fn kind_announced_only_when_worker_up() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, _) = fake_worker(&dir, json!({"ok": true}));
    let up = handler(sock.to_str().expect("p"), Some(vec![]), ENV).await;
    assert!(up.op_kinds().contains(&"browser.page".to_string()));

    let down = handler("/nonexistent/zriz-worker.sock", Some(vec![]), ENV).await;
    assert!(!down.op_kinds().contains(&"browser.page".to_string()));
    assert_eq!(down.op_kinds().len(), 3);
    let r = run(&down, json!({"commands": [fill("a")]}), &[]).await;
    assert_eq!(reason(&r), "worker-unavailable");
}

#[tokio::test]
async fn refresh_picks_up_a_worker_that_comes_back() {
    let dir = tempfile::tempdir().expect("dir");
    let h = handler(
        dir.path().join("late.sock").to_str().expect("p"),
        Some(vec![]),
        ENV,
    )
    .await;
    assert!(!h.op_kinds().contains(&"browser.page".to_string()));
    let (_sock, _) = fake_worker_named(&dir, "late.sock");
    *h.last_ping.lock().expect("lock") = None;
    h.refresh_worker().await;
    assert!(h.op_kinds().contains(&"browser.page".to_string()));
}

#[tokio::test]
async fn browser_resource_listed_only_when_worker_up() {
    let dir = tempfile::tempdir().expect("dir");
    let h = handler(
        dir.path().join("r.sock").to_str().expect("p"),
        Some(vec![]),
        ENV,
    )
    .await;
    let configured = vec!["web".to_string(), "other".to_string()];
    assert_eq!(h.announced_resources(&configured), vec!["other"]);
    let (_sock, _) = fake_worker_named(&dir, "r.sock");
    *h.last_ping.lock().expect("lock") = None;
    h.refresh_worker().await;
    assert_eq!(h.announced_resources(&configured), configured);
}

#[test]
fn http_without_secrets_is_named_once() {
    let mut cfg = Config::default();
    for (id, secrets) in [("a", None), ("b", Some(vec!["X".to_string()])), ("c", None)] {
        cfg.resources.insert(
            id.to_string(),
            Resource {
                r#type: "http".to_string(),
                base_url: "http://h".to_string(),
                secrets,
                ..Default::default()
            },
        );
    }
    assert_eq!(secretless_http_resources(&cfg), vec!["a", "c"]);
}

#[tokio::test]
async fn http_secrets_allowlist_enforced() {
    let mut cfg = Config::default();
    cfg.resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            secrets: Some(vec!["OK".to_string()]),
            ..Default::default()
        },
    );
    let opts = Options {
        lookup: Some(Arc::new(|_| Some("v".to_string()))),
        ..Default::default()
    };
    let h = Handler::new(cfg, opts).await.expect("handler");
    let args: HashMap<String, Value> =
        serde_json::from_value(json!({"path": "/", "headers": {"x": "${NO}"}})).expect("args");
    let (_, err) = h
        .handle(test_op("o", "r", "http.request", "api", 1000, args, vec![]))
        .await;
    assert_eq!(err.expect("err").reason, "placeholder-not-found");
}

#[tokio::test]
async fn browser_result_scrubs_secret_listed_on_another_resource() {
    let dir = tempfile::tempdir().expect("dir");
    let out = json!({"ok": true, "reads": {"echo": "saw other-secret here"}});
    let (sock, _seen) = fake_worker(&dir, out);
    let mut cfg = config(sock.to_str().expect("p"), None);
    cfg.resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: "https://api.example.com".to_string(),
            secrets: Some(vec!["OTHER".to_string()]),
            ..Default::default()
        },
    );
    let env: HashMap<String, String> = ENV
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let opts = Options {
        lookup: Some(Arc::new(move |n: &str| env.get(n).cloned())),
        ..Default::default()
    };
    let h = Handler::new(cfg, opts).await.expect("handler");
    let (res, err) = run(
        &h,
        json!({"commands": [{"do": "goto", "path": "/"}]}),
        &[&["reads", "echo"]],
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    let res = res.expect("result");
    let text = serde_json::to_string(&res.payload).expect("json");
    assert!(!text.contains("other-secret"), "{text}");
    assert!(text.contains("[scrubbed]"), "{text}");
}

fn op() -> crate::contract::Op {
    test_op(
        "op",
        "run1",
        "browser.page",
        "shop-browser",
        3000,
        HashMap::new(),
        vec![],
    )
}

#[test]
fn capacity_carries_the_numbers() {
    let resp = json!({"v": 1, "op-id": "op", "ok": false, "reason": "at-capacity",
        "message": "free text", "max-contexts": 8, "busy": 8, "waited-ms": 5000});
    let e = super::worker_out(&op(), &resp, 600_000).expect_err("error");
    assert_eq!(e.reason, "runner-at-capacity");
    assert_eq!(
        Value::Object(e.details),
        json!({"resource": "shop-browser", "limit-name": "max-contexts", "limit": 8,
            "busy": 8, "waited-ms": 5000})
    );
}

#[test]
fn context_lost_is_idle() {
    let resp = json!({"ok": false, "reason": "context-lost", "why": "idle",
        "message": "free text", "idle-ms": 600000, "max-contexts": 3});
    let e = super::worker_out(&op(), &resp, 600_000).expect_err("error");
    assert_eq!(e.reason, "context-lost");
    assert_eq!(
        Value::Object(e.details),
        json!({"resource": "shop-browser", "why": "idle", "idle-ms": 600000})
    );
}

/// A worker that answers a ping and then never answers an op.
fn hanging_worker(dir: &tempfile::TempDir) -> PathBuf {
    let sock = dir.path().join("hang.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (rd, mut wr) = stream.into_split();
                let mut line = String::new();
                let _ = BufReader::new(rd).read_line(&mut line).await;
                if line.contains("\"ping\"") {
                    let _ = wr
                        .write_all(b"{\"v\":1,\"kind\":\"ping\",\"ok\":true}\n")
                        .await;
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
            });
        }
    });
    sock
}

#[tokio::test]
async fn no_worker_answer_by_the_deadline_is_worker_deadline_not_timeout() {
    let dir = tempfile::tempdir().expect("dir");
    let sock = hanging_worker(&dir);
    let h = handler(sock.to_str().expect("path"), None, &[]).await;
    let args: HashMap<String, Value> =
        serde_json::from_value(json!({"commands": [{"do": "title"}]})).expect("args");
    let (r, e) = h
        .handle(test_op(
            "op",
            "run1",
            "browser.page",
            "web",
            300,
            args,
            vec![],
        ))
        .await;
    assert!(r.is_none());
    let e = e.expect("error");
    assert_eq!(e.reason, "runner-error");
    assert_eq!(e.details["where"], "worker-deadline");
}
