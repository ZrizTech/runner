//! Test-only infrastructure shared by `exchange_tests.rs` and
//! `conformance_tests.rs`: a minimal hand-rolled HTTP/1.1 server (so the
//! fakes below can reproduce the cloud's long-poll hold, which a
//! request/response mocking library can't express), fake `Handler`
//! implementations, and and small wait helpers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{BoxFuture, ErrorFrame, Handler, ResultFrame};
use crate::contract;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

pub(crate) const TEST_TOKEN: &str = "test-token";
pub(crate) const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Waits for `rx` to yield a value within [`WAIT_TIMEOUT`], panicking (test
/// failure) otherwise.
pub(crate) async fn recv_timeout<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    match tokio::time::timeout(WAIT_TIMEOUT, rx.recv()).await {
        Ok(Some(v)) => v,
        Ok(None) => panic!("channel closed while waiting"),
        Err(_) => panic!("timed out after {WAIT_TIMEOUT:?} waiting on channel"),
    }
}

/// Waits until some value received from `rx` satisfies `matches`, buffering
/// (returning) everything else so a caller can inspect what it skipped.
pub(crate) async fn wait_for<T: Clone>(
    rx: &mut mpsc::UnboundedReceiver<T>,
    mut matches: impl FnMut(&T) -> bool,
) -> T {
    match tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(v) if matches(&v) => return v,
                Some(_) => continue,
                None => panic!("channel closed while waiting for a match"),
            }
        }
    })
    .await
    {
        Ok(v) => v,
        Err(_) => panic!("timed out after {WAIT_TIMEOUT:?} waiting for a match"),
    }
}

// ---------------------------------------------------------------------
// A minimal HTTP/1.1 server: just enough to run the exchange endpoint's
// request/response shape (one POST per connection, `Connection: close`),
// so the fakes below can hold a connection open (the cloud's long poll)
// the way no request/response mock library is built to do.
// ---------------------------------------------------------------------

pub(crate) struct RawResponse {
    pub(crate) status: u16,
    pub(crate) body: Option<Vec<u8>>,
}

type RawHandler = Arc<
    dyn Fn(usize, HashMap<String, String>, Vec<u8>) -> BoxFuture<'static, RawResponse>
        + Send
        + Sync,
>;

/// Starts the raw server on an OS-assigned port and returns its base URL
/// (`http://127.0.0.1:PORT`). The accept loop, and every connection task it
/// spawns, are torn down when the test's tokio runtime shuts down.
pub(crate) async fn start_raw_server(handler: RawHandler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local_addr");
    let seq = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            let seq = Arc::clone(&seq);
            tokio::spawn(async move {
                let _ = handle_conn(stream, handler, seq).await;
            });
        }
    });
    format!("http://{addr}")
}

async fn handle_conn(
    mut stream: TcpStream,
    handler: RawHandler,
    seq: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let _request_line = lines.next().unwrap_or_default();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    let n = seq.fetch_add(1, Ordering::SeqCst);
    let resp = handler(n, headers, body).await;

    let mut out = format!("HTTP/1.1 {} status\r\nConnection: close\r\n", resp.status);
    match &resp.body {
        Some(b) => {
            out.push_str("Content-Type: application/json\r\n");
            out.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
        }
        None => out.push_str("Content-Length: 0\r\n\r\n"),
    }
    stream.write_all(out.as_bytes()).await?;
    if let Some(b) = &resp.body {
        stream.write_all(b).await?;
    }
    stream.flush().await?;
    Ok(())
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_exchange_request(body: &[u8]) -> Option<contract::ExchangeRequest> {
    serde_json::from_slice(body).ok()
}

fn json_body(v: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(v).expect("encode fixture response")
}

// ---------------------------------------------------------------------
// ImmediateCloud: one respond callback per request, no hold.
// ---------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Arrival {
    pub(crate) n: usize,
    pub(crate) req: contract::ExchangeRequest,
    pub(crate) auth: String,
    pub(crate) user_agent: String,
}

pub(crate) struct ImmediateCloud {
    pub(crate) base_url: String,
    arrivals_rx: Mutex<Option<mpsc::UnboundedReceiver<Arrival>>>,
    count: Arc<AtomicUsize>,
}

impl ImmediateCloud {
    /// `respond` is called for every request, in order, with its sequence
    /// number (0-based) and the decoded request body; it returns the
    /// status code and an optional pre-encoded JSON response body.
    pub(crate) async fn new<F>(respond: F) -> Self
    where
        F: Fn(usize, &contract::ExchangeRequest) -> (u16, Option<Vec<u8>>) + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_handler = Arc::clone(&count);
        let respond = Arc::new(respond);
        let handler: RawHandler = Arc::new(move |n, headers, body| {
            let tx = tx.clone();
            let count = Arc::clone(&count_for_handler);
            let respond = Arc::clone(&respond);
            let respond_result = decode_exchange_request(&body);
            Box::pin(async move {
                let auth = headers.get("authorization").cloned().unwrap_or_default();
                let user_agent = headers.get("user-agent").cloned().unwrap_or_default();
                count.fetch_add(1, Ordering::SeqCst);
                let Some(req) = respond_result else {
                    return RawResponse {
                        status: 500,
                        body: None,
                    };
                };
                let _ = tx.send(Arrival {
                    n,
                    req: req.clone(),
                    auth,
                    user_agent,
                });
                let (status, body) = respond(n, &req);
                RawResponse { status, body }
            })
        });
        let base_url = start_raw_server(handler).await;
        Self {
            base_url,
            arrivals_rx: Mutex::new(Some(rx)),
            count,
        }
    }

    pub(crate) fn take_receiver(&self) -> mpsc::UnboundedReceiver<Arrival> {
        self.arrivals_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("arrivals receiver already taken")
    }

    pub(crate) fn request_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// A cheap, `'static` way to read the live request count after `self`
    /// (or its `base_url`) has been moved into a spawned task.
    pub(crate) fn request_count_handle(&self) -> impl Fn() -> usize + use<> {
        let count = Arc::clone(&self.count);
        move || count.load(Ordering::SeqCst)
    }
}

/// A raw fake cloud that only inspects the `Authorization` header, for
/// tests exercising the 401/token-refresh path.
pub(crate) struct RawAuthCloud {
    pub(crate) base_url: String,
}

impl RawAuthCloud {
    pub(crate) async fn new<F>(respond: F) -> Self
    where
        F: Fn(&str) -> (u16, Option<Vec<u8>>) + Send + Sync + 'static,
    {
        let respond = Arc::new(respond);
        let handler: RawHandler = Arc::new(move |_n, headers, _body| {
            let respond = Arc::clone(&respond);
            Box::pin(async move {
                let auth = headers.get("authorization").cloned().unwrap_or_default();
                let (status, body) = respond(&auth);
                RawResponse { status, body }
            })
        });
        let base_url = start_raw_server(handler).await;
        Self { base_url }
    }
}

pub(crate) fn find_frame(frames: &[contract::Frame], t: &str) -> Option<contract::Frame> {
    frames.iter().find(|f| f.t == t).cloned()
}

pub(crate) fn frame_op_id(f: &contract::Frame) -> String {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(rename = "op-id", default)]
        op_id: String,
    }
    serde_json::from_value::<Body>(f.d.clone())
        .map(|b| b.op_id)
        .unwrap_or_default()
}

pub(crate) fn exchange_response_body(frames: Vec<contract::Frame>) -> Vec<u8> {
    exchange_response_with(frames, &[])
}

/// A `200` body with a run-end notice: `(run id, trace id)` pairs.
pub(crate) fn exchange_response_with(
    frames: Vec<contract::Frame>,
    ended: &[(&str, &str)],
) -> Vec<u8> {
    let ended_runs = ended
        .iter()
        .map(|(r, t)| contract::EndedRun {
            run_id: r.to_string(),
            trace_id: t.to_string(),
        })
        .collect();
    json_body(&contract::ExchangeResponse {
        v: 1,
        frames,
        ended_runs,
    })
}

pub(crate) fn build_op_frame(frame_id: &str, op_id: &str) -> contract::Frame {
    let op = contract::Op {
        op_id: op_id.to_string(),
        run_id: "run-1".to_string(),
        step_index: 0,
        kind: "http.request".to_string(),
        resource: "shop-api".to_string(),
        timeout_ms: 5000,
        args: [
            ("method".to_string(), serde_json::json!("GET")),
            ("path".to_string(), serde_json::json!("/health")),
        ]
        .into_iter()
        .collect(),
        project: vec![vec!["status".to_string()]],
        trace_id: None,
    };
    contract::Frame {
        v: 1,
        t: "op".to_string(),
        id: frame_id.to_string(),
        re: None,
        ts: 1,
        d: serde_json::to_value(op).expect("encode op"),
    }
}

// ---------------------------------------------------------------------
// Fake Handler implementations: fake, echo, capacity.
// ---------------------------------------------------------------------

/// Waits until `rx` carries `true`, checking the current value first so a
/// waiter that arrives after the gate opened doesn't block — closed-channel
/// semantics: once the test opens the gate, every future wait sees it at once.
async fn wait_gate(rx: &tokio::sync::watch::Receiver<bool>) {
    let mut rx = rx.clone();
    if *rx.borrow() {
        return;
    }
    let _ = rx.changed().await;
}

fn pass_result(op_id: &str) -> ResultFrame {
    ResultFrame {
        op_id: op_id.to_string(),
        status: "pass".to_string(),
        payload: std::collections::BTreeMap::new(),
        scrubbed: 0,
        timing: contract::Timing { exec_ms: 1 },
    }
}

/// A canned reply regardless of input, optionally pausing until `block`
/// fires and reporting every op it ran on `received`.
pub(crate) struct FakeHandler {
    pub(crate) max_inflight: i64,
    pub(crate) result: Mutex<Option<ResultFrame>>,
    pub(crate) op_err: Mutex<Option<ErrorFrame>>,
    pub(crate) block: Option<tokio::sync::watch::Receiver<bool>>,
    pub(crate) received: Option<mpsc::UnboundedSender<contract::Op>>,
}

impl FakeHandler {
    pub(crate) fn new(max_inflight: i64) -> Self {
        Self {
            max_inflight,
            result: Mutex::new(None),
            op_err: Mutex::new(None),
            block: None,
            received: None,
        }
    }

    pub(crate) fn with_result(max_inflight: i64, result: ResultFrame) -> Self {
        let h = Self::new(max_inflight);
        *h.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        h
    }

    pub(crate) fn with_error(max_inflight: i64, op_err: ErrorFrame) -> Self {
        let h = Self::new(max_inflight);
        *h.op_err.lock().unwrap_or_else(|e| e.into_inner()) = Some(op_err);
        h
    }
}

impl Handler for FakeHandler {
    fn handle(&self, op: contract::Op) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
        Box::pin(async move {
            if let Some(tx) = &self.received {
                let _ = tx.send(op);
            }
            if let Some(block) = &self.block {
                wait_gate(block).await;
            }
            let result = self
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let op_err = self
                .op_err
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            (result, op_err)
        })
    }

    fn max_inflight(&self) -> i64 {
        self.max_inflight
    }
}

/// Panics on every call to `handle`, regardless of the op. Used to prove
/// `run_op`'s `catch_unwind` guard: a panicking handler must still produce
/// a reply frame instead of the op vanishing .
pub(crate) struct PanickingHandler {
    pub(crate) max_inflight: i64,
}

impl Handler for PanickingHandler {
    fn handle(
        &self,
        _op: contract::Op,
    ) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
        Box::pin(async move { panic!("fake handler panic") })
    }

    fn max_inflight(&self) -> i64 {
        self.max_inflight
    }
}

/// Always answers pass, with the result's op-id copied from whatever op it
/// was asked to run.
pub(crate) struct EchoHandler {
    pub(crate) max_inflight: i64,
    pub(crate) block: Option<tokio::sync::watch::Receiver<bool>>,
    pub(crate) received: Option<mpsc::UnboundedSender<contract::Op>>,
}

impl Handler for EchoHandler {
    fn handle(&self, op: contract::Op) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
        Box::pin(async move {
            if let Some(tx) = &self.received {
                let _ = tx.send(op.clone());
            }
            if let Some(block) = &self.block {
                wait_gate(block).await;
            }
            (Some(pass_result(&op.op_id)), None)
        })
    }

    fn max_inflight(&self) -> i64 {
        self.max_inflight
    }
}

/// Tracks how many of its own `handle` calls are running at once, blocking
/// each on `release`, so a test can catch it at its declared max
/// concurrency.
pub(crate) struct CapacityHandler {
    pub(crate) max: i64,
    pub(crate) release_tx: tokio::sync::watch::Sender<bool>,
    release_rx: tokio::sync::watch::Receiver<bool>,
    cur: AtomicI64,
    pub(crate) high: AtomicI64,
    at_cap_tx: Mutex<Option<oneshot::Sender<()>>>,
    at_cap_rx: Mutex<Option<oneshot::Receiver<()>>>,
}

impl CapacityHandler {
    pub(crate) fn new(max: i64) -> Self {
        let (tx, rx) = oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        Self {
            max,
            release_tx,
            release_rx,
            cur: AtomicI64::new(0),
            high: AtomicI64::new(0),
            at_cap_tx: Mutex::new(Some(tx)),
            at_cap_rx: Mutex::new(Some(rx)),
        }
    }

    /// Releases every op currently (or later) blocked in `handle`, exactly
    /// once.
    pub(crate) fn release(&self) {
        let _ = self.release_tx.send(true);
    }

    pub(crate) fn take_at_cap(&self) -> oneshot::Receiver<()> {
        self.at_cap_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("at_cap receiver already taken")
    }
}

impl Handler for CapacityHandler {
    fn handle(&self, op: contract::Op) -> BoxFuture<'_, (Option<ResultFrame>, Option<ErrorFrame>)> {
        Box::pin(async move {
            let n = self.cur.fetch_add(1, Ordering::SeqCst) + 1;
            self.high.fetch_max(n, Ordering::SeqCst);
            if n == self.max
                && let Some(tx) = self
                    .at_cap_tx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
            {
                let _ = tx.send(());
            }
            wait_gate(&self.release_rx).await;
            self.cur.fetch_sub(1, Ordering::SeqCst);
            (Some(pass_result(&op.op_id)), None)
        })
    }

    fn max_inflight(&self) -> i64 {
        self.max
    }
}

// ---------------------------------------------------------------------
// PendingRx: a receiver that buffers whatever it
// pulled off the channel but didn't match what a wait call was looking
// for, so a later, differently targeted wait still finds it.
// ---------------------------------------------------------------------

pub(crate) struct PendingRx<T> {
    rx: mpsc::UnboundedReceiver<T>,
    buf: Vec<T>,
}

impl<T> PendingRx<T> {
    fn new(rx: mpsc::UnboundedReceiver<T>) -> Self {
        Self {
            rx,
            buf: Vec::new(),
        }
    }

    pub(crate) async fn wait_for(&mut self, mut matches: impl FnMut(&T) -> bool) -> T {
        if let Some(pos) = self.buf.iter().position(&mut matches) {
            return self.buf.remove(pos);
        }
        let rx = &mut self.rx;
        let buf = &mut self.buf;
        tokio::time::timeout(WAIT_TIMEOUT, async move {
            loop {
                match rx.recv().await {
                    Some(v) if matches(&v) => return v,
                    Some(v) => buf.push(v),
                    None => panic!("channel closed while waiting for a match"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out after {WAIT_TIMEOUT:?} waiting for a match"))
    }

    pub(crate) fn drain(&mut self) -> Vec<T> {
        let mut out = std::mem::take(&mut self.buf);
        while let Ok(v) = self.rx.try_recv() {
            out.push(v);
        }
        out
    }
}

// ---------------------------------------------------------------------
// SlotCloud: a fake cloud implementing one runner slot's semantics: a
// queue of op frames, an in-flight map keyed by op id (d.op-id, not the
// frame's own id), at most one open hold, and a dropped counter for
// frames that don't match anything in flight. Includes the
// test-facing waits.
// ---------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct ExchangeEvent {
    pub(crate) n: usize,
    pub(crate) status: u16,
    pub(crate) sent: Vec<contract::Frame>,
    pub(crate) before_inflight: i64,
    pub(crate) max_inflight_declared: i64,
}

struct SlotState {
    present: bool,
    queue: std::collections::VecDeque<contract::Frame>,
    inflight: HashMap<String, contract::Frame>,
    waiter: Option<oneshot::Sender<contract::Frame>>,
    outage: bool,
}

pub(crate) struct SlotCloud {
    state: Mutex<SlotState>,
    hold: Duration,
    presence_tx: tokio::sync::watch::Sender<bool>,
    presence_rx: tokio::sync::watch::Receiver<bool>,
    hold_started_tx: mpsc::UnboundedSender<()>,
    answered_tx: mpsc::UnboundedSender<String>,
    dropped_tx: mpsc::UnboundedSender<String>,
    events_tx: mpsc::UnboundedSender<ExchangeEvent>,
    dropped_count: AtomicUsize,
}

pub(crate) struct SlotCloudRx {
    pub(crate) hold_started: mpsc::UnboundedReceiver<()>,
    pub(crate) answered: PendingRx<String>,
    pub(crate) dropped: PendingRx<String>,
    pub(crate) events: PendingRx<ExchangeEvent>,
}

impl SlotCloud {
    pub(crate) async fn new(hold: Duration) -> (Arc<SlotCloud>, String, SlotCloudRx) {
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(false);
        let (hold_started_tx, hold_started_rx) = mpsc::unbounded_channel();
        let (answered_tx, answered_rx) = mpsc::unbounded_channel();
        let (dropped_tx, dropped_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        let sc = Arc::new(SlotCloud {
            state: Mutex::new(SlotState {
                present: false,
                queue: std::collections::VecDeque::new(),
                inflight: HashMap::new(),
                waiter: None,
                outage: false,
            }),
            hold,
            presence_tx,
            presence_rx,
            hold_started_tx,
            answered_tx,
            dropped_tx,
            events_tx,
            dropped_count: AtomicUsize::new(0),
        });

        let sc_for_handler = Arc::clone(&sc);
        let handler: RawHandler = Arc::new(move |n, _headers, body| {
            let sc = Arc::clone(&sc_for_handler);
            Box::pin(async move { sc.handle_request(n, body).await })
        });
        let base_url = start_raw_server(handler).await;

        (
            sc,
            base_url,
            SlotCloudRx {
                hold_started: hold_started_rx,
                answered: PendingRx::new(answered_rx),
                dropped: PendingRx::new(dropped_rx),
                events: PendingRx::new(events_rx),
            },
        )
    }

    pub(crate) async fn wait_presence(&self) {
        wait_gate(&self.presence_rx).await;
    }

    pub(crate) fn dropped_count(&self) -> usize {
        self.dropped_count.load(Ordering::SeqCst)
    }

    pub(crate) fn set_outage(&self, v: bool) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).outage = v;
    }

    /// Adds an op frame for delivery: handed straight to an open hold if
    /// one is waiting, otherwise appended to the queue for the next drain.
    /// Either way the frame enters the in-flight map here, keyed by its op
    /// id, before the runner has even seen it, so a drain racing a
    /// completion never miscounts.
    pub(crate) fn enqueue(&self, frame: contract::Frame) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = st.waiter.take() {
            st.inflight.insert(frame_op_id(&frame), frame.clone());
            drop(st);
            let _ = tx.send(frame);
            return;
        }
        st.queue.push_back(frame);
    }

    /// Simulates a cloud restart: presence, in-flight and any open hold are
    /// cleared, and the logical op is re-enqueued as `new_frame`, carrying
    /// a fresh op id (a new op, not a retry of the old one). Anything
    /// already in flight under the old op id is now unrecognised — a late
    /// answer for it is dropped, never double-answered.
    pub(crate) fn restart(&self, new_frame: contract::Frame) {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.present = false;
            st.inflight.clear();
            st.waiter = None;
        }
        self.enqueue(new_frame);
    }

    /// Removes an op from in-flight as if the cloud gave up waiting for it:
    /// any later answer for it is now unrecognised and counted dropped.
    pub(crate) fn timeout_op(&self, op_id: &str) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inflight
            .remove(op_id);
    }

    fn complete_frame(&self, f: contract::Frame) {
        if f.t != "result" && f.t != "error" {
            return;
        }
        let id = frame_op_id(&f);
        let matched = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.inflight.remove(&id).is_some()
        };
        if matched {
            let _ = self.answered_tx.send(id);
        } else {
            self.dropped_count.fetch_add(1, Ordering::SeqCst);
            let _ = self.dropped_tx.send(id);
        }
    }

    fn try_drain(&self, max_inflight_declared: i64) -> (i64, Vec<contract::Frame>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = st.inflight.len() as i64;
        let capacity = (max_inflight_declared - before).max(1);
        let mut sent = Vec::new();
        while (sent.len() as i64) < capacity {
            let Some(f) = st.queue.pop_front() else {
                break;
            };
            st.inflight.insert(frame_op_id(&f), f.clone());
            sent.push(f);
        }
        (before, sent)
    }

    /// Parks the request until [`SlotCloud::enqueue`] hands it an op
    /// directly, or the hold elapses. Only one hold is ever open at a
    /// time: a second request that finds one already parked is answered
    /// at once rather than parking too.
    async fn await_op(&self) -> Option<contract::Frame> {
        let (tx, mut rx) = oneshot::channel();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.waiter.is_some() {
                return None;
            }
            st.waiter = Some(tx);
        }
        let _ = self.hold_started_tx.send(());

        let sleep = tokio::time::sleep(self.hold);
        tokio::pin!(sleep);
        tokio::select! {
            res = &mut rx => res.ok(),
            () = &mut sleep => {
                self.state.lock().unwrap_or_else(|e| e.into_inner()).waiter = None;
                // enqueue may have raced the timer, handing off the frame
                // just as it fired: give the receiver one more, non-blocking
                // chance before giving up.
                rx.try_recv().ok()
            }
        }
    }

    fn respond(
        &self,
        n: usize,
        max_inflight_declared: i64,
        before: i64,
        sent: Vec<contract::Frame>,
    ) -> RawResponse {
        let status: u16 = if sent.is_empty() { 204 } else { 200 };
        let body = if sent.is_empty() {
            None
        } else {
            Some(exchange_response_body(sent.clone()))
        };
        let _ = self.events_tx.send(ExchangeEvent {
            n,
            status,
            sent,
            before_inflight: before,
            max_inflight_declared,
        });
        RawResponse { status, body }
    }

    async fn handle_request(self: Arc<Self>, n: usize, body: Vec<u8>) -> RawResponse {
        let outage = self.state.lock().unwrap_or_else(|e| e.into_inner()).outage;
        if outage {
            return RawResponse {
                status: 503,
                body: None,
            };
        }
        let Some(req) = decode_exchange_request(&body) else {
            return RawResponse {
                status: 500,
                body: None,
            };
        };
        for f in req.frames {
            self.complete_frame(f);
        }

        let first = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let first = !st.present;
            st.present = true;
            first
        };
        if first {
            let _ = self.presence_tx.send(true);
        }

        let (before, mut sent) = self.try_drain(req.runner.max_inflight);
        if !first
            && sent.is_empty()
            && let Some(f) = self.await_op().await
        {
            sent = vec![f];
        }
        self.respond(n, req.runner.max_inflight, before, sent)
    }
}
