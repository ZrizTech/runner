//! One measured HTTP client for each http.request op in work.
//!
//! The target time of an http op must not hold the open of a connection
//! (DNS, TCP, TLS). `hyper-util` races the pool checkout against a new
//! connect and finishes the loser in the background, so a connect can be one
//! that no request waited for. So each op takes its own client out of a
//! per-resource idle set. A connector layer notes, for each new connection,
//! its two socket addresses and how long the connect took. After a hop, the
//! op reads the same two addresses from the response
//! (`hyper_util` `HttpInfo`, set by `Connection::connected`) and is charged
//! only for the connect of a connection that one of its own requests ran on.
//!
//! What the layer covers (reqwest 0.12.28, `src/connect.rs`):
//! `ClientBuilder::connector_layer` wraps the base `ConnectorService`
//! (`connect.rs:90-140`, the layers loop at 137). Its `call` (line 928)
//! does the proxy tunnel (`connect_via_proxy`, line 783) or
//! `connect_with_maybe_proxy` (line 613). That one runs the TCP connect
//! with DNS and then, for rustls, the TLS handshake (`Inner::RustlsTls`,
//! lines 664-684). So DNS, TCP, TLS and proxy tunnels are all inside the
//! layer. Only `connect_timeout` sits outside it. The response of the layer
//! is reqwest's `Conn`; its `Connection::connected` (`connect.rs:1307`, and
//! `:1664` for rustls) gives the `HttpInfo` of the TCP stream
//! (`hyper-util` `connect/http.rs:586-596`), also under TLS.

use hyper_util::client::legacy::connect::{Connection, HttpInfo};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;
use tower_layer::Layer;
use tower_service::Service;

/// The most idle clients kept for one resource; an extra one is dropped.
const MAX_IDLE: usize = 8;
/// The most connects noted for one client; when full, the notes are cleared.
const MAX_NOTES: usize = 64;

/// The identity of a connection: local and remote socket address.
type ConnId = (SocketAddr, SocketAddr);
/// How long a connect took, and when it ended.
type Note = (Duration, Instant);
type Notes = Arc<Mutex<HashMap<ConnId, Note>>>;

/// A client and the notes of its connects.
#[derive(Clone)]
pub(super) struct Measured {
    client: reqwest::Client,
    notes: Notes,
}

pub(super) fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
}

/// Adds the measuring layer to `builder` and builds the client.
pub(super) fn measured(builder: reqwest::ClientBuilder) -> Result<Measured, reqwest::Error> {
    let notes = Notes::default();
    let client = builder
        .connector_layer(MeasureLayer(notes.clone()))
        .build()?;
    Ok(Measured { client, notes })
}

/// A client with the options of the old shared client plus the measuring layer.
fn build_measured() -> Result<Measured, reqwest::Error> {
    measured(base_builder())
}

#[derive(Clone)]
struct MeasureLayer(Notes);

impl<S> Layer<S> for MeasureLayer {
    type Service = MeasureService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MeasureService {
            inner,
            notes: self.0.clone(),
        }
    }
}

#[derive(Clone)]
struct MeasureService<S> {
    inner: S,
    notes: Notes,
}

type BoxFut<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

/// The identity of a new connection, if its transport tells it.
fn conn_id<C: Connection>(conn: &C) -> Option<ConnId> {
    let mut ext = http::Extensions::new();
    conn.connected().get_extras(&mut ext);
    ext.get::<HttpInfo>()
        .map(|i| (i.local_addr(), i.remote_addr()))
}

fn note(notes: &Notes, id: ConnId, took: Duration) {
    let mut map = notes.lock().unwrap_or_else(PoisonError::into_inner);
    if map.len() >= MAX_NOTES {
        map.clear();
    }
    map.insert(id, (took, Instant::now()));
}

impl<S, R> Service<R> for MeasureService<S>
where
    S: Service<R>,
    S::Future: Send + 'static,
    S::Response: Connection + 'static,
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
        let notes = self.notes.clone();
        Box::pin(async move {
            let start = Instant::now();
            let conn = fut.await?;
            if let Some(id) = conn_id(&conn) {
                note(&notes, id, start.elapsed());
            }
            Ok(conn)
        })
    }
}

type Builder = Arc<dyn Fn() -> Result<Measured, reqwest::Error> + Send + Sync>;

/// The idle measured clients of each http resource.
pub(super) struct Clients {
    idle: Mutex<HashMap<String, Vec<Measured>>>,
    builder: Builder,
}

impl Default for Clients {
    fn default() -> Self {
        Self::with_builder(Arc::new(build_measured))
    }
}

impl Clients {
    /// Clients made by `builder` (tests inject a slow connector).
    pub(super) fn with_builder(builder: Builder) -> Self {
        Self {
            idle: Mutex::default(),
            builder,
        }
    }

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
            None => (self.builder)()?,
        };
        Ok(Lease {
            clients: self,
            resource: resource.to_string(),
            measured,
            done: false,
            charged: Duration::ZERO,
        })
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
/// drops the future), the client may have a connect in flight, so it is
/// dropped, not kept.
pub(super) struct Lease<'a> {
    clients: &'a Clients,
    resource: String,
    measured: Measured,
    done: bool,
    /// The connect time of the connections this op's requests ran on.
    charged: Duration,
}

impl Lease<'_> {
    pub(super) fn client(&self) -> &reqwest::Client {
        &self.measured.client
    }

    /// The op ended, with a result or an error: the client may be kept.
    pub(super) fn finish(&mut self) {
        self.done = true;
    }

    /// Charges the op for the connect of the connection `resp` came on, if
    /// that connect was new. `sent_at` is when the hop started: a connect
    /// that ended before it was not waited for. The charge is at most the
    /// time since `sent_at`.
    pub(super) fn charge(&mut self, resp: &reqwest::Response, sent_at: Instant) {
        let Some(info) = resp.extensions().get::<HttpInfo>() else {
            return;
        };
        let id = (info.local_addr(), info.remote_addr());
        let note = self
            .measured
            .notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some((took, ended)) = note
            && ended >= sent_at
        {
            self.charged += took.min(sent_at.elapsed());
        }
    }

    /// `waited` without the connects this op was charged for, floored at 0.
    pub(super) fn target(&self, waited: Duration) -> Duration {
        waited.saturating_sub(self.charged)
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.done {
            self.clients.put_back(&self.resource, self.measured.clone());
        }
    }
}
