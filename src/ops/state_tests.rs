//! The state a run holds on the runner: no idle limit, the 1000-entry
//! limit with one `state dropped` line, the run-end notice, the health numbers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::jar::{JarKey, MAX_JARS};
use crate::config::{Config, Resource};
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl MakeWriter<'_> for Buf {
    type Writer = Buf;
    fn make_writer(&self) -> Buf {
        self.clone()
    }
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn key(run: &str) -> JarKey {
    (run.to_string(), "web".to_string())
}

fn sid() -> Vec<String> {
    vec!["sid=abc; Path=/".to_string()]
}

fn page() -> url::Url {
    url::Url::parse("https://app.example.com/me").unwrap()
}

#[test]
fn jar_and_vault_keep_an_entry_after_31_minutes() {
    let jars = super::jar::Jars::default();
    jars.store(&key("run-a"), &sid(), at(1_000));
    let later = at(1_000 + 31 * 60);
    assert_eq!(
        jars.cookie_header(&key("run-a"), &page(), later).as_deref(),
        Some("sid=abc")
    );
    let vault = super::vault::Vault::default();
    vault.put("run-a", &[("T".to_string(), "v".to_string())], at(1_000));
    assert_eq!(vault.get("run-a", "T", later).as_deref(), Some("v"));
}

#[test]
fn entry_1001_drops_the_longest_idle_with_one_line() {
    let _pin = tracing::Dispatch::new(tracing_subscriber::registry());
    let buf = Buf::default();
    let (sub, _logging) = crate::logfmt::subscriber_with_writer("info", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);

    let jars = super::jar::Jars::default();
    let vault = super::vault::Vault::default();
    for i in 0..MAX_JARS {
        let run = format!("run-{i}");
        jars.store(&key(&run), &sid(), at(1_000 + i as u64));
        vault.put(
            &run,
            &[("T".to_string(), "v".to_string())],
            at(1_000 + i as u64),
        );
    }
    // run-0 is the longest idle; use run-1 once so it is not the oldest after run-0 goes.
    jars.cookie_header(&key("run-1"), &page(), at(5_000));
    assert_eq!(jars.len(), MAX_JARS);
    jars.store(&key("run-new"), &sid(), at(6_000));
    vault.put("run-new", &[("T".to_string(), "v".to_string())], at(6_000));
    assert_eq!(jars.len(), MAX_JARS);
    assert_eq!(vault.run_count(), MAX_JARS);
    assert!(
        jars.cookie_header(&key("run-0"), &page(), at(6_001))
            .is_none()
    );
    assert!(
        jars.cookie_header(&key("run-1"), &page(), at(6_001))
            .is_some()
    );
    assert!(vault.get("run-0", "T", at(6_001)).is_none());
    assert!(vault.get("run-new", "T", at(6_001)).is_some());

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    for kind in ["jar", "vault"] {
        let want = format!("state dropped run_id=run-0 kind={kind}");
        assert_eq!(out.matches(&want).count(), 1, "{out}");
    }
    assert_eq!(out.matches("state dropped").count(), 2, "{out}");
    assert!(out.contains(" WARN "), "{out}");
}

type Seen = Arc<Mutex<Vec<Value>>>;

/// A fake worker: ping answers with `ping`, `run.close` is recorded.
fn fake_worker(dir: &tempfile::TempDir, ping: Value) -> (String, Seen) {
    let sock = dir.path().join("w.sock");
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
                ping.clone()
            } else {
                log.lock().unwrap().push(req.clone());
                json!({"v": 1, "kind": "run.close", "ok": true, "closed": 1})
            };
            let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
        }
    });
    (sock.to_string_lossy().into_owned(), seen)
}

fn worker_config(socket: &str) -> Config {
    let res = |ty: &str, max_contexts: Option<u32>| Resource {
        r#type: ty.to_string(),
        base_url: "https://app.example.com".to_string(),
        max_contexts,
        ..Default::default()
    };
    let resources = HashMap::from([
        ("web".to_string(), res("browser", Some(8))),
        ("shell".to_string(), res("cli", None)),
    ]);
    Config {
        resources,
        worker_socket: socket.to_string(),
        ..Default::default()
    }
}

fn evidence_entry() -> crate::evidence::Entry {
    crate::evidence::Entry {
        op_id: "op-1".to_string(),
        status: 200,
        body: json!({"a": 1}),
        secrets: vec![],
    }
}

#[tokio::test]
async fn run_end_frees_each_resource_of_that_run_only() {
    let dir = tempfile::tempdir().unwrap();
    let ping = json!({"v": 1, "kind": "ping", "ok": true});
    let (sock, seen) = fake_worker(&dir, ping);
    let h = Handler::new(worker_config(&sock), Options::default())
        .await
        .unwrap();
    let now = at(1_000);
    for run in ["run-a", "run-b"] {
        h.jars.store(&key(run), &sid(), now);
        h.vault.put(run, &[("T".to_string(), "v".to_string())], now);
        h.handles.put(run, "shell", "h1", "cmd");
        h.evidence.put(run, evidence_entry(), now);
    }
    h.run_ended("run-a", "5d0c9a52-7c1e-4b1f-9a55-0d6f3a2b8c11")
        .await;

    assert!(h.jars.cookie_header(&key("run-a"), &page(), now).is_none());
    assert!(h.vault.get("run-a", "T", now).is_none());
    assert!(h.handles.get("run-a", "shell", "h1").is_none());
    assert!(h.evidence.get("run-a", now).is_none());
    assert!(h.jars.cookie_header(&key("run-b"), &page(), now).is_some());
    assert!(h.vault.get("run-b", "T", now).is_some());
    assert!(h.handles.get("run-b", "shell", "h1").is_some());
    assert!(h.evidence.get("run-b", now).is_some());

    let seen = seen.lock().unwrap();
    assert_eq!(
        *seen,
        vec![
            json!({"v": 1, "kind": "run.close", "run": "run-a", "trace-id": "5d0c9a52-7c1e-4b1f-9a55-0d6f3a2b8c11"})
        ]
    );
}

#[tokio::test]
async fn run_end_with_the_worker_down_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("none.sock").to_string_lossy().into_owned();
    let h = Handler::new(worker_config(&sock), Options::default())
        .await
        .unwrap();
    h.vault
        .put("run-a", &[("T".to_string(), "v".to_string())], at(1));
    h.run_ended("run-a", "5d0c9a52-7c1e-4b1f-9a55-0d6f3a2b8c11")
        .await;
    assert_eq!(h.vault.run_count(), 0);
}

#[tokio::test]
async fn health_numbers_come_from_the_ping_and_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let ping = json!({"v": 1, "kind": "ping", "ok": true,
        "browser": [{"resource": "web", "busy": 8}], "cli": {"busy": 1, "limit": 16}, "boot-id": "b-aaaaaaaaaaaa"});
    let (sock, _) = fake_worker(&dir, ping);
    let h = Handler::new(worker_config(&sock), Options::default())
        .await
        .unwrap();
    let before = h.worker_health();
    assert!(before.up && before.needed);
    assert_eq!(before.browser[0].busy, 0, "no ping result yet");
    assert!(before.cli.is_none(), "no cli numbers without a ping");
    *h.last_ping.lock().unwrap() = None;
    h.refresh_worker().await;
    let w = h.worker_health();
    assert_eq!(
        (
            w.browser[0].resource.as_str(),
            w.browser[0].busy,
            w.browser[0].limit
        ),
        ("web", 8, 8)
    );
    let cli = w.cli.unwrap();
    assert_eq!((cli.busy, cli.limit), (1, 16));
}

#[test]
fn handle_names_have_no_idle_limit_and_the_1001st_drops_the_oldest_with_one_line() {
    let _pin = tracing::Dispatch::new(tracing_subscriber::registry());
    let buf = Buf::default();
    let (sub, _logging) = crate::logfmt::subscriber_with_writer("info", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);
    let handles = super::handles::Handles::default();
    for i in 0..super::handles::MAX_HANDLES {
        handles.put(&format!("run-{i}"), "shell", "h1", "cmd");
    }
    handles.put("run-new", "shell", "h1", "cmd");
    assert!(handles.get("run-0", "shell", "h1").is_none());
    assert_eq!(handles.get("run-1", "shell", "h1").as_deref(), Some("cmd"));
    assert_eq!(handles.get("run-new", "shell", "h1").as_deref(), Some("cmd"));
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert_eq!(out.matches("state dropped").count(), 1, "{out}");
    assert!(out.contains("state dropped run_id=run-0 kind=handles"), "{out}");
}
