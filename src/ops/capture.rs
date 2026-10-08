//! The `capture` arg: `{ "NAME": ["body","api-key"] }`. Reads values out of
//! an op's projection source into the run vault, and masks those paths in
//! the returned payload. Never puts a value in an error or a log.

use serde_json::{Map, Value};
use std::collections::HashMap;

pub(crate) const CAPTURED: &str = "[captured]";

pub(crate) type Captures = Vec<(String, Vec<String>)>;

pub(crate) enum CaptureError {
    /// The arg is malformed.
    Invalid,
    /// The path for this capture name is missing or null in the source.
    Missing(String),
}

/// Parses `args["capture"]`; absent means no captures.
pub(crate) fn parse(args: &HashMap<String, Value>) -> Result<Captures, CaptureError> {
    let Some(v) = args.get("capture") else {
        return Ok(Vec::new());
    };
    let Value::Object(m) = v else {
        return Err(CaptureError::Invalid);
    };
    let mut out = Vec::new();
    for (name, path) in m {
        let Value::Array(segs) = path else {
            return Err(CaptureError::Invalid);
        };
        let segs: Option<Vec<String>> = segs.iter().map(|s| s.as_str().map(String::from)).collect();
        match segs {
            Some(s) if !s.is_empty() => out.push((name.clone(), s)),
            _ => return Err(CaptureError::Invalid),
        }
    }
    Ok(out)
}

/// Reads each captured path from `source`. Scalars become their string
/// form, objects and arrays compact JSON.
pub(crate) fn extract(
    source: &Map<String, Value>,
    caps: &Captures,
) -> Result<Vec<(String, String)>, CaptureError> {
    let mut out = Vec::new();
    for (name, path) in caps {
        let found = get_path(source, path);
        let text = match found {
            None | Some(Value::Null) => return Err(CaptureError::Missing(name.clone())),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        out.push((name.clone(), text));
    }
    Ok(out)
}

fn get_path<'a>(source: &'a Map<String, Value>, path: &[String]) -> Option<&'a Value> {
    let mut cur = source.get(path.first()?)?;
    for seg in &path[1..] {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Sets the value at each captured path in `payload` to `[captured]`,
/// creating the containers on the way.
pub(crate) fn mask(payload: &mut Map<String, Value>, caps: &Captures) {
    for (_, path) in caps {
        let Some((first, rest)) = path.split_first() else {
            continue;
        };
        let slot = payload.entry(first.clone()).or_insert(Value::Null);
        set_path(slot, rest);
    }
}

fn set_path(slot: &mut Value, path: &[String]) {
    let Some((seg, rest)) = path.split_first() else {
        *slot = Value::String(CAPTURED.to_string());
        return;
    };
    if let (Ok(idx), true) = (
        seg.parse::<usize>(),
        matches!(slot, Value::Array(_) | Value::Null),
    ) {
        if !slot.is_array() {
            *slot = Value::Array(Vec::new());
        }
        if let Value::Array(items) = slot {
            while items.len() <= idx {
                items.push(Value::Null);
            }
            set_path(&mut items[idx], rest);
        }
        return;
    }
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    if let Value::Object(m) = slot {
        set_path(m.entry(seg.clone()).or_insert(Value::Null), rest);
    }
}
