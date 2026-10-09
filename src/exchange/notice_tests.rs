//! The run-end notice, the rule of 3 tries and the health on the wire,
//! against a fake cloud that answers at once.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use crate::contract::{BrowserLoad, CliLoad};
use std::io::Write;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;

const TRACE: &str = "5d0c9a52-7c1e-4b1f-9a55-0d6f3a2b8c11";

/// Answers each op with a pass, or with the error `reason` when set; reports
/// each run-end notice; its worker numbers are set by the test.
struct NoticeHandler {
    reason: Option<&'static str>,
    worker: WorkerHealth,
    ended: mpsc::UnboundedSender<(String, String)>,
}

impl Handler for NoticeHandler {
    fn handle(&self, op: contract::Op) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
        Box::pin(async move {
            match self.reason {
                Some(r) => (
                    None,
                    Some(ErrorFrame::new(
                        &op.op_id,
                        r,
                        serde_json::json!({"where": "op-handler"}),
                    )),
                ),
                None => (
                    Some(ResultFrame {
                        op_id: op.op_id,
                        status: "pass".to_string(),
                        payload: Default::default(),
                        scrubbed: 0,
                        timing: contract::Timing { exec_ms: 1 },
                    }),
                    None,
                ),
            }
        })
    }

    fn max_inflight(&self) -> i64 {
        4
    }

    fn worker_health(&self) -> WorkerHealth {
        self.worker.clone()
    }

    fn run_ended(&self, run_id: &str, trace_id: &str) -> BoxFuture<'_, ()> {
        let _ = self.ended.send((run_id.to_string(), trace_id.to_string()));
        Box::pin(async {})
    }
}

fn handler(
    reason: Option<&'static str>,
    worker: WorkerHealth,
) -> (Arc<dyn Handler>, mpsc::UnboundedReceiver<(String, String)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let h = NoticeHandler {
        reason,
        worker,
        ended: tx,
    };
    (Arc::new(h), rx)
}

fn config(url: &str, h: Arc<dyn Handler>) -> Config {
    let mut cfg = Config::new(url, TEST_TOKEN, "runner-1", h);
    cfg.backoff = Duration::from_millis(2);
    cfg.max_backoff = Duration::from_millis(5);
    cfg
}

/// Runs the loop until `stop` is true for the arrivals so far; returns them.
async fn arrivals_until(
    cloud: &ImmediateCloud,
    cfg: Config,
    stop: impl Fn(&[Arrival]) -> bool,
) -> Vec<Arrival> {
    let mut rx = cloud.take_receiver();
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });
    let mut got = Vec::new();
    while !stop(&got) {
        got.push(recv_timeout(&mut rx).await);
    }
    cancel.cancel();
    done.await.unwrap().unwrap();
    got
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_with_frames_runs_the_op_and_frees_the_run() {
    let cloud = ImmediateCloud::new(|n, _| match n {
        0 => (
            200,
            Some(exchange_response_with(
                vec![build_op_frame("f1", "op-1")],
                &[("run-9", TRACE)],
            )),
        ),
        _ => (204, None),
    })
    .await;
    let (h, mut ended) = handler(None, WorkerHealth::default());
    let got = arrivals_until(&cloud, config(&cloud.base_url, h), |a| {
        a.iter().any(|x| !x.req.frames.is_empty())
    })
    .await;
    assert_eq!(
        recv_timeout(&mut ended).await,
        ("run-9".into(), TRACE.into())
    );
    assert_eq!(got.iter().filter(|a| !a.req.frames.is_empty()).count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_with_no_frame_is_a_normal_200() {
    let cloud = ImmediateCloud::new(|n, _| match n {
        0 => (
            200,
            Some(exchange_response_with(
                vec![],
                &[("run-1", TRACE), ("run-2", TRACE)],
            )),
        ),
        _ => (204, None),
    })
    .await;
    let (h, mut ended) = handler(None, WorkerHealth::default());
    arrivals_until(&cloud, config(&cloud.base_url, h), |a| a.len() >= 3).await;
    assert_eq!(recv_timeout(&mut ended).await.0, "run-1");
    assert_eq!(recv_timeout(&mut ended).await.0, "run-2");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_with_an_unknown_top_level_key_is_accepted() {
    let body = format!(
        r#"{{"v":1,"frames":[],"ended-runs":[{{"run-id":"run-1","trace-id":"{TRACE}"}}],"extra":{{"a":1}}}}"#
    );
    let cloud = ImmediateCloud::new(move |n, _| match n {
        0 => (200, Some(body.clone().into_bytes())),
        _ => (204, None),
    })
    .await;
    let (h, mut ended) = handler(None, WorkerHealth::default());
    arrivals_until(&cloud, config(&cloud.base_url, h), |a| a.len() >= 2).await;
    assert_eq!(recv_timeout(&mut ended).await.0, "run-1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_200_with_no_ended_runs_is_a_bad_reply() {
    let cloud = ImmediateCloud::new(|n, _| match n {
        0 => (200, Some(br#"{"v":1,"frames":[]}"#.to_vec())),
        _ => (204, None),
    })
    .await;
    let (h, mut ended) = handler(None, WorkerHealth::default());
    arrivals_until(&cloud, config(&cloud.base_url, h), |a| a.len() >= 2).await;
    assert!(ended.try_recv().is_err());
}

/// An op in the first answer; every request that carries a frame gets `status`
/// (up to `times` times), then 204.
async fn refusing_cloud(status: u16, times: usize) -> ImmediateCloud {
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    ImmediateCloud::new(move |n, req| {
        if n == 0 {
            let f = vec![build_op_frame("f1", "op-1")];
            return (200, Some(exchange_response_body(f)));
        }
        if !req.frames.is_empty() && seen.fetch_add(1, Ordering::SeqCst) < times {
            return (status, None);
        }
        (204, None)
    })
    .await
}

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

#[tokio::test(flavor = "current_thread")]
async fn three_refusals_drop_the_frames_with_one_line() {
    let _pin = tracing::Dispatch::new(tracing_subscriber::registry());
    let buf = Buf::default();
    let (sub, _logging) = crate::logfmt::subscriber_with_writer("info", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);
    let cloud = refusing_cloud(400, usize::MAX).await;
    let (h, _ended) = handler(None, WorkerHealth::default());
    let seen = buf.clone();
    let got = arrivals_until(&cloud, config(&cloud.base_url, h), |a| {
        let carrying = a.iter().filter(|x| !x.req.frames.is_empty()).count();
        let dropped = String::from_utf8_lossy(&seen.0.lock().unwrap()).contains("frames dropped");
        carrying >= 3 && dropped && a.last().is_some_and(|x| x.req.frames.is_empty())
    })
    .await;
    let carrying = got.iter().filter(|x| !x.req.frames.is_empty()).count();
    assert!(
        carrying >= 3,
        "sent at least three times (spaced), then dropped"
    );
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert_eq!(out.matches("frames dropped").count(), 1, "{out}");
    assert!(
        out.contains("frames dropped http_status=400 reason=refused count=1")
            || out.contains("frames dropped http_status=400 count=1 reason=refused"),
        "{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_503_keeps_the_frames() {
    let cloud = refusing_cloud(503, 5).await;
    let (h, _ended) = handler(None, WorkerHealth::default());
    let got = arrivals_until(&cloud, config(&cloud.base_url, h), |a| {
        a.iter().filter(|x| !x.req.frames.is_empty()).count() >= 6
    })
    .await;
    let ids: Vec<&str> = got
        .iter()
        .filter(|x| !x.req.frames.is_empty())
        .map(|x| x.req.frames[0].id.as_str())
        .collect();
    assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");
}

fn load(worker_up: bool) -> WorkerHealth {
    WorkerHealth {
        needed: true,
        up: worker_up,
        browser: vec![BrowserLoad {
            resource: "web".into(),
            busy: 1,
            limit: 3,
        }],
        cli: Some(CliLoad { busy: 0, limit: 2 }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_request_has_the_same_boot_id_and_the_worker_numbers() {
    let cloud = ImmediateCloud::new(|_, _| (204, None)).await;
    let (h, _ended) = handler(None, load(false));
    let got = arrivals_until(&cloud, config(&cloud.base_url, h), |a| a.len() >= 5).await;
    let first = &got[0].req.health;
    assert!(first.boot_id.starts_with("b-") && first.boot_id.len() == 14);
    for a in &got {
        assert_eq!(a.req.health.boot_id, first.boot_id);
        assert_eq!(a.req.health.state, "degraded");
        assert_eq!(a.req.health.worker.as_deref(), Some("down"));
        assert_eq!(a.req.health.browser.as_ref().unwrap()[0].limit, 3);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_is_counted_then_back_to_zero_after_a_request() {
    let cloud = ImmediateCloud::new(|n, _| {
        if n == 0 {
            (
                200,
                Some(exchange_response_body(vec![build_op_frame("f1", "op-1")])),
            )
        } else {
            (204, None)
        }
    })
    .await;
    let (h, _ended) = handler(Some("runner-at-capacity"), load(true));
    let got = arrivals_until(&cloud, config(&cloud.base_url, h), |a| {
        a.iter()
            .position(|x| x.req.health.refused > 0)
            .is_some_and(|i| a.len() > i + 2)
    })
    .await;
    let i = got.iter().position(|x| x.req.health.refused > 0).unwrap();
    assert_eq!(got[i].req.health.refused, 1);
    assert_eq!(got[i].req.health.state, "busy");
    assert_eq!(got[i + 1].req.health.refused, 0);
    assert_eq!(got[i + 1].req.health.state, "ok");
    assert!(got[..i].iter().all(|x| x.req.health.refused == 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runner_error_makes_the_next_request_degraded_once() {
    let cloud = ImmediateCloud::new(|n, _| {
        if n == 0 {
            (
                200,
                Some(exchange_response_body(vec![build_op_frame("f1", "op-1")])),
            )
        } else {
            (204, None)
        }
    })
    .await;
    let (h, _ended) = handler(Some("runner-error"), load(true));
    let got = arrivals_until(&cloud, config(&cloud.base_url, h), |a| {
        a.iter()
            .position(|x| x.req.health.state == "degraded")
            .is_some_and(|i| a.len() > i + 2)
    })
    .await;
    let i = got
        .iter()
        .position(|x| x.req.health.state == "degraded")
        .unwrap();
    assert_eq!(got[i + 1].req.health.state, "ok");
}
