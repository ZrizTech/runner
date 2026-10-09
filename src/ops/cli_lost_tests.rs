//! The words for a handle the runner or the worker lost, and the `run-closed`
//! word of the worker.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::tests::{config, run};
use crate::contract;
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

type Shared = Arc<Mutex<Value>>;

/// A fake worker: the ping has `boot-id` from `boot`; any other request is
/// answered with `reply`. Counts the ops it got.
fn worker(dir: &tempfile::TempDir) -> (String, Shared, Shared, Arc<Mutex<usize>>) {
    let sock = dir.path().join("w.sock");
    let listener = UnixListener::bind(&sock).expect("bind");
    let boot: Shared = Arc::new(Mutex::new(json!("boot-1")));
    let reply: Shared = Arc::new(Mutex::new(json!({"ok": true, "out": {"exit-code": 0}})));
    let count = Arc::new(Mutex::new(0usize));
    let (b, r, c) = (boot.clone(), reply.clone(), count.clone());
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
                json!({"v": 1, "kind": "ping", "ok": true, "boot-id": b.lock().unwrap().clone()})
            } else {
                *c.lock().unwrap() += 1;
                let mut x = r.lock().unwrap().clone();
                x["v"] = json!(1);
                x["op-id"] = req["op-id"].clone();
                x
            };
            let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
        }
    });
    (sock.to_str().unwrap().to_string(), boot, reply, count)
}

async fn up() -> (
    tempfile::TempDir,
    Handler,
    Shared,
    Shared,
    Arc<Mutex<usize>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let (sock, boot, reply, count) = worker(&dir);
    let h = Handler::new(config(&sock), Options::default())
        .await
        .unwrap();
    (dir, h, boot, reply, count)
}

fn err(r: (Option<contract::Result>, Option<contract::Error>)) -> contract::Error {
    r.1.expect("error frame")
}

fn start(h: &str) -> Value {
    json!({"mode": "start", "command": "shopctl", "args": ["version"], "handle": h})
}

#[tokio::test]
async fn a_handle_never_started_is_no_handle_and_the_worker_is_not_asked() {
    let (_d, h, _b, _r, count) = up().await;
    let e = err(run(&h, json!({"mode": "read", "handle": "h9"}), &[]).await);
    assert_eq!(e.reason, "no-handle");
    assert_eq!(Value::Object(e.details), json!({"handle": "h9"}));
    assert_eq!(*count.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_handle_lost_in_a_worker_restart_is_handle_lost_once() {
    let (_d, h, boot, _r, _c) = up().await;
    assert!(run(&h, start("h1"), &[]).await.1.is_none());
    *boot.lock().unwrap() = json!("boot-2");
    let e = err(run(&h, json!({"mode": "read", "handle": "h1"}), &[]).await);
    assert_eq!(e.reason, "handle-lost");
    assert_eq!(
        Value::Object(e.details),
        json!({"handle": "h1", "why": "worker-restarted"})
    );
    let e = err(run(&h, json!({"mode": "read", "handle": "h1"}), &[]).await);
    assert_eq!(e.reason, "no-handle", "once only");
}

#[tokio::test]
async fn the_worker_idle_word_is_handle_lost_with_the_idle_ms() {
    let (_d, h, _b, reply, _c) = up().await;
    assert!(run(&h, start("h1"), &[]).await.1.is_none());
    *reply.lock().unwrap() = json!({"ok": false, "reason": "no-handle", "why": "idle"});
    let e = err(run(&h, json!({"mode": "stop", "handle": "h1"}), &[]).await);
    assert_eq!(e.reason, "handle-lost");
    assert_eq!(e.details["handle"], "h1");
    assert_eq!(e.details["why"], "idle");
    assert_eq!(e.details["idle-ms"], 600_000);
    // The name is gone: the next read is a plain no-handle.
    let e = err(run(&h, json!({"mode": "read", "handle": "h1"}), &[]).await);
    assert_eq!(e.reason, "no-handle");
}

#[tokio::test]
async fn a_plain_no_handle_of_the_worker_stays_no_handle() {
    let (_d, h, _b, reply, _c) = up().await;
    assert!(run(&h, start("h1"), &[]).await.1.is_none());
    *reply.lock().unwrap() = json!({"ok": false, "reason": "no-handle"});
    let e = err(run(&h, json!({"mode": "read", "handle": "h1"}), &[]).await);
    assert_eq!(e.reason, "no-handle");
}

#[tokio::test]
async fn context_lost_run_closed_of_the_worker_passes_with_the_resource() {
    let (_d, h, _b, reply, _c) = up().await;
    *reply.lock().unwrap() =
        json!({"ok": false, "reason": "context-lost", "why": "run-closed", "idle-ms": 5});
    let e = err(run(&h, json!({"command": "shopctl", "args": ["version"]}), &[]).await);
    assert_eq!(e.reason, "context-lost");
    assert_eq!(
        Value::Object(e.details),
        json!({"resource": "shopctl", "why": "run-closed"})
    );
}
