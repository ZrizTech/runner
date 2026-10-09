//! Client for the worker sidecar: one JSON line out and one line back over
//! a unix socket, per connection. The runner only ever connects out.

use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Most bytes accepted in one response line.
const MAX_RESPONSE_BYTES: u64 = 4 << 20;

const PING_TIMEOUT: Duration = Duration::from_secs(1);

const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The worker could not be reached or spoke garbage.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unavailable;

/// Sends `req` to the worker at `socket` and returns its one response line
/// parsed as JSON. Fails with [`Unavailable`] on any transport error or
/// when `deadline` passes.
pub(crate) async fn call(
    socket: &str,
    req: &Value,
    deadline: Duration,
) -> Result<Value, Unavailable> {
    let work = async {
        let mut stream = UnixStream::connect(socket).await.map_err(|_| Unavailable)?;
        let mut line = serde_json::to_vec(req).map_err(|_| Unavailable)?;
        line.push(b'\n');
        stream.write_all(&line).await.map_err(|_| Unavailable)?;
        stream.flush().await.map_err(|_| Unavailable)?;
        let mut reader = BufReader::new(stream.take(MAX_RESPONSE_BYTES));
        let mut resp = String::new();
        let n = reader.read_line(&mut resp).await.map_err(|_| Unavailable)?;
        if n == 0 {
            return Err(Unavailable);
        }
        serde_json::from_str::<Value>(&resp).map_err(|_| Unavailable)
    };
    tokio::time::timeout(deadline, work)
        .await
        .map_err(|_| Unavailable)?
}

/// The ping response when the worker answers a good ping, else `None`.
pub(crate) async fn ping_info(socket: &str) -> Option<Value> {
    let req = json!({"v": 1, "kind": "ping"});
    let v = call(socket, &req, PING_TIMEOUT).await.ok()?;
    (v.get("ok") == Some(&Value::Bool(true)) && v.get("kind") == Some(&json!("ping"))).then_some(v)
}

/// True when the worker answers a ping.
pub(crate) async fn ping(socket: &str) -> bool {
    ping_info(socket).await.is_some()
}

/// Tells the worker that a run ended; it closes the contexts and handles of
/// that run. `Err` when the worker is not reachable.
pub(crate) async fn run_close(
    socket: &str,
    run_id: &str,
    trace_id: &str,
) -> Result<(), Unavailable> {
    let req = json!({"v": 1, "kind": "run.close", "run": run_id, "trace-id": trace_id});
    call(socket, &req, CLOSE_TIMEOUT).await.map(|_| ())
}
