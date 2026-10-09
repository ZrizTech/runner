//! Building the result/error frames a finished op replies with.

use super::{ErrorFrame, ResultFrame};
use crate::contract;
use std::time::{SystemTime, UNIX_EPOCH};

/// Errors building a reply frame: either the handler broke its contract
/// (returned neither a result nor an error), or the reply body failed to
/// encode. Either way the op's reply is dropped and only logged, never sent
/// as a synthesized frame.
#[derive(Debug, thiserror::Error)]
pub(super) enum ReplyError {
    #[error("exchange: handler returned neither result nor error")]
    NeitherResultNorError,
    #[error("exchange: marshal frame body: {0}")]
    Marshal(#[source] serde_json::Error),
}

pub(super) fn reply_frame(
    frame_id: &str,
    result: Option<ResultFrame>,
    op_err: Option<ErrorFrame>,
) -> std::result::Result<contract::Frame, ReplyError> {
    if let Some(e) = op_err {
        return error_frame(frame_id, e).map_err(ReplyError::Marshal);
    }
    if let Some(r) = result {
        return result_frame(frame_id, r).map_err(ReplyError::Marshal);
    }
    Err(ReplyError::NeitherResultNorError)
}

fn result_frame(
    frame_id: &str,
    result: ResultFrame,
) -> std::result::Result<contract::Frame, serde_json::Error> {
    new_frame("result", frame_id, result)
}

pub(super) fn error_frame(
    frame_id: &str,
    op_err: ErrorFrame,
) -> std::result::Result<contract::Frame, serde_json::Error> {
    new_frame("error", frame_id, op_err)
}

fn new_frame(
    t: &str,
    re: &str,
    d: impl serde::Serialize,
) -> std::result::Result<contract::Frame, serde_json::Error> {
    let value = serde_json::to_value(d)?;
    Ok(contract::Frame {
        v: 1,
        t: t.to_string(),
        id: uuid::Uuid::new_v4().to_string(),
        re: Some(re.to_string()),
        ts: now_millis(),
        d: value,
    })
}

fn now_millis() -> i64 {
    crate::logfmt::millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    )
}
