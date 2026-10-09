//! The request batching and the 3-tries rule against a fake cloud that
//! answers at once. The loop is driven by hand: no timers, no triggers apart
//! from the ones `exchange` itself fires.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use std::sync::atomic::Ordering;

fn frame(id: &str, pad: usize) -> contract::Frame {
    contract::Frame {
        v: 1,
        t: "result".into(),
        id: id.into(),
        re: None,
        ts: 1,
        d: serde_json::json!({"pad": "x".repeat(pad)}),
    }
}

fn inner(url: &str, backoff: Duration) -> Arc<Inner> {
    let h: Arc<dyn Handler> = Arc::new(FakeHandler::new(2));
    let mut cfg = Config::new(url, TEST_TOKEN, "runner-1", h);
    cfg.backoff = backoff;
    Arc::new(Inner::new(cfg, CancellationToken::new()))
}

fn queued(i: &Inner) -> Vec<String> {
    let q = i.queue.lock().unwrap();
    q.iter().map(|f| f.id.clone()).collect()
}

fn has(req: &contract::ExchangeRequest, id: &str) -> bool {
    req.frames.iter().any(|f| f.id == id)
}

#[tokio::test(flavor = "multi_thread")]
async fn one_bad_frame_is_dropped_alone_and_the_good_ones_arrive() {
    let cloud = ImmediateCloud::new(|_, req| {
        if has(req, "bad") {
            (400, None)
        } else {
            (204, None)
        }
    })
    .await;
    let mut rx = cloud.take_receiver();
    let i = inner(&cloud.base_url, Duration::from_millis(1));
    for id in ["bad", "g1", "g2"] {
        i.push_back(frame(id, 0));
    }
    for _ in 0..60 {
        if queued(&i).is_empty() {
            break;
        }
        let _ = i.exchange().await;
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(queued(&i).is_empty(), "{:?}", queued(&i));
    let mut got = Vec::new();
    while let Ok(a) = rx.try_recv() {
        got.push(a.req);
    }
    let bad: Vec<_> = got.iter().filter(|r| has(r, "bad")).collect();
    assert_eq!(bad[0].frames.len(), 3, "the first request is the batch");
    assert!(bad[1..].iter().all(|r| r.frames.len() == 1), "then alone");
    assert_eq!(bad.len(), 4, "one batch and three lone tries");
    for g in ["g1", "g2"] {
        assert!(got.iter().any(|r| has(r, g) && !has(r, "bad")), "{g}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn immediate_triggers_do_not_burn_the_three_tries() {
    let cloud = ImmediateCloud::new(|_, _| (400, None)).await;
    let i = inner(&cloud.base_url, Duration::from_secs(30));
    i.push_back(frame("bad", 0));
    for _ in 0..5 {
        let _ = i.exchange().await;
    }
    assert_eq!(queued(&i), ["bad"], "still queued");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_one_mib_frames_go_in_two_requests() {
    let cloud = ImmediateCloud::new(|_, _| (204, None)).await;
    let mut rx = cloud.take_receiver();
    let i = inner(&cloud.base_url, Duration::from_millis(1));
    i.push_back(frame("big1", 1 << 20));
    i.push_back(frame("big2", 1 << 20));
    i.push_back(frame("small", 10));
    for _ in 0..6 {
        let _ = i.exchange().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut sizes = Vec::new();
    while let Ok(a) = rx.try_recv() {
        if !a.req.frames.is_empty() {
            sizes.push(
                a.req
                    .frames
                    .iter()
                    .map(|f| f.id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    }
    assert_eq!(sizes, ["big1", "big2,small"], "{sizes:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_request_with_no_frame_drops_nothing() {
    let cloud = ImmediateCloud::new(|n, _| if n < 5 { (400, None) } else { (204, None) }).await;
    let i = inner(&cloud.base_url, Duration::from_millis(1));
    for _ in 0..5 {
        let _ = i.exchange().await;
    }
    i.push_back(frame("f", 0));
    let _ = i.exchange().await;
    assert!(queued(&i).is_empty(), "the frame went out and was taken");
}

#[tokio::test(flavor = "multi_thread")]
async fn health_counts_come_back_after_a_5xx() {
    let cloud = ImmediateCloud::new(|n, _| if n == 0 { (503, None) } else { (204, None) }).await;
    let mut rx = cloud.take_receiver();
    let i = inner(&cloud.base_url, Duration::from_millis(1));
    i.refused.store(2, Ordering::SeqCst);
    i.errors.store(1, Ordering::SeqCst);
    assert!(i.exchange().await.is_err());
    let first = rx.recv().await.unwrap();
    assert_eq!(first.req.health.refused, 2);
    let _ = i.exchange().await;
    let second = rx.recv().await.unwrap();
    assert_eq!(second.req.health.refused, 2, "put back");
    assert_eq!(second.req.health.state, "degraded");
}
