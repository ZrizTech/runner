//! One measured HTTP client for each http.request op in work.
//!
//! The target time of an http op must not hold the open of a connection
//! (DNS, TCP, TLS). `hyper-util` races the pool checkout against a new
//! connect and finishes the loser in the background, so with one shared
//! client a connect cannot be given to the op that caused it. So each op
//! takes its own client out of a per-resource idle set, uses it for every
//! hop, and puts it back. The connector of such a client is wrapped by a
//! layer that adds the duration of each connect to a counter of the client.
//! Only one op uses a client at a time, so every connect during the op is
//! the op's.
//!
//! What the layer covers (reqwest 0.12.28, `src/connect.rs`):
//! `ClientBuilder::connector_layer` wraps the base `ConnectorService`
//! (`connect.rs:90-140`, the layers loop at 137). Its `call` (line 928)
//! does the proxy tunnel (`connect_via_proxy`, line 783) or
//! `connect_with_maybe_proxy` (line 613). That one runs the TCP connect
//! with DNS and then, for rustls, the TLS handshake (`Inner::RustlsTls`,
//! lines 664-684). So DNS, TCP, TLS and proxy tunnels are all inside the
//! layer. Only `connect_timeout` sits outside it.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tower_layer::Layer;
use tower_service::Service;

/// The most idle clients kept for one resource; an extra one is dropped.
const MAX_IDLE: usize = 8;

/// A client and the counter of its connect time, in nanoseconds.
pub(super) struct Measured {
    client: reqwest::Client,
    connect_ns: Arc<AtomicU64>,
}

fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
}

fn finish(builder: reqwest::ClientBuilder, ns: Arc<AtomicU64>) -> Result<Measured, reqwest::Error> {
    let client = builder.connector_layer(MeasureLayer(ns.clone())).build()?;
    Ok(Measured {
        client,
        connect_ns: ns,
    })
}

/// Builds a client with the options of the old shared client plus the
/// measuring layer.
pub(super) fn build_measured() -> Result<Measured, reqwest::Error> {
    finish(base_builder(), Arc::new(AtomicU64::new(0)))
}

/// Test only: as [`build_measured`], with a layer under the measuring layer
/// that sleeps `delay` in each connect (a slow network).
#[cfg(test)]
pub(super) fn build_measured_slow(delay: Duration) -> Result<Measured, reqwest::Error> {
    finish(
        base_builder().connector_layer(DelayLayer(delay)),
        Arc::new(AtomicU64::new(0)),
    )
}

#[derive(Clone)]
struct MeasureLayer(Arc<AtomicU64>);

impl<S> Layer<S> for MeasureLayer {
    type Service = MeasureService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MeasureService {
            inner,
            ns: self.0.clone(),
        }
    }
}

#[derive(Clone)]
struct MeasureService<S> {
    inner: S,
    ns: Arc<AtomicU64>,
}

/// Adds the time since `start` to the counter when dropped, so a connect
/// that is cancelled is counted up to the cancel.
struct CountOnDrop {
    start: Instant,
    ns: Arc<AtomicU64>,
}

impl Drop for CountOnDrop {
    fn drop(&mut self) {
        let ns = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.ns.fetch_add(ns, Ordering::Relaxed);
    }
}

type BoxFut<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S, R> Service<R> for MeasureService<S>
where
    S: Service<R>,
    S::Future: Send + 'static,
    S::Response: 'static,
    S::Error: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFut<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let fut = self.inner.call(req);
        let ns = self.ns.clone();
        Box::pin(async move {
            let _count = CountOnDrop {
                start: Instant::now(),
                ns,
            };
            fut.await
        })
    }
}

#[cfg(test)]
#[derive(Clone)]
struct DelayLayer(Duration);

#[cfg(test)]
impl<S> Layer<S> for DelayLayer {
    type Service = DelayService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        DelayService {
            inner,
            delay: self.0,
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct DelayService<S> {
    inner: S,
    delay: Duration,
}

#[cfg(test)]
impl<S, R> Service<R> for DelayService<S>
where
    S: Service<R>,
    S::Future: Send + 'static,
    S::Response: 'static,
    S::Error: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFut<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let fut = self.inner.call(req);
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            fut.await
        })
    }
}

/// The idle measured clients of each http resource.
#[derive(Default)]
pub(super) struct Clients {
    idle: Mutex<HashMap<String, Vec<Measured>>>,
    /// Test only: the slow connect that new clients get.
    #[cfg(test)]
    pub(super) slow_connect: Mutex<Option<Duration>>,
}

impl Clients {
    /// Takes the newest idle client of `resource` (its connections are the
    /// warmest), or builds one. Call it before any clock of target time.
    pub(super) fn lease(&self, resource: &str) -> Result<Lease<'_>, reqwest::Error> {
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(resource)
            .and_then(Vec::pop);
        let measured = match idle {
            Some(m) => m,
            None => self.build()?,
        };
        Ok(Lease {
            clients: self,
            resource: resource.to_string(),
            measured: Some(measured),
            done: false,
            waited: Duration::ZERO,
            connect_start: 0,
        }
        .started())
    }

    fn build(&self) -> Result<Measured, reqwest::Error> {
        #[cfg(test)]
        if let Some(d) = *self
            .slow_connect
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return build_measured_slow(d);
        }
        build_measured()
    }

    fn put_back(&self, resource: &str, m: Measured) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        let set = idle.entry(resource.to_string()).or_default();
        if set.len() < MAX_IDLE {
            set.push(m);
        }
    }

    #[cfg(test)]
    pub(super) fn idle_count(&self, resource: &str) -> usize {
        let idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        idle.get(resource).map_or(0, Vec::len)
    }
}

/// A client out of the idle set for the length of one op. It goes back on
/// drop, also after an error. If the op was cut before `finish` (a timeout
/// drops the future), the client may have a connect in flight that would
/// count into the next op, so it is dropped, not kept.
pub(super) struct Lease<'a> {
    clients: &'a Clients,
    resource: String,
    measured: Option<Measured>,
    done: bool,
    /// The time of the hops: send to end of the body read, connects included.
    pub(super) waited: Duration,
    connect_start: u64,
}

impl Lease<'_> {
    fn started(mut self) -> Self {
        self.connect_start = self.connect_ns();
        self
    }

    pub(super) fn client(&self) -> Option<&reqwest::Client> {
        self.measured.as_ref().map(|m| &m.client)
    }

    fn connect_ns(&self) -> u64 {
        self.measured
            .as_ref()
            .map_or(0, |m| m.connect_ns.load(Ordering::Relaxed))
    }

    /// The op ended, with a result or an error: the client may be kept.
    pub(super) fn finish(&mut self) {
        self.done = true;
    }

    /// `waited` without the connects of this op, floored at 0.
    pub(super) fn target(&self) -> Duration {
        let connect = Duration::from_nanos(self.connect_ns().saturating_sub(self.connect_start));
        self.waited.saturating_sub(connect)
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.done
            && let Some(m) = self.measured.take()
        {
            self.clients.put_back(&self.resource, m);
        }
    }
}
