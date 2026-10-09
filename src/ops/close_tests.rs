//! `run.close` is tried whatever the flag says, a refusal is retried with the
//! next good ping, the trace id is checked.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::config::{Config, Resource};
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

type Shared = Arc<Mutex<Value>>;
type Seen = Arc<Mutex<Vec<Value>>>;

const TRACE: &str = "5d0c9a52-7c1e-4b1f-9a55-0d6f3a2b8c11";

/// A fake worker: ping is good; `run.close` is recorded and answered `close`.
fn worker(dir: &tempfile::TempDir) -> (String, Shared, Seen) {
    let sock = dir.path().join("w.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    let close: Shared = Arc::new(Mutex::new(json!({"ok": true, "closed": 1})));
    let seen: Seen = Arc::default();
    let (c, s) = (close.clone(), seen.clone());
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
                s.lock().unwrap().push(req);
                let mut r = c.lock().unwrap().clone();
                r["v"] = json!(1);
                r
            };
            let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
        }
    });
    (sock.to_str().unwrap().to_string(), close, seen)
}

async fn up() -> (tempfile::TempDir, Handler, Shared, Seen) {
    let dir = tempfile::tempdir().unwrap();
    let (sock, close, seen) = worker(&dir);
    let res = Resource {
        r#type: "browser".into(),
        base_url: "https://app.example.com".into(),
        ..Default::default()
    };
    let cfg = Config {
        resources: HashMap::from([("web".to_string(), res)]),
        worker_socket: sock,
        ..Default::default()
    };
    let h = Handler::new(cfg, Options::default()).await.unwrap();
    (dir, h, close, seen)
}

async fn ping_again(h: &Handler) {
    *h.last_ping.lock().unwrap() = None;
    h.refresh_worker().await;
}

#[tokio::test]
async fn run_close_is_tried_when_the_flag_says_down() {
    let (_d, h, _c, seen) = up().await;
    h.worker_up.store(false, Ordering::SeqCst);
    h.run_ended("run-a", TRACE).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_refused_close_is_sent_again_with_the_next_good_ping() {
    let (_d, h, close, seen) = up().await;
    *close.lock().unwrap() = json!({"ok": false, "reason": "internal"});
    h.run_ended("run-a", TRACE).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    *close.lock().unwrap() = json!({"ok": true, "closed": 1});
    ping_again(&h).await;
    assert_eq!(seen.lock().unwrap().len(), 2, "sent again");
    assert_eq!(seen.lock().unwrap()[1]["run"], "run-a");
    ping_again(&h).await;
    assert_eq!(seen.lock().unwrap().len(), 2, "and only once more");
}

#[tokio::test]
async fn the_retry_list_keeps_64() {
    let (_d, h, close, seen) = up().await;
    *close.lock().unwrap() = json!({"ok": false});
    for i in 0..70 {
        h.run_ended(&format!("run-{i}"), TRACE).await;
    }
    seen.lock().unwrap().clear();
    *close.lock().unwrap() = json!({"ok": true, "closed": 0});
    ping_again(&h).await;
    let runs: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| r["run"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(runs.len(), 64);
    assert!(!runs.contains(&"run-5".to_string()) && runs.contains(&"run-6".to_string()));
}

#[tokio::test]
async fn a_bad_trace_id_does_not_reach_the_worker() {
    let (_d, h, _c, seen) = up().await;
    h.run_ended("run-a", "not a trace id; secret=abc").await;
    let s = seen.lock().unwrap();
    assert_eq!(s.len(), 1);
    assert!(s[0].get("trace-id").is_none(), "{:?}", s[0]);
    assert!(!s[0].to_string().contains("secret"));
}
