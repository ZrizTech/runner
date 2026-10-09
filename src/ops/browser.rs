//! Implements the browser.page op kind: check and substitute the commands,
//! hand them to the worker sidecar, then capture, project and scrub what
//! comes back, in the same order as http.

use super::worker_map::{from_worker, restarted};
use super::{Handler, capture, duration_ms, new_error, runner_error, scrub_payload, worker};
use crate::config::Resource;
use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame, Timing};
use crate::{origin, placeholder, project};
use serde_json::{Map, Value, json};
use std::time::Duration;

/// Deadline sent to the worker when the op carries none.
const DEFAULT_DEADLINE_MS: i64 = 600_000;

/// The worker's `idle-ms` when the resource sets none (10 minutes).
pub(super) const DEFAULT_IDLE_MS: u64 = 600_000;

impl Handler {
    pub(crate) async fn browser_page(
        &self,
        op: &contract::Op,
    ) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        match self.browser_page_inner(op).await {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e)),
        }
    }

    async fn browser_page_inner(
        &self,
        op: &contract::Op,
    ) -> std::result::Result<ResultFrame, ErrorFrame> {
        let resource = self.resource(&op.op_id, &op.resource, "browser")?;
        let socket = self.cfg.worker_socket.as_str();
        if socket.is_empty() {
            return Err(unavailable(&op.op_id));
        }
        let (args, mut secrets) = self.substitute_browser(op, &resource)?;
        check_gotos(&op.op_id, &resource, &args)?;
        // A new `boot-id` of the worker: the context of this run is gone.
        if self
            .pair_lost(&self.contexts, &op.run_id, &op.resource)
            .await
        {
            return Err(restarted(op));
        }

        let deadline_ms = if op.timeout_ms > 0 {
            op.timeout_ms.min(DEFAULT_DEADLINE_MS)
        } else {
            DEFAULT_DEADLINE_MS
        };
        let mut req = json!({
            "v": 1,
            "op-id": op.op_id,
            "run": op.run_id,
            "kind": "browser.page",
            "policy": policy(&op.resource, &resource),
            "args": args,
            "deadline-ms": deadline_ms,
        });
        if let Some(t) = op.valid_trace_id() {
            req["trace-id"] = json!(t);
        }

        let start = (self.now)();
        let wait = Duration::from_millis(u64::try_from(deadline_ms).unwrap_or(0) + 2000);
        // The op may make a context, even if it ends in an error.
        self.contexts.add(&op.run_id, &op.resource);
        let resp = worker::call(socket, &req, wait).await.map_err(|_| {
            self.mark_worker_down();
            unavailable(&op.op_id)
        })?;
        let exec_ms = duration_ms((self.now)(), start);

        let out = worker_out(op, &resp, resource.idle_ms.unwrap_or(DEFAULT_IDLE_MS))?;
        let status = if out.get("ok") == Some(&Value::Bool(false)) {
            "fail"
        } else {
            "pass"
        };
        let caps = self.apply_capture(op, &out, &mut secrets)?;
        let mut payload = project::select_paths(&out, &op.project);
        capture::mask(&mut payload, &caps);
        let (scrubbed, count) = scrub_payload(&payload, &secrets)
            .map_err(|_| runner_error(&op.op_id, "response-encoding"))?;
        Ok(ResultFrame {
            op_id: op.op_id.clone(),
            status: status.to_string(),
            payload: scrubbed,
            scrubbed: count,
            timing: Timing { exec_ms },
        })
    }

    /// Substitutes secrets into the `fill`/`press-seq` values only, and
    /// only for names in the resource's `secrets` (or the run's vault).
    /// A placeholder anywhere else is refused.
    fn substitute_browser(
        &self,
        op: &contract::Op,
        resource: &Resource,
    ) -> std::result::Result<(Map<String, Value>, Vec<String>), ErrorFrame> {
        let allow: &[String] = resource.secrets.as_deref().unwrap_or(&[]);
        let rest: Map<String, Value> = op
            .args
            .iter()
            .filter(|(k, _)| k.as_str() != "commands")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let commands = op.args.get("commands").cloned().unwrap_or(Value::Null);
        // No other arg key takes a placeholder.
        let others = self.with_lookup(op, allow, |l| {
            placeholder::substitute("browser.page", &rest, l)
        })?;
        let cmds = self.with_lookup(op, allow, |l| {
            placeholder::substitute_commands(&commands, l)
        })?;
        let mut args = others.args;
        args.extend(cmds.args);
        let mut secrets = others.secrets;
        secrets.extend(cmds.secrets);
        Ok((args, secrets))
    }

    pub(super) fn mark_worker_down(&self) {
        self.worker_up
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) fn unavailable(op_id: &str) -> ErrorFrame {
    new_error(op_id, "worker-unavailable", json!({}))
}

/// Every `goto` path must stay on the resource's origin.
fn check_gotos(
    op_id: &str,
    resource: &Resource,
    args: &Map<String, Value>,
) -> std::result::Result<(), ErrorFrame> {
    let Some(Value::Array(cmds)) = args.get("commands") else {
        return Err(runner_error(op_id, "browser-args"));
    };
    for c in cmds {
        if c.get("do").and_then(Value::as_str) != Some("goto") {
            continue;
        }
        let path = c.get("path").and_then(Value::as_str).unwrap_or("");
        if !path.starts_with('/') || origin::check(&resource.base_url, path).is_err() {
            return Err(new_error(op_id, "host-not-allowed", json!({})));
        }
    }
    Ok(())
}

/// The policy the worker enforces: the resource's own origin first, then
/// the configured extras.
fn policy(name: &str, r: &Resource) -> Value {
    let own = url::Url::parse(&r.base_url)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default();
    let mut origins = vec![own.clone()];
    for o in &r.origins {
        if !origins.contains(o) {
            origins.push(o.clone());
        }
    }
    let mut p = Map::new();
    p.insert("resource".into(), json!(name));
    p.insert("base-url".into(), json!(own));
    p.insert("origins".into(), json!(origins));
    if let Some((width, height)) = r.viewport {
        p.insert("viewport".into(), json!({"width": width, "height": height}));
    }
    if let Some(n) = r.max_contexts {
        p.insert("max-contexts".into(), json!(n));
    }
    if let Some(n) = r.idle_ms {
        p.insert("idle-ms".into(), json!(n));
    }
    Value::Object(p)
}

/// The `out` object of a good response, or the error a bad one maps to.
pub(super) fn worker_out(
    op: &contract::Op,
    resp: &Value,
    idle_ms: u64,
) -> std::result::Result<Map<String, Value>, ErrorFrame> {
    if resp.get("ok") == Some(&Value::Bool(true))
        && let Some(Value::Object(out)) = resp.get("out")
    {
        return Ok(out.clone());
    }
    Err(from_worker(op, resp, idle_ms))
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "browser_restart_tests.rs"]
mod restart_tests;
