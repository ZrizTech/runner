//! Conformance tests: drives `run` against [`SlotCloud`], a
//! fake cloud with the real slot semantics (immediate first exchange, hold,
//! drain by max-inflight, late frame dropped, restart with a re-issued op,
//! an outage), pinning exactly-once answers, bounded backoff and one
//! `on_connected`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The slot cloud's hold length for these tests: short enough to keep the
/// suite fast, long enough (relative to test-side scheduling) that an op
/// enqueued right after a hold starts reliably beats the timeout instead of
/// racing it.
const CONFORMANCE_HOLD: Duration = Duration::from_millis(300);

/// Starts `run` in the background against `cfg` and returns a guard that,
/// on drop... Rust has no `t.Cleanup`, so callers `.cancel_and_join(cancel,
/// done).await` explicitly at the end of the test instead.
async fn cancel_and_join(
    cancel: CancellationToken,
    done: tokio::task::JoinHandle<std::result::Result<(), ExchangeError>>,
) {
    cancel.cancel();
    let result = tokio::time::timeout(WAIT_TIMEOUT, done)
        .await
        .unwrap_or_else(|_| panic!("run did not return in time"))
        .expect("join");
    assert_eq!(result, Ok(()));
}

fn base_config(handler: Arc<dyn Handler>) -> Config {
    let mut cfg = Config::new("http://placeholder", TEST_TOKEN, "runner-1", handler);
    cfg.resources = vec!["shop-api".to_string()];
    cfg.backoff = Duration::from_millis(5);
    cfg.max_backoff = Duration::from_millis(20);
    cfg
}

#[tokio::test(flavor = "multi_thread")]
async fn answers_every_op_exactly_once() {
    let (sc, base_url, mut rx) = SlotCloud::new(CONFORMANCE_HOLD).await;
    let handler: Arc<dyn Handler> = Arc::new(EchoHandler {
        max_inflight: 4,
        block: None,
        received: None,
    });
    let mut cfg = base_config(handler);
    cfg.cloud_url = base_url;

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    sc.wait_presence().await;

    for id in ["op-1", "op-2", "op-3", "op-4", "op-5"] {
        recv_timeout(&mut rx.hold_started).await; // force delivery out of an open hold
        sc.enqueue(build_op_frame(id, id));
        rx.answered.wait_for(|got| got == id).await;
    }

    assert!(rx.answered.drain().is_empty());
    assert_eq!(sc.dropped_count(), 0);

    cancel_and_join(cancel, done).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn first_exchange_immediate_then_hold() {
    let (sc, base_url, mut rx) = SlotCloud::new(CONFORMANCE_HOLD).await;
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(4));
    let mut cfg = base_config(handler);
    cfg.cloud_url = base_url;

    let start = std::time::Instant::now();
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let first = rx.events.wait_for(|e| e.n == 0).await;
    assert!(
        start.elapsed() < CONFORMANCE_HOLD,
        "first exchange took {:?}, want well under the {:?} hold",
        start.elapsed(),
        CONFORMANCE_HOLD
    );
    assert_eq!(first.status, 204, "first exchange: nothing queued yet");

    recv_timeout(&mut rx.hold_started).await;

    let frame = build_op_frame("op-1", "op-1");
    sc.enqueue(frame);

    let second = rx.events.wait_for(|e| e.n == 1).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.sent.len(), 1);
    assert_eq!(second.sent[0].id, "op-1");

    cancel_and_join(cancel, done).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn drains_by_max_inflight() {
    let (sc, base_url, mut rx) = SlotCloud::new(CONFORMANCE_HOLD).await;
    let handler = Arc::new(CapacityHandler::new(2));
    let handler_dyn: Arc<dyn Handler> = handler.clone();
    let mut cfg = base_config(handler_dyn);
    cfg.cloud_url = base_url;

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    sc.wait_presence().await;

    let ids = ["op-1", "op-2", "op-3", "op-4", "op-5"];
    for id in ids {
        sc.enqueue(build_op_frame(id, id));
    }

    let at_cap = handler.take_at_cap();
    // two ops now genuinely running at once
    let _ = tokio::time::timeout(WAIT_TIMEOUT, at_cap)
        .await
        .expect("two ops were never running at once");
    handler.release();

    for id in ids {
        rx.answered.wait_for(|got| got == id).await;
    }

    assert!(handler.high.load(Ordering::SeqCst) <= 2);
    for e in rx.events.drain() {
        let limit = (e.max_inflight_declared - e.before_inflight).max(1);
        assert!(
            (e.sent.len() as i64) <= limit,
            "exchange {} sent {} ops, want <= {} (max-inflight={} before={})",
            e.n,
            e.sent.len(),
            limit,
            e.max_inflight_declared,
            e.before_inflight
        );
    }

    cancel_and_join(cancel, done).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn late_frame_is_dropped() {
    let (sc, base_url, mut rx) = SlotCloud::new(CONFORMANCE_HOLD).await;
    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel();
    let (block_tx, block_rx) = tokio::sync::watch::channel(false);
    let handler: Arc<dyn Handler> = Arc::new(EchoHandler {
        max_inflight: 4,
        received: Some(received_tx),
        block: Some(block_rx),
    });
    let mut cfg = base_config(handler);
    cfg.cloud_url = base_url;

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    sc.wait_presence().await;

    sc.enqueue(build_op_frame("op-late", "op-late"));
    recv_timeout(&mut received_rx).await; // the runner has started executing it

    sc.timeout_op("op-late"); // the fake gives up on it
    let _ = block_tx.send(true); // let the (now-late) reply go out

    rx.dropped.wait_for(|id| id == "op-late").await;
    assert_eq!(sc.dropped_count(), 1);

    // The runner must not resend it and must keep serving new ops.
    sc.enqueue(build_op_frame("op-fresh", "op-fresh"));
    rx.answered.wait_for(|id| id == "op-fresh").await;

    cancel_and_join(cancel, done).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn outage_backoff_bounded_then_recovers() {
    let (sc, base_url, _rx) = SlotCloud::new(CONFORMANCE_HOLD).await;
    let handler: Arc<dyn Handler> = Arc::new(EchoHandler {
        max_inflight: 4,
        block: None,
        received: None,
    });

    let durations: Arc<std::sync::Mutex<Vec<Duration>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let d2 = Arc::clone(&durations);
    let sleep: SleepFn = Arc::new(move |_cancel, d| {
        let durations = Arc::clone(&d2);
        Box::pin(async move {
            durations.lock().unwrap_or_else(|e| e.into_inner()).push(d);
            true
        })
    });

    // max_backoff is left at Config::new's zero-duration-normalized default
    // on purpose: the default (10s) must apply here, as when
    // MaxBackoff is left unset.
    let mut cfg = Config::new(base_url, TEST_TOKEN, "runner-1", handler);
    cfg.resources = vec!["shop-api".to_string()];
    cfg.sleep = Some(sleep);

    sc.set_outage(true);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    let got = tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            {
                let got = durations.lock().unwrap_or_else(|e| e.into_inner());
                if got.iter().sum::<Duration>() >= Duration::from_secs(20) {
                    return got.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("sleep sum reached 20s");
    for (i, d) in got.iter().enumerate() {
        assert!(
            *d <= Duration::from_secs(10),
            "sleep[{i}] = {d:?}, want <= 10s (default max_backoff)"
        );
    }

    sc.set_outage(false);
    sc.wait_presence().await;

    let mut rx = _rx;
    sc.enqueue(build_op_frame("op-recovered", "op-recovered"));
    rx.answered.wait_for(|id| id == "op-recovered").await;

    cancel_and_join(cancel, done).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reissued_op_after_restart() {
    let (sc, base_url, mut rx) = SlotCloud::new(CONFORMANCE_HOLD).await;
    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel();
    let (block_tx, block_rx) = tokio::sync::watch::channel(false);
    let handler: Arc<dyn Handler> = Arc::new(EchoHandler {
        max_inflight: 4,
        received: Some(received_tx),
        block: Some(block_rx),
    });
    let mut cfg = base_config(handler);
    cfg.cloud_url = base_url;

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    sc.wait_presence().await;

    sc.enqueue(build_op_frame("op-x-old", "op-x"));
    recv_timeout(&mut received_rx).await; // executing the old op, still blocked

    // A restart re-issues the op: a new op, so a fresh op id too.
    sc.restart(build_op_frame("op-x-fresh", "op-x-fresh"));

    let _ = block_tx.send(true); // let both the blocked old run and the fresh dispatch proceed

    rx.answered.wait_for(|id| id == "op-x-fresh").await;
    rx.dropped.wait_for(|id| id == "op-x").await; // the old op's (late) answer, now unrecognised

    cancel_and_join(cancel, done).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn on_connected_fires_once_across_outage() {
    let (sc, base_url, mut rx) = SlotCloud::new(CONFORMANCE_HOLD).await;
    let handler: Arc<dyn Handler> = Arc::new(EchoHandler {
        max_inflight: 4,
        block: None,
        received: None,
    });

    let durations: Arc<std::sync::Mutex<Vec<Duration>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let d2 = Arc::clone(&durations);
    let sleep: SleepFn = Arc::new(move |_cancel, d| {
        let durations = Arc::clone(&d2);
        Box::pin(async move {
            durations.lock().unwrap_or_else(|e| e.into_inner()).push(d);
            true
        })
    });

    let calls = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let calls2 = Arc::clone(&calls);
    let (connected_tx, mut connected_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut cfg = base_config(handler);
    cfg.cloud_url = base_url;
    cfg.sleep = Some(sleep);
    cfg.on_connected = Some(Arc::new(move || {
        calls2.fetch_add(1, Ordering::SeqCst);
        let _ = connected_tx.send(());
    }));

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });

    recv_timeout(&mut connected_rx).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    for id in ["op-a", "op-b", "op-c"] {
        sc.enqueue(build_op_frame(id, id));
        rx.answered.wait_for(|got| got == id).await;
    }

    sc.set_outage(true);
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            {
                let got = durations.lock().unwrap_or_else(|e| e.into_inner());
                if got.iter().sum::<Duration>() >= Duration::from_millis(30) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("sleep sum reached 30ms");
    sc.set_outage(false);

    // Presence was already established before the outage, so recovery is
    // proven by this op actually being delivered and answered below, not by
    // a second presence signal.
    sc.enqueue(build_op_frame("op-recovered", "op-recovered"));
    rx.answered.wait_for(|id| id == "op-recovered").await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);

    cancel_and_join(cancel, done).await;
}
