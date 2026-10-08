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
        let ok = t.len() == 36
            && t.bytes().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => b == b'-',
                _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
            });
        ok.then_some(t)
    }
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
    pub message: String,
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
    pub frames: Vec<Frame>,
}

/// What the cloud sends back to the runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeResponse {
    pub v: i64,
    pub frames: Vec<Frame>,
}
