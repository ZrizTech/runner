use crate::config::{Config, Resource};
use crate::config_cli::{self, CliCommand};
use crate::contract;
use crate::ops::tests::test_op;
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

type Seen = Arc<Mutex<Vec<Value>>>;
type Reply = Arc<Mutex<Value>>;
type Outcome = (Option<contract::Result>, Option<contract::Error>);

/// A fake worker: answers ping, and every other request with the current
/// `reply` (`{"ok":true,"out":..}` or `{"ok":false,"reason":..}`).
pub(super) fn fake_worker(
    dir: &tempfile::TempDir,
    name: &str,
    out: Value,
) -> (PathBuf, Seen, Reply) {
    let sock = dir.path().join(name);
    let listener = UnixListener::bind(&sock).expect("bind");
    let seen: Seen = Arc::default();
    let reply: Reply = Arc::new(Mutex::new(json!({"ok": true, "out": out})));
    let (log, rep) = (seen.clone(), reply.clone());
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
                let mut r = rep.lock().expect("lock").clone();
                r["v"] = json!(1);
                r["op-id"] = req["op-id"].clone();
                r
            };
            let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
        }
    });
    (sock, seen, reply)
}

pub(super) fn command() -> CliCommand {
    let shapes = [
        vec!["version"],
        vec!["init", "{dir:slug}"],
        vec!["run", "{file:relpath}", "--json"],
        vec!["login", "--no-browser"],
    ];
    CliCommand {
        path: "/opt/shopctl".into(),
        argv_prefix: vec![],
        shapes: shapes
            .iter()
            .map(|s| s.iter().map(|t| t.to_string()).collect())
            .collect(),
        env: [("NO_COLOR".to_string(), "1".to_string())].into(),
        env_allow: vec!["SHOPCTL_TOKEN".into()],
        timeout_ms: 60_000,
        max_life_ms: 660_000,
        max_output_bytes: 65_536,
    }
}

pub(super) fn config(socket: &str) -> Config {
    let mut resources = HashMap::new();
    resources.insert(
        "shopctl".to_string(),
        Resource {
            r#type: "cli".to_string(),
            secrets: Some(vec!["KEY".to_string()]),
            commands: [("shopctl".to_string(), command())].into(),
            max_handles: Some(2),
            ..Default::default()
        },
    );
    Config {
        resources,
        worker_socket: socket.to_string(),
        ..Default::default()
    }
}

async fn handler(socket: &str) -> Handler {
    let env: HashMap<&str, &str> = [("KEY", "key-secret-77"), ("OTHER", "other-secret")].into();
    let opts = Options {
        lookup: Some(Arc::new(move |n: &str| env.get(n).map(|v| v.to_string()))),
        ..Default::default()
    };
    Handler::new(config(socket), opts).await.expect("handler")
}

pub(super) async fn run(h: &Handler, args: Value, project: &[&[&str]]) -> Outcome {
    let args: HashMap<String, Value> = serde_json::from_value(args).expect("args");
    let project = project
        .iter()
        .map(|p| p.iter().map(|s| s.to_string()).collect())
        .collect();
    h.handle(test_op(
        "op", "run1", "cli.exec", "shopctl", 3000, args, project,
    ))
    .await
}

pub(super) fn reason(r: &Outcome) -> String {
    r.1.as_ref().map(|e| e.reason.clone()).unwrap_or_default()
}

async fn up() -> (tempfile::TempDir, Handler, Seen, Reply) {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, seen, reply) = fake_worker(&dir, "w.sock", json!({"exit-code": 0}));
    let h = handler(sock.to_str().expect("p")).await;
    (dir, h, seen, reply)
}

#[tokio::test]
async fn argv_shape_refusals() {
    let (_d, h, seen, _) = up().await;
    let bad: [&[&str]; 11] = [
        &[],
        &["init"],
        &["init", "a", "b"],
        &["init", ".."],
        &["init", "/x"],
        &["init", "A"],
        &["init", "-x"],
        &["run", "../x.json", "--json"],
        &["run", "/etc/x.json", "--json"],
        &["version", "--api-url"],
        &["sh", "-c", "id"],
    ];
    for argv in bad {
        let r = run(&h, json!({"command": "shopctl", "args": argv}), &[]).await;
        assert_eq!(reason(&r), "arg-not-allowed", "{argv:?}");
    }
    let r = run(&h, json!({"command": "shopctl", "args": [1]}), &[]).await;
    assert_eq!(reason(&r), "arg-not-allowed");
    assert!(seen.lock().expect("lock").is_empty());
    let good = run(
        &h,
        json!({"command": "shopctl", "args": ["init", "qa"]}),
        &[],
    )
    .await;
    assert!(good.1.is_none(), "{good:?}");
    let good = run(
        &h,
        json!({"command": "shopctl", "args": ["run", "qa/a.json", "--json"]}),
        &[],
    )
    .await;
    assert!(good.1.is_none(), "{good:?}");
}

#[tokio::test]
async fn unknown_command_and_closed_keys() {
    let (_d, h, seen, _) = up().await;
    for cmd in [json!("sh"), json!("SHOPCTL"), json!(7)] {
        let r = run(&h, json!({"command": cmd, "args": ["version"]}), &[]).await;
        assert_eq!(reason(&r), "command-not-allowed");
    }
    let r = run(&h, json!({"args": ["version"]}), &[]).await;
    assert_eq!(reason(&r), "command-not-allowed");
    let r = run(
        &h,
        json!({"command": "shopctl", "args": ["version"], "cwd": "/"}),
        &[],
    )
    .await;
    assert_eq!(reason(&r), "unknown-arg");
    let r = run(
        &h,
        json!({"command": "shopctl", "args": ["version"], "mode": "fork"}),
        &[],
    )
    .await;
    assert_eq!(reason(&r), "arg-not-allowed");
    assert!(seen.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn placeholders_only_in_env_for_allowed_names() {
    let (_d, h, seen, _) = up().await;
    let cases = [
        json!({"command": "shopctl", "args": ["init", "${KEY}"]}),
        json!({"command": "shopctl", "args": ["version"], "until": {"stream": "stdout", "after": "${KEY}"}}),
        json!({"command": "shopctl", "args": ["version"], "env": {"SHOPCTL_TOKEN": "${OTHER}"}}),
    ];
    for args in cases {
        let r = run(&h, args, &[]).await;
        assert_eq!(reason(&r), "placeholder-in-disallowed-slot");
    }
    let r = run(
        &h,
        json!({"command": "shopctl", "args": ["version"], "env": {"PATH": "/x"}}),
        &[],
    )
    .await;
    assert_eq!(reason(&r), "arg-not-allowed");
    assert!(seen.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn env_secret_reaches_worker_but_is_scrubbed() {
    let dir = tempfile::tempdir().expect("dir");
    let out = json!({"exit-code": 0, "stdout": "key is key-secret-77 ok"});
    let (sock, seen, _) = fake_worker(&dir, "w.sock", out);
    let h = handler(sock.to_str().expect("p")).await;
    let args = json!({"command": "shopctl", "args": ["version"], "env": {"SHOPCTL_TOKEN": "${KEY}"}, "stdout": "text"});
    let (res, err) = run(&h, args, &[&["exit-code"], &["stdout"]]).await;
    assert!(err.is_none(), "{err:?}");
    let res = res.expect("result");
    assert_eq!(res.status, "pass");
    let text = serde_json::to_string(&res.payload).expect("json");
    assert!(!text.contains("key-secret-77"), "{text}");
    assert!(res.scrubbed >= 1);
    let sent = seen.lock().expect("lock")[0].clone();
    assert_eq!(sent["kind"], "cli.exec");
    assert_eq!(sent["args"]["env"]["SHOPCTL_TOKEN"], "key-secret-77");
    assert_eq!(sent["args"]["mode"], "run");
    assert_eq!(sent["args"]["args"], json!(["version"]));
    for key in ["command", "stdout", "capture"] {
        assert!(
            sent["args"].get(key).is_none(),
            "{key} leaked to the worker"
        );
    }
    assert_eq!(sent["policy"]["command"], "shopctl");
    assert_eq!(sent["policy"]["path"], "/opt/shopctl");
    assert_eq!(sent["policy"]["env"], json!({"NO_COLOR": "1"}));
    assert_eq!(sent["policy"]["cwd"], "run");
    assert_eq!(sent["policy"]["max-handles"], 2);
}

#[tokio::test]
async fn extract_goes_to_vault_and_is_masked() {
    let dir = tempfile::tempdir().expect("dir");
    let out = json!({"running": true, "extract": {"user-code": "ABCD-1234"}});
    let (sock, seen, _) = fake_worker(&dir, "w.sock", out);
    let h = handler(sock.to_str().expect("p")).await;
    let r = run(&h, json!({"mode": "start", "command": "shopctl", "args": ["login", "--no-browser"], "handle": "login"}), &[]).await;
    assert!(r.1.is_none(), "{r:?}");
    let args = json!({
        "mode": "read", "handle": "login",
        "until": {"stream": "stderr", "after": " and enter "},
        "extract": {"user-code": {"stream": "stderr", "after": " and enter "}},
        "capture": {"CODE": ["extract", "user-code"]},
    });
    let (res, err) = run(&h, args, &[&["running"], &["extract", "user-code"]]).await;
    assert!(err.is_none(), "{err:?}");
    let res = res.expect("result");
    assert_eq!(res.payload["extract"]["user-code"], "[captured]");
    assert_eq!(res.payload["running"], true);
    // No `command` on a read: the one remembered from `start` is used.
    assert_eq!(
        seen.lock().expect("lock")[1]["policy"]["command"],
        "shopctl"
    );
    // A later op resolves ${CODE} from the vault (not in `secrets`).
    let args =
        json!({"command": "shopctl", "args": ["version"], "env": {"SHOPCTL_TOKEN": "${CODE}"}});
    let (_, err) = run(&h, args, &[]).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(
        seen.lock().expect("lock")[2]["args"]["env"]["SHOPCTL_TOKEN"],
        "ABCD-1234"
    );
}

#[tokio::test]
async fn read_wait_stop_take_no_argv() {
    let (_d, h, seen, _) = up().await;
    let r = run(
        &h,
        json!({"mode": "wait", "handle": "login", "args": ["version"]}),
        &[],
    )
    .await;
    assert_eq!(reason(&r), "arg-not-allowed");
    let r = run(&h, json!({"mode": "wait"}), &[]).await;
    assert_eq!(reason(&r), "arg-not-allowed");
    assert!(seen.lock().expect("lock").is_empty());
    let r = run(&h, json!({"mode": "start", "command": "shopctl", "args": ["login", "--no-browser"], "handle": "login"}), &[]).await;
    assert!(r.1.is_none(), "{r:?}");
    let r = run(
        &h,
        json!({"mode": "wait", "handle": "login"}),
        &[&["exit-code"]],
    )
    .await;
    assert_eq!(r.0.expect("result").payload["exit-code"], 0);
}

#[tokio::test]
async fn stdout_json_is_parsed() {
    let dir = tempfile::tempdir().expect("dir");
    let out = json!({"exit-code": 0, "stdout": "{\"status\":\"passed\",\"n\":2}"});
    let (sock, _, reply) = fake_worker(&dir, "w.sock", out);
    let h = handler(sock.to_str().expect("p")).await;
    let args = json!({"command": "shopctl", "args": ["version"], "stdout": "json"});
    let proj: &[&[&str]] = &[&["json", "status"], &["json-invalid"]];
    let (res, err) = run(&h, args.clone(), proj).await;
    assert!(err.is_none(), "{err:?}");
    let res = res.expect("result");
    assert_eq!(res.payload["json"]["status"], "passed");
    assert_eq!(res.payload["json-invalid"], false);
    *reply.lock().expect("lock") =
        json!({"ok": true, "out": {"exit-code": 1, "stdout": "not json"}});
    let (res, _) = run(&h, args, &[&["json"], &["json-invalid"]]).await;
    let res = res.expect("result");
    assert_eq!(res.payload["json"], Value::Null);
    assert_eq!(res.payload["json-invalid"], true);
}

#[tokio::test]
async fn worker_reasons_are_mapped() {
    let (_d, h, _, reply) = up().await;
    let table = [
        ("handle-busy", "runner-error"),
        ("no-handle", "runner-error"),
        ("too-many-handles", "runner-error"),
        ("spawn-failed", "runner-error"),
        ("at-capacity", "runner-at-capacity"),
        ("internal", "runner-error"),
    ];
    for (worker_reason, want) in table {
        *reply.lock().expect("lock") =
            json!({"ok": false, "reason": worker_reason, "message": "free text /opt/shopctl"});
        let r = run(&h, json!({"command": "shopctl", "args": ["version"]}), &[]).await;
        assert_eq!(reason(&r), want, "{worker_reason}");
        let msg = r.1.expect("err").message;
        assert!(!msg.contains("free text"), "{msg}");
        if want == "runner-error" && worker_reason != "internal" {
            assert!(msg.contains(worker_reason), "{msg}");
        }
    }
}

#[tokio::test]
async fn cli_announced_only_while_worker_up() {
    let dir = tempfile::tempdir().expect("dir");
    let down = handler(dir.path().join("late.sock").to_str().expect("p")).await;
    assert_eq!(down.op_kinds().len(), 3);
    let configured = vec!["shopctl".to_string(), "other".to_string()];
    assert_eq!(down.announced_resources(&configured), vec!["other"]);
    let r = run(
        &down,
        json!({"command": "shopctl", "args": ["version"]}),
        &[],
    )
    .await;
    assert_eq!(reason(&r), "worker-unavailable");
    let (_sock, _, _) = fake_worker(&dir, "late.sock", json!({}));
    *down.last_ping.lock().expect("lock") = None;
    down.refresh_worker().await;
    assert!(down.op_kinds().contains(&"cli.exec".to_string()));
    assert!(!down.op_kinds().contains(&"browser.page".to_string()));
    assert_eq!(down.announced_resources(&configured), configured);
}

#[test]
fn config_validation() {
    let mut r = Resource {
        r#type: "cli".into(),
        commands: [("shopctl".to_string(), command())].into(),
        ..Default::default()
    };
    assert!(config_cli::validate("shopctl", &r).is_ok());
    let breakers: [fn(&mut CliCommand); 8] = [
        |c| c.path = "shopctl".into(),
        |c| c.path = "/a/../b".into(),
        |c| c.shapes = vec![vec!["x".into(), "{d:regex}".into()]],
        |c| c.shapes = vec![],
        |c| c.timeout_ms = 0,
        |c| c.max_output_bytes = 2_000_000,
        |c| c.env_allow = vec!["NO_COLOR".into()],
        |c| c.env_allow = vec!["a-b".into()],
    ];
    for f in breakers {
        let mut c = command();
        f(&mut c);
        r.commands = [("shopctl".to_string(), c)].into();
        assert!(config_cli::validate("shopctl", &r).is_err());
    }
    r.commands = [("a b".to_string(), command())].into();
    assert!(config_cli::validate("shopctl", &r).is_err());
    r.commands.clear();
    assert!(config_cli::validate("shopctl", &r).is_err());
}
