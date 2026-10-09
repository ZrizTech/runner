//! Implements the evidence.fetch op kind. The op's resource field is
//! ignored: evidence is keyed by run, not by resource.

use super::{Handler, new_error, runner_error};
use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame, Timing};
use serde_json::{Value, json};
use std::collections::BTreeMap;

impl Handler {
    pub(crate) fn evidence_fetch(
        &self,
        op: &contract::Op,
    ) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        if self.cfg.evidence == "none" {
            return (
                None,
                Some(new_error(&op.op_id, "evidence-disabled", json!({}))),
            );
        }

        let now = (self.now)();
        let entry = match self.evidence.get(&op.run_id, now) {
            Some(e) => e,
            None => {
                return (
                    None,
                    Some(new_error(&op.op_id, "evidence-expired", json!({}))),
                );
            }
        };
        let want_op_id = op.args.get("op-id").and_then(|v| v.as_str()).unwrap_or("");
        if entry.op_id != want_op_id {
            return (
                None,
                Some(new_error(&op.op_id, "evidence-expired", json!({}))),
            );
        }

        let excerpt = match crate::evidence::excerpt(&entry, &self.deny_keys) {
            Ok(e) => e,
            Err(_) => {
                return (None, Some(runner_error(&op.op_id, "evidence-excerpt")));
            }
        };

        let mut payload = BTreeMap::new();
        payload.insert("excerpt".to_string(), Value::String(excerpt));
        payload.insert("status".to_string(), Value::from(entry.status));
        (
            Some(ResultFrame {
                op_id: op.op_id.clone(),
                status: "pass".to_string(),
                payload,
                scrubbed: 0,
                timing: Timing { exec_ms: 0 },
            }),
            None,
        )
    }
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
