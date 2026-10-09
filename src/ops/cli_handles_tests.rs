use super::tests::{command, config, fake_worker, reason, run};
use crate::ops::{Handler, Options};
use serde_json::json;

#[tokio::test]
async fn handle_modes_use_the_command_of_the_start() {
    let dir = tempfile::tempdir().expect("dir");
    let (sock, seen, _) = fake_worker(&dir, "w.sock", json!({"exit-code": 0}));
    let mut cfg = config(sock.to_str().expect("p"));
    let res = cfg.resources.get_mut("shopctl").expect("shopctl");
    let mut other = command();
    other.path = "/opt/other".into();
    let mut third = command();
    third.path = "/opt/third".into();
    res.commands.insert("other".into(), other);
    res.commands.insert("third".into(), third);
    let h = Handler::new(cfg, Options::default())
        .await
        .expect("handler");
    let start = json!({"mode": "start", "command": "other", "args": ["version"], "handle": "h1"});
    assert!(run(&h, start, &[]).await.1.is_none());
    let r = run(&h, json!({"mode": "read", "handle": "h1"}), &[]).await;
    assert!(r.1.is_none(), "{r:?}");
    let s = seen.lock().expect("lock").clone();
    assert_eq!(s[1]["policy"]["command"], "other");
    assert_eq!(s[1]["policy"]["path"], "/opt/other");
    // A command that differs from the remembered one is refused.
    let bad = json!({"mode": "stop", "handle": "h1", "command": "third"});
    assert_eq!(reason(&run(&h, bad, &[]).await), "arg-not-allowed");
    let same = json!({"mode": "read", "handle": "h1", "command": "other"});
    assert!(run(&h, same, &[]).await.1.is_none());
    // A finished wait forgets the handle, and so does stop.
    let r = run(&h, json!({"mode": "wait", "handle": "h1"}), &[]).await;
    assert!(r.1.is_none(), "{r:?}");
    let r = run(&h, json!({"mode": "read", "handle": "h1"}), &[]).await;
    assert_eq!(reason(&r), "no-handle");
    let n = seen.lock().expect("lock").len();
    let start = json!({"mode": "start", "command": "third", "args": ["version"], "handle": "h2"});
    assert!(run(&h, start, &[]).await.1.is_none());
    let r = run(&h, json!({"mode": "stop", "handle": "h2"}), &[]).await;
    assert!(r.1.is_none(), "{r:?}");
    let r = run(&h, json!({"mode": "stop", "handle": "h2"}), &[]).await;
    assert_eq!(reason(&r), "no-handle");
    // Only the start and the first stop reached the worker.
    assert_eq!(seen.lock().expect("lock").len(), n + 2);
}
