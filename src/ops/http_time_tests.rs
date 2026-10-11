//! Spec 8.1, rows H1-H4: the target time of http.request holds no connect.
//! These tests use real time. The fake target answers after D = 200 ms and
//! the slow connect is S = 1000 ms; a target time passes when 200 <= t < 700.

use crate::config::{Cloud, Config, Resource};
use crate::ops::http_client::{Clients, base_builder, measured};
use crate::ops::target::Span;
use crate::ops::tests::{test_lookup, test_op};
use crate::ops::{Handler, Options};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tower_layer::Layer;
use tower_service::Service;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const D: Duration = Duration::from_millis(200);
const S: Duration = Duration::from_millis(1000);

/// Test only: sleeps in each connect of the client, a slow network. Connect
/// number n sleeps `delays[n]`; the last delay holds for every later one.
#[derive(Clone)]
struct DelayLayer {
    delays: Vec<Duration>,
    seen: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct DelayService<S> {
    inner: S,
    layer: DelayLayer,
}

impl<S> Layer<S> for DelayLayer {
    type Service = DelayService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        DelayService {
            inner,
            layer: self.clone(),
        }
    }
}

impl<S, R> Service<R> for DelayService<S>
where
    S: Service<R>,
    S::Future: Send + 'static,
    S::Response: 'static,
    S::Error: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let fut = self.inner.call(req);
        let n = self.layer.seen.fetch_add(1, Ordering::SeqCst);
        let d = self.layer.delays.get(n).or(self.layer.delays.last());
        let delay = d.copied().unwrap_or_default();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            fut.await
        })
    }
}

fn slow_clients(delays: Vec<Duration>) -> (Clients, Arc<AtomicUsize>) {
    let seen = Arc::new(AtomicUsize::new(0));
    let layer = DelayLayer {
        delays,
        seen: seen.clone(),
    };
    let build = move || measured(base_builder().connector_layer(layer.clone()));
    (Clients::with_builder(Arc::new(build)), seen)
}

async fn slow_handler(base_url: &str, slow: Option<Duration>) -> Handler {
    let delays = slow.map(|d| vec![d]).unwrap_or_default();
    slow_handler_with(base_url, delays).await.0
}

async fn slow_handler_with(base_url: &str, delays: Vec<Duration>) -> (Handler, Arc<AtomicUsize>) {
    let mut resources = HashMap::new();
    resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{base_url}/api"),
            ..Default::default()
        },
    );
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let mut h = Handler::new(
        cfg,
        Options {
            lookup: Some(test_lookup(HashMap::new())),
            ..Default::default()
        },
    )
    .await
    .expect("Handler::new");
    let (clients, seen) = slow_clients(delays);
    h.clients = clients;
    (h, seen)
}

async fn answering(delay: Duration, p: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_delay(delay))
        .mount(&server)
        .await;
    server
}

async fn get(h: &Handler, op_id: &str, p: &str) -> (i64, i64) {
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String(p.to_string()));
    let (result, err) = h
        .handle(test_op(
            op_id,
            "run-t",
            "http.request",
            "api",
            20_000,
            args,
            vec![],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let t = result.expect("result").timing;
    (t.target_ms.expect("target-ms"), t.exec_ms)
}

fn passes(t: i64) -> bool {
    (200..700).contains(&t)
}

#[tokio::test]
async fn h1_cold_then_warm_target_has_no_connect() {
    let server = answering(D, "/api/x").await;
    let h = slow_handler(&server.uri(), Some(S)).await;
    let (target, exec) = get(&h, "op-1", "/x").await;
    eprintln!("H1 cold: target-ms={target} exec-ms={exec}");
    assert!(passes(target), "cold target {target}");
    assert!(exec >= 1200, "cold exec {exec}");
    let (target, exec) = get(&h, "op-2", "/x").await;
    eprintln!("H1 warm: target-ms={target} exec-ms={exec}");
    assert!(passes(target), "warm target {target}");
    assert!(
        exec < 1000,
        "warm exec {exec}: the connection was not reused"
    );
}

#[tokio::test]
async fn h2_two_hops_each_with_a_connect_sum_to_two_waits() {
    // The runner follows same-origin redirects only (origin::check_origin),
    // so the second "origin" of the spec is the same origin on a new
    // connection: the first answer closes it, so hop 2 connects (slowly) again.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/redirect"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "/api/landed")
                .insert_header("connection", "close")
                .set_delay(D),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/landed"))
        .respond_with(ResponseTemplate::new(200).set_delay(D))
        .mount(&server)
        .await;
    let h = slow_handler(&server.uri(), Some(S)).await;
    let (target, exec) = get(&h, "op-1", "/redirect").await;
    eprintln!("H2: target-ms={target} exec-ms={exec}");
    assert!((400..900).contains(&target), "target {target}");
    assert!(exec >= 2400, "exec {exec}");
}

#[tokio::test]
async fn h3_two_ops_at_once_on_a_cold_resource() {
    let server = answering(D, "/api/x").await;
    let h = slow_handler(&server.uri(), Some(S)).await;
    let ((t1, e1), (t2, e2)) = tokio::join!(get(&h, "op-1", "/x"), get(&h, "op-2", "/x"));
    eprintln!("H3: a target-ms={t1} exec-ms={e1}; b target-ms={t2} exec-ms={e2}");
    assert!(passes(t1), "first target {t1}");
    assert!(passes(t2), "second target {t2}");
}

#[tokio::test]
async fn client_returns_to_the_idle_set_after_an_error_frame() {
    // A port that nothing listens on: bind, note the port, close.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let uri = format!("http://127.0.0.1:{port}");
    let h = slow_handler(&uri, None).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/x".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-e",
            "run-t",
            "http.request",
            "api",
            5000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none(), "{result:?} {err:?}");
    assert_eq!(err.expect("error").reason, "connection-error");
    assert_eq!(h.clients.idle_count("api"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h4_keep_alive_redirect_is_not_lowered_by_a_background_connect() {
    // Hop 1 answers 302 after 5 ms and keeps the connection alive. Hop 2 may
    // start a new connect (hyper-util races it against the pool) and then run
    // on hop 1's connection. The first connect is 20 ms; a later one is 40 ms,
    // shorter than hop 2's 80 ms wait, so it ends inside the op. Only the
    // first connect may leave the target time: 5 + 80 = 85 ms stays. The race
    // is a matter of timing, so the op runs many times.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/redirect"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "/api/landed")
                .set_delay(Duration::from_millis(5)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/landed"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(80)))
        .mount(&server)
        .await;
    let delays = vec![Duration::from_millis(20), Duration::from_millis(40)];
    let (h, seen) = slow_handler_with(&server.uri(), delays).await;
    let mut worst = i64::MAX;
    for i in 0..40 {
        let (target, _) = get(&h, &format!("op-{i}"), "/redirect").await;
        worst = worst.min(target);
        assert!((85..140).contains(&target), "op {i} target {target}");
    }
    let connects = seen.load(Ordering::SeqCst);
    eprintln!("H4: lowest target-ms={worst} connects in 40 ops={connects}");
}

#[tokio::test]
async fn a_timed_out_op_does_not_return_its_client() {
    let server = answering(Duration::from_millis(1500), "/api/x").await;
    let h = slow_handler(&server.uri(), None).await;
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String("/x".to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-t",
            "run-t",
            "http.request",
            "api",
            300,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none(), "{result:?}");
    assert_eq!(err.expect("error").reason, "timeout");
    assert_eq!(h.clients.idle_count("api"), 0);
}

#[tokio::test]
async fn an_op_is_charged_only_for_the_connect_of_its_own_connection() {
    // The race of H4, made certain: request A starts a connect (150 ms) and is
    // left running; request B starts a later connect (50 ms) and is the only
    // one the lease waits for. B's wait is 50 + 200 ms; only 50 ms may leave it.
    let server = MockServer::start().await;
    for (p, ms) in [("/api/a", 400), ("/api/b", 200)] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(ms)))
            .mount(&server)
            .await;
    }
    let (clients, _) = slow_clients(vec![Duration::from_millis(150), Duration::from_millis(50)]);
    let mut lease = clients.lease("api").expect("lease");
    let a = lease.client().get(format!("{}/api/a", server.uri()));
    let background = tokio::spawn(async move { a.send().await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut span = Span::default();
    let req = lease.client().get(format!("{}/api/b", server.uri()));
    let resp = span.time(req.send()).await.expect("b");
    let sent_at = span.last_start().expect("started");
    lease.charge(&resp, sent_at);
    let waited = span.total();
    let target = lease.target(waited);
    eprintln!("H4 own: waited={waited:?} target={target:?}");
    assert!(waited >= Duration::from_millis(250), "waited {waited:?}");
    assert!(target >= Duration::from_millis(200), "target {target:?}");
    assert!(target < Duration::from_millis(240), "target {target:?}");
    background.abort();
}
