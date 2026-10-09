//! Wire types: same JSON field names (kebab-case) and omitempty behavior as
//! the wire schemas in `contract/`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// The envelope every message on the wire is wrapped in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    /// Wire schema version.
    pub v: i64,
    /// Frame type: "op", "result", or "error".
    pub t: String,
    /// This frame's id.
    pub id: String,
    /// The id of the frame this one replies to, or null.
    pub re: Option<String>,
    /// Milliseconds since the epoch.
    pub ts: i64,
    /// The frame's decoded body.
    pub d: serde_json::Value,
}

/// The decoded body of a frame with `t == "op"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Op {
    #[serde(rename = "op-id")]
    pub op_id: String,
    #[serde(rename = "run-id")]
    pub run_id: String,
    #[serde(rename = "step-index")]
    pub step_index: i64,
    pub kind: String,
    pub resource: String,
    #[serde(rename = "timeout-ms")]
    pub timeout_ms: i64,
    pub args: HashMap<String, serde_json::Value>,
    pub project: Vec<Vec<String>>,
    /// Optional trace id from the cloud; shown in log lines only.
    #[serde(rename = "trace-id", default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl Op {
    /// The cloud's trace id when it is a lowercase uuid; anything else is
    /// ignored (it would break the log columns and the worker schema).
    pub fn valid_trace_id(&self) -> Option<&str> {
        let t = self.trace_id.as_deref()?;
        is_trace_id(t).then_some(t)
    }
}

/// True for a trace id: a lower-case hex uuid with hyphens.
pub fn is_trace_id(t: &str) -> bool {
    t.len() == 36
        && t.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// Execution timing for a Result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    #[serde(rename = "exec-ms")]
    pub exec_ms: i64,
}

/// The decoded body of a frame with `t == "result"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Result {
    #[serde(rename = "op-id")]
    pub op_id: String,
    pub status: String,
    /// A `BTreeMap`, not a `HashMap`: the wire encoding of this field is
    /// sorted-key JSON. A `BTreeMap` gives that by construction.
    pub payload: BTreeMap<String, serde_json::Value>,
    pub scrubbed: i64,
    pub timing: Timing,
}

/// The decoded body of a frame with `t == "error"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    #[serde(rename = "op-id")]
    pub op_id: String,
    pub reason: String,
    /// Ids and numbers of the reason. Always there, can be empty. The keys
    /// are the closed set of `contract/error.json`.
    pub details: serde_json::Map<String, serde_json::Value>,
}

impl Error {
    /// The one place that makes an error frame. `details` must be a JSON
    /// object; anything else gives an empty one.
    pub fn new(op_id: &str, reason: &str, details: serde_json::Value) -> Self {
        Self {
            op_id: op_id.to_string(),
            reason: reason.to_string(),
            details: match details {
                serde_json::Value::Object(m) => m,
                _ => serde_json::Map::new(),
            },
        }
    }
}

/// Describes the runner to the cloud in an ExchangeRequest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Runner {
    #[serde(rename = "runner-id")]
    pub runner_id: String,
    pub version: String,
    pub ops: Vec<String>,
    pub resources: Vec<String>,
    #[serde(rename = "max-inflight")]
    pub max_inflight: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub env: String,
}

/// What the runner sends the cloud on each poll. An empty (or absent)
/// `frames` always encodes as `[]`, never `null` (`Vec` does this
/// naturally, so no custom encoder is needed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeRequest {
    pub v: i64,
    pub runner: Runner,
    pub inflight: i64,
    pub health: Health,
    pub frames: Vec<Frame>,
}

/// One browser resource in the health: contexts in use and the limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserLoad {
    pub resource: String,
    pub busy: u64,
    pub limit: u64,
}

/// The cli handles in use and the limit, for all cli resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliLoad {
    pub busy: u64,
    pub limit: u64,
}

/// The health of the runner, in each exchange request. Only numbers, ids
/// and fixed words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    #[serde(rename = "boot-id")]
    pub boot_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<Vec<BrowserLoad>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliLoad>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    pub refused: u64,
}

/// What the cloud sends back to the runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeResponse {
    pub v: i64,
    pub frames: Vec<Frame>,
    #[serde(rename = "ended-runs")]
    pub ended_runs: Vec<EndedRun>,
}

/// A run that ended, in the run-end notice of an exchange response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndedRun {
    #[serde(rename = "run-id")]
    pub run_id: String,
    #[serde(rename = "trace-id")]
    pub trace_id: String,
}
