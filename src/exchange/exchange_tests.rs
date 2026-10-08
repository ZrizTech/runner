//! The core `run` behaviors against a fake cloud that
//! answers each request immediately (no long-poll hold).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn base_config(handler: Arc<dyn Handler>) -> Config {
    let mut cfg = Config::new("http://placeholder", TEST_TOKEN, "runner-1", handler);
    cfg.resources = vec!["shop-api".to_string()];
    cfg.backoff = Duration::from_millis(5);
    cfg.max_backoff = Duration::from_millis(20);
    cfg
}

fn fake_sleeper() -> (SleepFn, Arc<Mutex<Vec<Duration>>>) {
    let durations: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let d2 = Arc::clone(&durations);
    let sleep: SleepFn = Arc::new(move |_cancel: CancellationToken, d: Duration| {
        let durations = Arc::clone(&d2);
        Box::pin(async move {
            durations.lock().unwrap_or_else(|e| e.into_inner()).push(d);
            true
        })
    });
    (sleep, durations)
}

async fn wait_for_sleep_count(durations: &Arc<Mutex<Vec<Duration>>>, n: usize) -> Vec<Duration> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            {
                let got = durations.lock().unwrap_or_else(|e| e.into_inner());
                if got.len() >= n {
                    return got.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {n} recorded sleeps"))
}

use std::sync::Mutex;

#[tokio::test(flavor = "multi_thread")]
async fn first_request_reports_env_and_ops() {
    for env in ["", "prod"] {
        let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(3));
        let cloud = ImmediateCloud::new(|_n, _req| (204, None)).await;
        let mut rx = cloud.take_receiver();

        let mut cfg = base_config(handler);
        cfg.cloud_url = cloud.base_url.clone();
        cfg.env = env.to_string();

        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let done = tokio::spawn(async move { run(cfg, cancel2).await });

        let first = wait_for(&mut rx, |a| a.n == 0).await;
        cancel.cancel();
        assert_eq!(done.await.expect("join"), Ok(()));

        assert_eq!(first.auth, format!("Bearer {TEST_TOKEN}"));
        assert_eq!(first.req.inflight, 0);
        assert!(first.req.frames.is_empty());
        assert_eq!(first.req.runner.env, env);
        assert_eq!(
            first.req.runner.ops,
            vec!["http.request", "sql.query", "evidence.fetch"]
        );
        assert_eq!(first.req.runner.max_inflight, 3);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn op_round_trip_delivers_result() {
    let op_frame = build_op_frame("frame-op-1", "op-1");
    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler {
        received: Some(received_tx),
        ..FakeHandler::with_result(
            4,
            contract::Result {
                op_id: "op-1".to_string(),
                status: "pass".to_string(),
                payload: [("foo".to_string(), serde_json::json!("bar"))]
                    .into_iter()
                    .collect(),
                scrubbed: 0,
                timing: contract::Timing { exec_ms: 1 },
            },
        )
    });

    let op_frame_id = op_frame.id.clone();
    let cloud = ImmediateCloud::new(move |n, _req| {
        if n == 1 {
            (200, Some(exchange_response_body(vec![op_frame.clone()])))
        } else {
            (204, None)
        }
    })
    .await;
    let mut rx = cloud.take_receiver();

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let op = recv_timeout(&mut received_rx).await;
    assert_eq!(op.kind, "http.request");
    assert_eq!(op.resource, "shop-api");
    assert_eq!(op.args.get("method").and_then(|v| v.as_str()), Some("GET"));

    let arrival = wait_for(&mut rx, |a| {
        find_frame(&a.req.frames, "result")
            .map(|f| f.re.as_deref() == Some(op_frame_id.as_str()))
            .unwrap_or(false)
    })
    .await;
    let f = find_frame(&arrival.req.frames, "result").expect("result frame");
    assert_eq!(f.v, 1);
    assert!(!f.id.is_empty());
    let result: contract::Result = serde_json::from_value(f.d).expect("decode result");
    assert_eq!(result.op_id, "op-1");

    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn error_reply_carries_reason() {
    let op_frame = build_op_frame("frame-op-2", "op-1");
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::with_error(
        4,
        contract::Error {
            op_id: "op-1".to_string(),
            reason: "timeout".to_string(),
            message: "boom".to_string(),
        },
    ));

    let op_frame_id = op_frame.id.clone();
    let cloud = ImmediateCloud::new(move |n, _req| {
        if n == 1 {
            (200, Some(exchange_response_body(vec![op_frame.clone()])))
        } else {
            (204, None)
        }
    })
    .await;
    let mut rx = cloud.take_receiver();

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let arrival = wait_for(&mut rx, |a| {
        find_frame(&a.req.frames, "error")
            .map(|f| f.re.as_deref() == Some(op_frame_id.as_str()))
            .unwrap_or(false)
    })
    .await;
    let f = find_frame(&arrival.req.frames, "error").expect("error frame");
    let op_err: contract::Error = serde_json::from_value(f.d).expect("decode error");
    assert_eq!(op_err.reason, "timeout");

    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn panicking_handler_replies_runner_error() {
    let op_frame = build_op_frame("frame-op-3", "op-1");
    let handler: Arc<dyn Handler> = Arc::new(PanickingHandler { max_inflight: 4 });

    let op_frame_id = op_frame.id.clone();
    let cloud = ImmediateCloud::new(move |n, _req| {
        if n == 1 {
            (200, Some(exchange_response_body(vec![op_frame.clone()])))
        } else {
            (204, None)
        }
    })
    .await;
    let mut rx = cloud.take_receiver();

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let arrival = wait_for(&mut rx, |a| {
        find_frame(&a.req.frames, "error")
            .map(|f| f.re.as_deref() == Some(op_frame_id.as_str()))
            .unwrap_or(false)
    })
    .await;
    let f = find_frame(&arrival.req.frames, "error").expect("error frame");
    let op_err: contract::Error = serde_json::from_value(f.d).expect("decode error");
    assert_eq!(op_err.op_id, "op-1");
    assert_eq!(op_err.reason, "runner-error");
    assert_eq!(op_err.message, "op handler panicked");

    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_fails_closed_with_no_token_source() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let cloud = ImmediateCloud::new(|_n, _req| (401, None)).await;

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let done = run(cfg, CancellationToken::new()).await;
    assert_eq!(done, Err(ExchangeError::Unauthorized));
    assert_eq!(cloud.request_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pool_full_rejects_with_capacity_error() {
    let op1 = build_op_frame("frame-op-1", "op-1");
    let op2 = build_op_frame("frame-op-2", "op-2");
    let (block_tx, block_rx) = tokio::sync::watch::channel(false);
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler {
        block: Some(block_rx),
        ..FakeHandler::with_result(
            1,
            contract::Result {
                op_id: "op-1".to_string(),
                status: "pass".to_string(),
                payload: std::collections::BTreeMap::new(),
                scrubbed: 0,
                timing: contract::Timing { exec_ms: 1 },
            },
        )
    });

    let op1_id = op1.id.clone();
    let op2_id = op2.id.clone();
    let cloud = ImmediateCloud::new(move |n, _req| {
        if n == 0 {
            (
                200,
                Some(exchange_response_body(vec![op1.clone(), op2.clone()])),
            )
        } else {
            (204, None)
        }
    })
    .await;
    let mut rx = cloud.take_receiver();

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let capacity = wait_for(&mut rx, |a| {
        find_frame(&a.req.frames, "error")
            .map(|f| f.re.as_deref() == Some(op2_id.as_str()))
            .unwrap_or(false)
    })
    .await;
    let f = find_frame(&capacity.req.frames, "error").expect("capacity error frame");
    let op_err: contract::Error = serde_json::from_value(f.d).expect("decode");
    assert_eq!(op_err.reason, "runner-at-capacity");
    assert_eq!(capacity.req.inflight, 1);

    let _ = block_tx.send(true);

    wait_for(&mut rx, |a| {
        find_frame(&a.req.frames, "result")
            .map(|f| f.re.as_deref() == Some(op1_id.as_str()))
            .unwrap_or(false)
    })
    .await;

    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn backoff_on_transport_error_keeps_looping() {
    let op_frame = build_op_frame("frame-op-1", "op-1");
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::with_result(
        4,
        contract::Result {
            op_id: "op-1".to_string(),
            status: "pass".to_string(),
            payload: std::collections::BTreeMap::new(),
            scrubbed: 0,
            timing: contract::Timing { exec_ms: 1 },
        },
    ));

    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failed2 = Arc::clone(&failed);
    let op_frame2 = op_frame.clone();
    let cloud = ImmediateCloud::new(move |n, req| {
        if n == 0 {
            return (200, Some(exchange_response_body(vec![op_frame2.clone()])));
        }
        if !failed2.load(Ordering::SeqCst) && find_frame(&req.frames, "result").is_some() {
            failed2.store(true, Ordering::SeqCst);
            return (500, None);
        }
        (204, None)
    })
    .await;
    let mut rx = cloud.take_receiver();

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    wait_for(&mut rx, |a| find_frame(&a.req.frames, "result").is_some()).await;
    wait_for(&mut rx, |a| find_frame(&a.req.frames, "result").is_some()).await;

    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn unparsable_frame_is_skipped_and_loop_continues() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(4));
    let bad_body =
        br#"{"v":1,"frames":[{"v":1,"t":"op","id":"x","re":null,"ts":1,"d":{"op-id":"bad"}}]}"#
            .to_vec();

    let cloud = ImmediateCloud::new(move |n, _req| {
        if n == 0 {
            (200, Some(bad_body.clone()))
        } else {
            (204, None)
        }
    })
    .await;
    let mut rx = cloud.take_receiver();

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    wait_for(&mut rx, |a| a.n == 1).await;

    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
    assert!(cloud.request_count() >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn backoff_doubles_on_repeated_failure() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let cloud = ImmediateCloud::new(|_n, _req| (500, None)).await;

    let (sleep, durations) = fake_sleeper();
    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();
    cfg.backoff = Duration::from_millis(10);
    cfg.max_backoff = Duration::from_millis(80);
    cfg.sleep = Some(sleep);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let got = wait_for_sleep_count(&durations, 3).await;
    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));

    let want = [
        Duration::from_millis(10),
        Duration::from_millis(20),
        Duration::from_millis(40),
    ];
    for (i, w) in want.iter().enumerate() {
        assert_eq!(got[i], *w, "sleep[{i}] (full: {got:?})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn backoff_caps_at_max_backoff() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let cloud = ImmediateCloud::new(|_n, _req| (500, None)).await;

    let (sleep, durations) = fake_sleeper();
    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();
    cfg.backoff = Duration::from_millis(10);
    cfg.max_backoff = Duration::from_millis(25);
    cfg.sleep = Some(sleep);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let got = wait_for_sleep_count(&durations, 4).await;
    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));

    let want = [
        Duration::from_millis(10),
        Duration::from_millis(20),
        Duration::from_millis(25),
        Duration::from_millis(25),
    ];
    for (i, w) in want.iter().enumerate() {
        assert_eq!(got[i], *w, "sleep[{i}] (full: {got:?})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn on_connected_fires_once_after_first_200() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let call_count = Arc::new(AtomicI64::new(0));
    let fired_after = Arc::new(AtomicI64::new(0));

    let cloud = ImmediateCloud::new(|n, _req| {
        if n == 0 {
            (500, None)
        } else {
            (200, Some(exchange_response_body(vec![])))
        }
    })
    .await;
    let mut rx = cloud.take_receiver();

    let call_count2 = Arc::clone(&call_count);
    let fired_after2 = Arc::clone(&fired_after);
    let cloud_count_handle = cloud.request_count_handle();
    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();
    cfg.on_connected = Some(Arc::new(move || {
        call_count2.fetch_add(1, Ordering::SeqCst);
        fired_after2.store(cloud_count_handle() as i64, Ordering::SeqCst);
    }));

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    wait_for(&mut rx, |a| a.n == 3).await;
    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert!(
        fired_after.load(Ordering::SeqCst) >= 2,
        "OnConnected must fire only after the first 200, not the earlier 500"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn on_connected_nil_is_fine() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let cloud = ImmediateCloud::new(|_n, _req| (204, None)).await;
    let mut rx = cloud.take_receiver();

    let mut cfg = base_config(handler);
    cfg.cloud_url = cloud.base_url.clone();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    wait_for(&mut rx, |a| a.n == 0).await;
    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_refreshes_token_once() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let (auth_tx, mut auth_rx) = tokio::sync::mpsc::unbounded_channel();
    let source_calls = Arc::new(AtomicI64::new(0));
    let source_calls2 = Arc::clone(&source_calls);

    let handler_ac: RawAuthCloud = RawAuthCloud::new(move |auth| {
        let _ = auth_tx.send(auth.to_string());
        if auth == "Bearer old-token" {
            (401, None)
        } else {
            (204, None)
        }
    })
    .await;

    let mut cfg = Config::new(
        handler_ac.base_url.clone(),
        "old-token",
        "runner-1",
        handler,
    );
    cfg.backoff = Duration::from_millis(5);
    cfg.max_backoff = Duration::from_millis(20);
    cfg.token_source = Some(Arc::new(move || {
        source_calls2.fetch_add(1, Ordering::SeqCst);
        Ok("new-token".to_string())
    }));

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let first = recv_timeout(&mut auth_rx).await;
    let second = recv_timeout(&mut auth_rx).await;
    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));

    assert_eq!(first, "Bearer old-token");
    assert_eq!(second, "Bearer new-token");
    assert_eq!(source_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_twice_fails_closed() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let (auth_tx, mut auth_rx) = tokio::sync::mpsc::unbounded_channel();
    let source_calls = Arc::new(AtomicI64::new(0));
    let source_calls2 = Arc::clone(&source_calls);

    let cloud = RawAuthCloud::new(move |auth| {
        let _ = auth_tx.send(auth.to_string());
        (401, None)
    })
    .await;

    let mut cfg = Config::new(cloud.base_url.clone(), "old-token", "runner-1", handler);
    cfg.backoff = Duration::from_millis(5);
    cfg.max_backoff = Duration::from_millis(20);
    cfg.token_source = Some(Arc::new(move || {
        source_calls2.fetch_add(1, Ordering::SeqCst);
        Ok("new-token".to_string())
    }));

    let done = run(cfg, CancellationToken::new()).await;
    assert_eq!(done, Err(ExchangeError::Unauthorized));

    let first = recv_timeout(&mut auth_rx).await;
    let second = recv_timeout(&mut auth_rx).await;
    assert_eq!(first, "Bearer old-token");
    assert_eq!(second, "Bearer new-token");
    assert_eq!(source_calls.load(Ordering::SeqCst), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), auth_rx.recv())
            .await
            .is_err(),
        "want exactly 2 requests (one retry, then fail closed)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_token_source_error_fails_closed_at_once() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let (auth_tx, mut auth_rx) = tokio::sync::mpsc::unbounded_channel();

    let cloud = RawAuthCloud::new(move |auth| {
        let _ = auth_tx.send(auth.to_string());
        (401, None)
    })
    .await;

    let mut cfg = Config::new(cloud.base_url.clone(), "old-token", "runner-1", handler);
    cfg.backoff = Duration::from_millis(5);
    cfg.max_backoff = Duration::from_millis(20);
    cfg.token_source = Some(Arc::new(|| Err("token source: boom".to_string())));

    let done = run(cfg, CancellationToken::new()).await;
    assert_eq!(done, Err(ExchangeError::Unauthorized));

    let first = recv_timeout(&mut auth_rx).await;
    assert_eq!(first, "Bearer old-token");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), auth_rx.recv())
            .await
            .is_err(),
        "want exactly 1 request: TokenSource failed, no retry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_retry_cancelled_during_backoff_shuts_down_clean() {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let cloud = RawAuthCloud::new(|_auth| (401, None)).await;

    let mut cfg = Config::new(cloud.base_url.clone(), "old-token", "runner-1", handler);
    cfg.backoff = Duration::from_millis(5);
    cfg.max_backoff = Duration::from_millis(20);
    cfg.token_source = Some(Arc::new(|| Ok("new-token".to_string())));
    cfg.sleep = Some(Arc::new(|_cancel, _d| Box::pin(async { false })));

    let done = run(cfg, CancellationToken::new()).await;
    assert_eq!(done, Ok(()));
}
