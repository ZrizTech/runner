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

/// A reply frame that fits one request. A frame over `FRAME_MAX` would be
/// refused (413) again and again and dropped, and the run would never get
/// the result. It becomes a small `runner-error` frame instead (`where`:
/// `response-encoding`), for the same op.
pub(super) fn fit_frame(frame: contract::Frame, frame_id: &str, op_id: &str) -> contract::Frame {
    let size = serde_json::to_vec(&frame).map_or(0, |b| b.len());
    if size <= super::batch::FRAME_MAX {
        return frame;
    }
    tracing::error!(target: "runner.exchange", frame = "too-large", "frame failed");
    let e = ErrorFrame::new(
        op_id,
        "runner-error",
        serde_json::json!({"where": "response-encoding"}),
    );
    // A small frame always encodes.
    error_frame(frame_id, e).unwrap_or(frame)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_over_the_limit_becomes_an_error_frame() {
        let big = new_frame(
            "result",
            "f1",
            serde_json::json!({"op-id": "o1", "pad": "x".repeat(super::super::batch::FRAME_MAX)}),
        )
        .unwrap();
        let f = fit_frame(big, "f1", "o1");
        assert_eq!(f.t, "error");
        assert_eq!(f.re.as_deref(), Some("f1"));
        assert_eq!(f.d["op-id"], "o1");
        assert_eq!(f.d["reason"], "runner-error");
        assert_eq!(f.d["details"]["where"], "response-encoding");
        assert!(serde_json::to_vec(&f).unwrap().len() < 1024);
    }

    #[test]
    fn a_frame_under_the_limit_stays() {
        let small = new_frame("result", "f1", serde_json::json!({"op-id": "o1"})).unwrap();
        let id = small.id.clone();
        assert_eq!(fit_frame(small, "f1", "o1").id, id);
    }
}
