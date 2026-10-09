//! The makers of error frames. `new_error` is the one place that builds a
//! frame; the others fix the details of a reason.

use super::*;

pub(super) fn capture_error(op_id: &str, missing: Option<&str>) -> ErrorFrame {
    match missing {
        Some(n) => new_error(op_id, "capture-not-found", json!({"name": n})),
        None => runner_error(op_id, "capture-spec"),
    }
}

pub(super) fn new_error(op_id: &str, reason: &str, details: Value) -> ErrorFrame {
    let e = ErrorFrame::new(op_id, reason, details);
    #[cfg(test)]
    assert_valid(&e);
    e
}

/// In tests, every frame this module makes must validate against
/// `contract/error.json`.
#[cfg(test)]
fn assert_valid(e: &ErrorFrame) {
    use std::sync::OnceLock;
    static V: OnceLock<Option<crate::contract::Validator>> = OnceLock::new();
    let v = V
        .get_or_init(|| crate::contract::Validator::new().ok())
        .as_ref()
        .expect("validator");
    let doc = serde_json::to_value(e).expect("encode");
    assert!(v.validate("error", &doc).is_ok(), "invalid frame: {doc}");
}

/// The "not known" frame: `runner-error` with one fixed `where` word.
pub(super) fn runner_error(op_id: &str, place: &str) -> ErrorFrame {
    new_error(op_id, "runner-error", json!({"where": place}))
}

/// Maps an error from [`placeholder::substitute`] to the contract error an
/// op should return. A disallowed slot keeps its own reason; an unknown
/// placeholder names the missing environment variable so the user knows
/// what to set on the runner, without ever including its value; anything
/// else falls back to a generic message.
pub(super) fn placeholder_substitute_error(op_id: &str, err: &PlaceholderError) -> ErrorFrame {
    match err {
        PlaceholderError::DisallowedSlot(_, name) if !name.is_empty() => new_error(
            op_id,
            "placeholder-in-disallowed-slot",
            json!({"name": name}),
        ),
        PlaceholderError::DisallowedSlot(..) => runner_error(op_id, "op-args"),
        PlaceholderError::UnknownPlaceholder(_) => runner_error(op_id, "placeholder"),
    }
}

/// Reasons whose frame names the resource of the op.
const NAMES_RESOURCE: [&str; 4] = [
    "host-not-allowed",
    "read-only",
    "command-not-allowed",
    "arg-not-allowed",
];

/// Adds the resource of the op to a refusal that has it in its row of
/// `cause/reasons.json` and was made below the resource lookup.
pub(super) fn with_resource(mut e: ErrorFrame, resource: &str) -> ErrorFrame {
    if NAMES_RESOURCE.contains(&e.reason.as_str()) {
        e.details.insert("resource".into(), json!(resource));
    }
    e
}
