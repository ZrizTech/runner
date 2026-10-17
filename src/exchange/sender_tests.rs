//! The sender against a fake cloud that delays its replies: results that
//! finish while one result request is in flight go together in the next.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn results_that_finish_in_flight_go_together() {
    const OPS: usize = 50;
    let frames: Vec<contract::Frame> = (0..OPS)
        .map(|i| build_op_frame(&format!("f-{i}"), &format!("op-{i}")))
        .collect();
    let first_reply = exchange_response_body(frames);
    let carrying = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let seen: Arc<StdMutex<HashMap<String, usize>>> = Arc::default();

    let (c, f, m, s) = (
        carrying.clone(),
        in_flight.clone(),
        max_in_flight.clone(),
        seen.clone(),
    );
    let handler = Arc::new(
        move |n: usize, _h: HashMap<String, String>, body: Vec<u8>| {
            let (c, f, m, s, first) = (
                c.clone(),
                f.clone(),
                m.clone(),
                s.clone(),
                first_reply.clone(),
            );
            Box::pin(async move {
                let req: contract::ExchangeRequest = serde_json::from_slice(&body).unwrap();
                if n == 0 {
                    return RawResponse {
                        status: 200,
                        body: Some(first),
                    };
                }
                if req.frames.is_empty() {
                    // the parked poll
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return RawResponse {
                        status: 204,
                        body: None,
                    };
                }
                c.fetch_add(1, Ordering::SeqCst);
                let now = f.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(now, Ordering::SeqCst);
                {
                    let mut s = s.lock().unwrap();
                    for fr in &req.frames {
                        *s.entry(fr.id.clone()).or_default() += 1;
                    }
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                f.fetch_sub(1, Ordering::SeqCst);
                RawResponse {
                    status: 204,
                    body: None,
                }
            }) as futures::future::BoxFuture<'static, RawResponse>
        },
    );
    let base_url = start_raw_server(handler as _).await;

    let (tx, rx) = tokio::sync::watch::channel(false);
    let echo: Arc<dyn Handler> = Arc::new(EchoHandler {
        max_inflight: 64,
        block: Some(rx),
        received: None,
    });
    let mut cfg = Config::new(&base_url, TEST_KEY, "runner-1", echo);
    cfg.resources = vec!["shop-api".to_string()];
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, c2).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(true).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while seen.lock().unwrap().len() < OPS && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(seen.lock().unwrap().len(), OPS, "every result arrives");
    assert!(
        seen.lock().unwrap().values().all(|n| *n == 1),
        "each result exactly once"
    );
    assert!(
        carrying.load(Ordering::SeqCst) <= 2,
        "result requests: {}",
        carrying.load(Ordering::SeqCst)
    );
    assert_eq!(max_in_flight.load(Ordering::SeqCst), 1, "one in flight");
    cancel.cancel();
    let _ = tokio::time::timeout(WAIT_TIMEOUT, done).await;
}

fn result_frame(id: &str) -> contract::Frame {
    contract::Frame {
        v: 1,
        t: "result".into(),
        id: id.into(),
        re: None,
        ts: 1,
        d: serde_json::json!({}),
    }
}

fn sender_inner(url: &str) -> Arc<Inner> {
    let h: Arc<dyn Handler> = Arc::new(FakeHandler::new(2));
    let cfg = Config::new(url, TEST_KEY, "runner-1", h);
    Arc::new(Inner::new(cfg, CancellationToken::new()))
}

/// Results queued at the moment the sender loop ends must still go: no
/// result waits for the long poll. The cloud answers at once; each round
/// queues a result from another thread while the loop is ending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_result_queued_as_the_sender_ends_is_still_sent() {
    let cloud = ImmediateCloud::new(|_, _| (204, None)).await;
    let i = sender_inner(&cloud.base_url);
    for round in 0..300 {
        i.push_back(result_frame(&format!("a-{round}")));
        i.trigger_exchange();
        // Land the next result around the time the loop ends.
        tokio::task::yield_now().await;
        let j = Arc::clone(&i);
        // As in `run_op`: push, then trigger, from the thread that pushed.
        let t = tokio::task::spawn_blocking(move || {
            j.push_back(result_frame(&format!("b-{round}")));
            j.trigger_exchange();
        });
        t.await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while i.queue_pending() || i.gate_busy() {
            assert!(
                std::time::Instant::now() < deadline,
                "round {round}: a result waits with no sender"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

/// A panic in the sender task must not leave the gate busy for ever.
#[tokio::test(flavor = "multi_thread")]
async fn a_panic_in_the_sender_frees_the_gate() {
    struct Boom;
    impl Handler for Boom {
        fn refresh(&self) -> futures::future::BoxFuture<'_, ()> {
            Box::pin(async { panic!("boom") })
        }
        fn max_inflight(&self) -> i64 {
            1
        }
        fn handle(
            &self,
            _op: contract::Op,
        ) -> futures::future::BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
            Box::pin(async { (None, None) })
        }
    }
    let cloud = ImmediateCloud::new(|_, _| (204, None)).await;
    let h: Arc<dyn Handler> = Arc::new(Boom);
    let cfg = Config::new(&cloud.base_url, TEST_KEY, "runner-1", h);
    let i = Arc::new(Inner::new(cfg, CancellationToken::new()));
    i.push_back(result_frame("p"));
    i.trigger_exchange();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while i.gate_busy() {
        assert!(
            std::time::Instant::now() < deadline,
            "gate stays busy after a panic"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A health request carries no result, so the cloud holds it like the main
/// poll (up to 30 s). A result that finishes meanwhile must not wait for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_result_does_not_wait_behind_a_held_health_request() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let handler = Arc::new(
        move |_n: usize, _h: HashMap<String, String>, body: Vec<u8>| {
            let tx = tx.clone();
            Box::pin(async move {
                let req: contract::ExchangeRequest = serde_json::from_slice(&body).unwrap();
                if req.frames.is_empty() {
                    let _ = tx.send("empty".into());
                    tokio::time::sleep(Duration::from_secs(3)).await;
                } else {
                    for f in &req.frames {
                        let _ = tx.send(f.id.clone());
                    }
                }
                RawResponse {
                    status: 204,
                    body: None,
                }
            }) as futures::future::BoxFuture<'static, RawResponse>
        },
    );
    let base_url = start_raw_server(handler).await;
    let i = sender_inner(&base_url);
    // The health sent last differs from now: one health request goes out.
    let mut last = health::health(&i.boot_id, &i.peek_snapshot());
    last.state = "busy".into();
    *i.last_sent.lock().unwrap() = Some(last);
    i.check_health(std::time::Instant::now());
    let first = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    assert_eq!(
        first.ok().flatten().as_deref(),
        Some("empty"),
        "no health request"
    );
    i.push_back(result_frame("r-1"));
    i.trigger_exchange();
    let got = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    assert_eq!(
        got.ok().flatten().as_deref(),
        Some("r-1"),
        "the result waited"
    );
}
