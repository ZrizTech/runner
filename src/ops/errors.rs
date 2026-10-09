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
    let e = ErrorFrame::new(op_id, reason, closed_details(details));
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
        if let Value::Object(m) = closed_details(Value::Object(std::mem::take(&mut e.details))) {
            e.details = m;
        }
    }
    e
}

/// Makes the closed `details` keys fit their schema (`contract/error.json`):
/// a value that does not fit is replaced by a fixed word, so a hostile or
/// odd text from outside never travels back in a frame.
fn closed_details(details: Value) -> Value {
    let Value::Object(mut m) = details else {
        return details;
    };
    let fits = |s: &str, ok: fn(u8) -> bool, first: fn(u8) -> bool, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(ok) && s.bytes().next().is_some_and(first)
    };
    let name = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let handle = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let kind = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-';
    if let Some(Value::String(s)) = m.get("name")
        && !fits(s, name, |b| b.is_ascii_alphabetic() || b == b'_', 128)
    {
        m.insert("name".into(), json!("_"));
    }
    if let Some(Value::String(s)) = m.get("handle")
        && !fits(s, handle, handle, 32)
    {
        m.insert("handle".into(), json!("-"));
    }
    if let Some(Value::String(s)) = m.get("kind")
        && !fits(s, kind, kind, 64)
    {
        let t: String = s
            .to_ascii_lowercase()
            .bytes()
            .filter(|b| kind(*b))
            .take(64)
            .map(char::from)
            .collect();
        m.insert(
            "kind".into(),
            json!(if t.is_empty() { "unknown" } else { &t }),
        );
    }
    if let Some(Value::String(s)) = m.get("resource")
        && (s.is_empty() || s.chars().count() > 128)
    {
        let t: String = s.chars().take(128).collect();
        m.insert(
            "resource".into(),
            json!(if t.is_empty() { "-" } else { &t }),
        );
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_details_fit_the_schema() {
        let long = "x".repeat(300);
        let cases = [
            (json!({"name": "TOKEN_1"}), json!({"name": "TOKEN_1"})),
            (json!({"name": "to ken;secret"}), json!({"name": "_"})),
            (json!({"name": "1abc"}), json!({"name": "_"})),
            (json!({"handle": "h-1_a"}), json!({"handle": "h-1_a"})),
            (json!({"handle": "a b"}), json!({"handle": "-"})),
            (json!({"handle": ""}), json!({"handle": "-"})),
            (
                json!({"kind": "http.request"}),
                json!({"kind": "http.request"}),
            ),
            (json!({"kind": "Tele Port!"}), json!({"kind": "teleport"})),
            (json!({"kind": "!!!"}), json!({"kind": "unknown"})),
            (
                json!({"resource": long.clone()}),
                json!({"resource": "x".repeat(128)}),
            ),
            (json!({"resource": ""}), json!({"resource": "-"})),
            (json!({"limit": 3}), json!({"limit": 3})),
        ];
        for (input, want) in cases {
            assert_eq!(closed_details(input.clone()), want, "{input}");
        }
    }
}
