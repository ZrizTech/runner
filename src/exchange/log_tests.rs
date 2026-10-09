//! Real exchange lines through the real subscriber: `runner connected` once,
//! `poll done` at DEBUG with trace `-`, `frame skipped`, and no dropped key.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use std::io::Write;
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

#[tokio::test(flavor = "current_thread")]
async fn exchange_lines_follow_the_log_format() {
    // Keep a second dispatcher alive: with only one, tracing lets a thread
    // that has no subscriber cache "never" for a callsite it reaches first.
    let _pin = tracing::Dispatch::new(tracing_subscriber::registry());
    let buf = Buf::default();
    let (sub, logging) = crate::logfmt::subscriber_with_writer("debug", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);

    let op_frame = build_op_frame("frame-op-1", "op-1");
    let mut odd = op_frame.clone();
    odd.t = "hello".to_string();
    let cloud = ImmediateCloud::new(move |n, _req| {
        if n == 0 {
            (200, Some(exchange_response_body(vec![odd.clone()])))
        } else {
            (204, None)
        }
    })
    .await;
    let mut rx = cloud.take_receiver();
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(3));
    let mut cfg = Config::new(cloud.base_url.clone(), TEST_TOKEN, "runner-1", handler);
    cfg.env = "uat".to_string();
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });
    wait_for(&mut rx, |a| a.n >= 2).await;
    cancel.cancel();
    done.await.unwrap().unwrap();

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let has = |needle: &str| out.lines().any(|l| l.contains(needle));
    assert_eq!(out.matches("runner connected").count(), 1, "{out}");
    assert!(
        has(&format!(
            " INFO  trace_id=-                                    runner.exchange runner connected runner=runner-1 env=uat cloud={}",
            cloud.base_url
        )),
        "{out}"
    );
    assert!(
        has(
            " INFO  trace_id=-                                    runner.exchange poll done http_status=200 sent=0 received=1 elapsed_ms="
        ),
        "{out}"
    );
    assert!(
        has(
            " DEBUG trace_id=-                                    runner.exchange poll done http_status=204 sent=0 received=0 elapsed_ms="
        ),
        "{out}"
    );
    assert!(
        has(
            " WARN  trace_id=-                                    runner.exchange frame skipped frame=hello"
        ),
        "{out}"
    );
    assert!(!out.contains("Some(") && !out.contains("None"), "{out}");
    assert_eq!(logging.dropped(), 0, "{out}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_poll_is_a_warn_line() {
    let _pin = tracing::Dispatch::new(tracing_subscriber::registry());
    let buf = Buf::default();
    let (sub, logging) = crate::logfmt::subscriber_with_writer("info", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);
    let cloud = ImmediateCloud::new(|_n, _req| (500, None)).await;
    let mut rx = cloud.take_receiver();
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(3));
    let mut cfg = Config::new(cloud.base_url.clone(), TEST_TOKEN, "runner-1", handler);
    cfg.backoff = Duration::from_millis(5);
    cfg.max_backoff = Duration::from_millis(20);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });
    wait_for(&mut rx, |a| a.n >= 1).await;
    cancel.cancel();
    done.await.unwrap().unwrap();
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains(" WARN  trace_id=-                                    runner.exchange poll failed http_status=500 error=bad-status elapsed_ms="), "{out}");
    assert!(!out.contains("runner connected"), "{out}");
    assert_eq!(logging.dropped(), 0, "{out}");
}
