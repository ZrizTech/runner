//! Implements the cli.exec op kind: resolve the declared command, check the
//! argv tail against its shapes, resolve env secrets, hand the op to the
//! worker sidecar, then capture, project and scrub what comes back.

use super::browser::unavailable;
use super::{Handler, capture, duration_ms, new_error, scrub_payload, worker};
use crate::config::Resource;
use crate::config_cli::CliCommand;
use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame, Timing};
use crate::{argshape, placeholder, project};
use serde_json::{Map, Value, json};
use std::time::Duration;

const DEFAULT_DEADLINE_MS: i64 = 600_000;
const MODES: [&str; 5] = ["run", "start", "read", "wait", "stop"];

impl Handler {
    pub(crate) async fn cli_exec(
        &self,
        op: &contract::Op,
    ) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        match self.cli_exec_inner(op).await {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e)),
        }
    }

    async fn cli_exec_inner(
        &self,
        op: &contract::Op,
    ) -> std::result::Result<ResultFrame, ErrorFrame> {
        let resource = self.resource(&op.op_id, &op.resource, "cli")?;
        let socket = self.cfg.worker_socket.as_str();
        if socket.is_empty() {
            return Err(unavailable(&op.op_id));
        }
        // Placeholders: `env` values only, names from the resource `secrets`
        // or the run's vault. Any other key holding one is refused here.
        let allow: &[String] = resource.secrets.as_deref().unwrap_or(&[]);
        let all: Map<String, Value> = op
            .args
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let sub = self.with_lookup(op, allow, |l| placeholder::substitute("cli.exec", &all, l))?;
        let mut secrets = sub.secrets;
        let mode = sub
            .args
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("run");
        let handle = sub.args.get("handle").and_then(Value::as_str);
        let by_handle = matches!(mode, "read" | "wait" | "stop");
        // read/wait/stop name a process by its handle only; the command comes
        // from the `start` that opened it. An unknown handle is refused here.
        let remembered = match handle {
            Some(h) if by_handle && handle_ok(h) => Some(
                self.handles
                    .get(&op.run_id, &op.resource, h, (self.now)())
                    .ok_or_else(|| not_allowed(&op.op_id, "handle is unknown"))?,
            ),
            _ if by_handle => {
                return Err(not_allowed(&op.op_id, "handle is missing or malformed"));
            }
            _ => None,
        };
        let (cmd_name, cmd, wargs) =
            checked(&op.op_id, &resource, &sub.args, remembered.as_deref())?;

        let deadline_ms = if op.timeout_ms > 0 {
            op.timeout_ms.min(DEFAULT_DEADLINE_MS)
        } else {
            DEFAULT_DEADLINE_MS
        };
        let mut req = json!({
            "v": 1,
            "op-id": op.op_id,
            "run": op.run_id,
            "kind": "cli.exec",
            "policy": policy(&op.resource, &cmd_name, cmd, &resource),
            "args": wargs,
            "deadline-ms": deadline_ms,
        });
        if let Some(t) = op.valid_trace_id() {
            req["trace-id"] = json!(t);
        }

        let start = (self.now)();
        let wait = Duration::from_millis(u64::try_from(deadline_ms).unwrap_or(0) + 2000);
        let resp = worker::call(socket, &req, wait).await.map_err(|_| {
            self.mark_worker_down();
            unavailable(&op.op_id)
        })?;
        let exec_ms = duration_ms((self.now)(), start);

        let mut out = worker_out(&op.op_id, &resp)?;
        if let Some(h) = handle {
            let finished = mode == "stop"
                || mode == "wait"
                    && out.contains_key("exit-code")
                    && out.get("running") != Some(&Value::Bool(true));
            if mode == "start" {
                self.handles
                    .put(&op.run_id, &op.resource, h, &cmd_name, (self.now)());
            } else if finished {
                self.handles.forget(&op.run_id, &op.resource, h);
            }
        }
        if sub.args.get("stdout").and_then(Value::as_str) == Some("json") {
            parse_stdout_json(&mut out);
        }
        let caps = self.apply_capture(op, &out, &mut secrets)?;
        let mut payload = project::select_paths(&out, &op.project);
        capture::mask(&mut payload, &caps);
        let (scrubbed, count) = scrub_payload(&payload, &secrets)
            .map_err(|_| new_error(&op.op_id, "runner-error", "response encoding failed"))?;
        Ok(ResultFrame {
            op_id: op.op_id.clone(),
            status: "pass".to_string(),
            payload: scrubbed,
            scrubbed: count,
            timing: Timing { exec_ms },
        })
    }
}

fn not_allowed(op_id: &str, what: &str) -> ErrorFrame {
    new_error(op_id, "arg-not-allowed", what)
}

/// Checks mode, command, argv shape, handle and env names against the
/// declared command. Returns the command and the args for the worker.
fn checked<'a>(
    op_id: &str,
    r: &'a Resource,
    args: &Map<String, Value>,
    remembered: Option<&str>,
) -> std::result::Result<(String, &'a CliCommand, Map<String, Value>), ErrorFrame> {
    let mode = match args.get("mode") {
        None => "run",
        Some(Value::String(m)) if MODES.contains(&m.as_str()) => m.as_str(),
        _ => return Err(not_allowed(op_id, "mode is not allowed")),
    };
    let (name, cmd) = pick_command(op_id, r, args.get("command"), remembered)?;

    let mut argv = Vec::new();
    if let Some(v) = args.get("args") {
        let Value::Array(items) = v else {
            return Err(not_allowed(op_id, "args must be a list"));
        };
        for i in items {
            let Some(s) = i.as_str() else {
                return Err(not_allowed(op_id, "args must be strings"));
            };
            argv.push(s.to_string());
        }
    }
    let runs = matches!(mode, "run" | "start");
    if runs && !argshape::matches(&cmd.shapes, &argv) || !runs && !argv.is_empty() {
        return Err(not_allowed(op_id, "argv does not match a declared shape"));
    }

    let mut w = Map::new();
    w.insert("mode".into(), json!(mode));
    if runs {
        w.insert("args".into(), json!(argv));
    }
    match args.get("handle") {
        Some(Value::String(h)) if handle_ok(h) => {
            w.insert("handle".into(), json!(h));
        }
        None if mode == "run" => {}
        _ => return Err(not_allowed(op_id, "handle is missing or malformed")),
    }
    for key in ["until", "extract"] {
        if let Some(v) = args.get(key) {
            w.insert(key.into(), v.clone());
        }
    }
    if let Some(env) = args.get("env") {
        let Value::Object(m) = env else {
            return Err(not_allowed(op_id, "env must be an object"));
        };
        if m.iter()
            .any(|(k, v)| !cmd.env_allow.contains(k) || !v.is_string())
        {
            return Err(not_allowed(op_id, "env name is not allowed"));
        }
        w.insert("env".into(), env.clone());
    }
    match args.get("stdout") {
        None => {}
        Some(Value::String(s)) if s == "json" || s == "text" => {}
        _ => return Err(not_allowed(op_id, "stdout must be json or text")),
    }
    Ok((name, cmd, w))
}

fn handle_ok(h: &str) -> bool {
    (1..=32).contains(&h.len())
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The declared command an op names. A read/wait/stop of a handle takes the
/// command remembered from its `start`; a `command` it names must match.
fn pick_command<'a>(
    op_id: &str,
    r: &'a Resource,
    named: Option<&Value>,
    remembered: Option<&str>,
) -> std::result::Result<(String, &'a CliCommand), ErrorFrame> {
    let refused = || new_error(op_id, "command-not-allowed", "command is not declared");
    let name = match (named, remembered) {
        (Some(Value::String(n)), Some(m)) if n != m => {
            return Err(not_allowed(op_id, "command does not match the handle"));
        }
        (Some(Value::String(n)), _) => n.as_str(),
        (None, Some(m)) => m,
        _ => return Err(refused()),
    };
    r.commands
        .get_key_value(name)
        .map(|(k, c)| (k.clone(), c))
        .ok_or_else(refused)
}

/// The policy the worker enforces, built from the declared command.
fn policy(resource_id: &str, name: &str, c: &CliCommand, r: &Resource) -> Value {
    let mut p = Map::new();
    p.insert("resource".into(), json!(resource_id));
    p.insert("command".into(), json!(name));
    p.insert("path".into(), json!(c.path));
    p.insert("argv-prefix".into(), json!(c.argv_prefix));
    p.insert("env".into(), json!(c.env));
    p.insert("cwd".into(), json!("run"));
    p.insert("timeout-ms".into(), json!(c.timeout_ms));
    p.insert("max-life-ms".into(), json!(c.max_life_ms));
    p.insert("max-output-bytes".into(), json!(c.max_output_bytes));
    if let Some(n) = r.max_handles {
        p.insert("max-handles".into(), json!(n));
    }
    if let Some(n) = r.idle_ms {
        p.insert("idle-ms".into(), json!(n));
    }
    Value::Object(p)
}

/// The `out` object of a good response, or the error a bad one maps to.
/// Messages are fixed strings naming the reason, never worker free text.
fn worker_out(op_id: &str, resp: &Value) -> std::result::Result<Map<String, Value>, ErrorFrame> {
    if resp.get("ok") == Some(&Value::Bool(true))
        && let Some(Value::Object(out)) = resp.get("out")
    {
        return Ok(out.clone());
    }
    Err(match resp.get("reason").and_then(Value::as_str) {
        Some("at-capacity") => new_error(op_id, "runner-at-capacity", "worker is at capacity"),
        Some("handle-busy") => new_error(op_id, "runner-error", "cli refused: handle-busy"),
        Some("no-handle") => new_error(op_id, "runner-error", "cli refused: no-handle"),
        Some("too-many-handles") => {
            new_error(op_id, "runner-error", "cli refused: too-many-handles")
        }
        Some("spawn-failed") => new_error(op_id, "runner-error", "cli refused: spawn-failed"),
        _ => new_error(op_id, "runner-error", "worker refused the op"),
    })
}

/// `stdout: json`: parses stdout into `json`; on failure `json` is null and
/// `json-invalid` is true.
fn parse_stdout_json(out: &mut Map<String, Value>) {
    let parsed = out
        .get("stdout")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    out.insert("json-invalid".into(), json!(parsed.is_none()));
    out.insert("json".into(), parsed.unwrap_or(Value::Null));
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "cli_handles_tests.rs"]
mod handle_tests;
