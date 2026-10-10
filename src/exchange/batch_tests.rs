//! The request batching and the 3-tries rule against a fake cloud that
//! answers at once. The loop is driven by hand: no timers, no triggers apart
//! from the ones `exchange` itself fires.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use std::sync::atomic::Ordering;
use std::time::Instant;

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

/// Calls `exchange` by hand until `done` is true (a deadline of 30 s). A
/// frame that goes out and comes back (a refusal) is in no state the test can
/// see, so the test never looks at the queue alone to decide it is finished.
async fn drive(i: &Arc<Inner>, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "not done in time: {:?}",
            queued(i)
        );
        let _ = i.exchange().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
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
    // No spacing: every refusal of a lone frame counts, so the number of
    // tries does not depend on how fast the machine is. (The spacing rule has
    // its own tests in `batch.rs`.)
    let i = inner(&cloud.base_url, Duration::ZERO);
    for id in ["bad", "g1", "g2"] {
        i.push_back(frame(id, 0));
    }
    let mut got = Vec::new();
    // The batch and three lone tries have arrived, and so have both good
    // frames. Exchanges the loop itself triggers run in other tasks.
    drive(&i, || {
        while let Ok(a) = rx.try_recv() {
            got.push(a.req);
        }
        let bad = got.iter().filter(|r| has(r, "bad")).count();
        bad >= 4 && ["g1", "g2"].iter().all(|g| got.iter().any(|r| has(r, g)))
    })
    .await;
    // The last try of "bad" may still be on its way back: wait for the end.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !queued(&i).is_empty() {
        assert!(Instant::now() < deadline, "{:?}", queued(&i));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // A fifth try would be a frame kept; give a stray one time to show up.
    tokio::time::sleep(Duration::from_millis(50)).await;
    while let Ok(a) = rx.try_recv() {
        got.push(a.req);
    }
    let mut bad: Vec<_> = got.iter().filter(|r| has(r, "bad")).collect();
    bad.sort_by_key(|r| std::cmp::Reverse(r.frames.len()));
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
    drive(&i, || queued(&i).is_empty()).await;
    // Exchanges the loop triggers run in other tasks, so a request may be
    // still in flight; the arrivals are counted until all three frames came.
    let mut sizes = Vec::new();
    let mut seen = 0;
    while seen < 3 {
        let a = recv_timeout(&mut rx).await;
        seen += a.req.frames.len();
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
    // Two requests; the two can reach the cloud in either order.
    sizes.sort();
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

/// A handler whose busy number is set by the test.
struct Busy(std::sync::atomic::AtomicU64);
impl Handler for Busy {
    fn handle(
        &self,
        _op: contract::Op,
    ) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
        Box::pin(async { (None, None) })
    }
    fn max_inflight(&self) -> i64 {
        4
    }
    fn worker_health(&self) -> WorkerHealth {
        WorkerHealth {
            needed: true,
            up: true,
            browser: vec![contract::BrowserLoad {
                resource: "web".into(),
                busy: self.0.load(Ordering::SeqCst),
                limit: 8,
            }],
            cli: None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_health_sends_one_request_and_an_unchanged_one_none() {
    let cloud = ImmediateCloud::new(|_, _| (204, None)).await;
    let mut rx = cloud.take_receiver();
    let busy = Arc::new(Busy(std::sync::atomic::AtomicU64::new(8)));
    let h: Arc<dyn Handler> = busy.clone();
    let mut cfg = Config::new(&cloud.base_url, TEST_TOKEN, "runner-1", h);
    cfg.health_every = Duration::from_secs(1);
    let i = Arc::new(Inner::new(cfg, CancellationToken::new()));
    let t0 = Instant::now();
    let _ = i.exchange().await;
    let first = rx.recv().await.unwrap();
    assert_eq!(first.req.health.state, "busy");
    // 100 checks with the same numbers: no request.
    for k in 0..100 {
        i.check_health(t0 + Duration::from_millis(k * 20));
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rx.try_recv().is_err(), "no extra request");
    // The last busy place is freed: one request, says ok.
    busy.0.store(0, Ordering::SeqCst);
    i.check_health(t0 + Duration::from_secs(3));
    i.check_health(t0 + Duration::from_millis(3100));
    let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.req.health.state, "ok");
    assert_eq!(second.req.health.browser.as_ref().unwrap()[0].busy, 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rx.try_recv().is_err(), "one request, not two");
}
