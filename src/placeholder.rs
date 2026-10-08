//! Substitutes `${NAME}` secrets into op args, restricted to the argument
//! keys where each op kind allows them.

use serde_json::{Map, Value};

/// Errors substituting placeholders into op args.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlaceholderError {
    /// A placeholder appeared in an arg key that does not allow secrets for
    /// the op's kind. Carries the key name, never a value.
    #[error("placeholder in disallowed slot: key {0:?}")]
    DisallowedSlot(String),
    /// A placeholder named an environment variable `lookup` did not know.
    /// Carries the placeholder's key, e.g. "TOKEN", never a value.
    #[error("unknown placeholder: {0}")]
    UnknownPlaceholder(String),
}

impl PlaceholderError {
    /// The unknown placeholder's name, if this is that variant.
    pub fn unknown_name(&self) -> Option<&str> {
        match self {
            PlaceholderError::UnknownPlaceholder(name) => Some(name),
            PlaceholderError::DisallowedSlot(_) => None,
        }
    }
}

/// The result of replacing placeholders in a set of op args.
#[derive(Debug, Clone, PartialEq)]
pub struct Substituted {
    pub args: Map<String, Value>,
    pub secrets: Vec<String>,
}

/// Returns the arg keys in which `${NAME}` placeholders may appear for
/// `kind`.
fn allowed_slots(kind: &str) -> &'static [&'static str] {
    match kind {
        "http.request" => &["headers", "body", "query-params"],
        "sql.query" => &[],
        "cli.exec" => &["env"],
        "evidence.fetch" => &[],
        _ => &[],
    }
}

/// Replaces every `${NAME}` placeholder found inside the allowed arg keys
/// for `kind` with the value `lookup(NAME)`, collecting each substituted
/// value as a secret. A placeholder in any other key is refused with
/// `DisallowedSlot`; an unresolved NAME is refused with
/// `UnknownPlaceholder`.
pub fn substitute(
    kind: &str,
    args: &Map<String, Value>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Substituted, PlaceholderError> {
    let allowed = allowed_slots(kind);
    let mut secrets = Vec::new();

    let mut out = Map::with_capacity(args.len());
    for (key, val) in args {
        if !allowed.contains(&key.as_str()) {
            if contains_placeholder(val) {
                return Err(PlaceholderError::DisallowedSlot(key.clone()));
            }
            out.insert(key.clone(), val.clone());
            continue;
        }
        let replaced = substitute_value(val, lookup, &mut secrets)?;
        out.insert(key.clone(), replaced);
    }
    Ok(Substituted { args: out, secrets })
}

/// Substitutes `${NAME}` into a browser command list. A placeholder may
/// sit only in the `value` of a `fill` or `press-seq` command; anywhere
/// else it is refused with `DisallowedSlot`.
pub fn substitute_commands(
    commands: &Value,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Substituted, PlaceholderError> {
    let mut secrets = Vec::new();
    let Value::Array(items) = commands else {
        return Err(PlaceholderError::DisallowedSlot("commands".to_string()));
    };
    let mut out_items = Vec::with_capacity(items.len());
    for item in items {
        let Value::Object(cmd) = item else {
            return Err(PlaceholderError::DisallowedSlot("commands".to_string()));
        };
        let fills = matches!(
            cmd.get("do").and_then(Value::as_str),
            Some("fill" | "press-seq")
        );
        let mut out = Map::with_capacity(cmd.len());
        for (k, v) in cmd {
            match v {
                Value::String(s) if fills && k == "value" => {
                    let r = substitute_string(s, lookup, &mut secrets)?;
                    out.insert(k.clone(), Value::String(r));
                }
                _ if contains_placeholder(v) => {
                    return Err(PlaceholderError::DisallowedSlot(k.clone()));
                }
                _ => {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        out_items.push(Value::Object(out));
    }
    let mut args = Map::new();
    args.insert("commands".to_string(), Value::Array(out_items));
    Ok(Substituted { args, secrets })
}

fn substitute_value(
    val: &Value,
    lookup: &dyn Fn(&str) -> Option<String>,
    secrets: &mut Vec<String>,
) -> std::result::Result<Value, PlaceholderError> {
    match val {
        Value::String(s) => Ok(Value::String(substitute_string(s, lookup, secrets)?)),
        Value::Object(m) => {
            let mut out = Map::with_capacity(m.len());
            for (k, v) in m {
                out.insert(k.clone(), substitute_value(v, lookup, secrets)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(substitute_value(item, lookup, secrets)?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

fn substitute_string(
    s: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    secrets: &mut Vec<String>,
) -> std::result::Result<String, PlaceholderError> {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(name_len) = placeholder_at(s, i) {
            let name = &s[i + 2..i + 2 + name_len];
            match lookup(name) {
                Some(val) => {
                    out.push_str(&val);
                    secrets.push(val);
                }
                None => return Err(PlaceholderError::UnknownPlaceholder(name.to_string())),
            }
            i += 2 + name_len + 1;
        } else {
            let ch = s[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// If `s[i..]` starts a `${NAME}` placeholder (name matching
/// `[A-Za-z_][A-Za-z0-9_]*`), returns the byte length of NAME.
fn placeholder_at(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(i) != Some(&b'$') || bytes.get(i + 1) != Some(&b'{') {
        return None;
    }
    let name_start = i + 2;
    let mut j = name_start;
    let first = *bytes.get(j)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    j += 1;
    while let Some(&b) = bytes.get(j) {
        if b.is_ascii_alphanumeric() || b == b'_' {
            j += 1;
        } else {
            break;
        }
    }
    if bytes.get(j) == Some(&b'}') {
        Some(j - name_start)
    } else {
        None
    }
}

fn contains_placeholder(val: &Value) -> bool {
    match val {
        Value::String(s) => placeholder_scan_any(s),
        Value::Object(m) => m.values().any(contains_placeholder),
        Value::Array(items) => items.iter().any(contains_placeholder),
        _ => false,
    }
}

fn placeholder_scan_any(s: &str) -> bool {
    let mut i = 0;
    while i < s.len() {
        if placeholder_at(s, i).is_some() {
            return true;
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        i += ch.len_utf8().max(1);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(env: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| env.get(name).map(|v| v.to_string())
    }

    fn obj(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn disallowed_slot() {
        let args = obj(vec![
            ("path", Value::String("${X}".into())),
            ("method", Value::String("GET".into())),
        ]);
        let mut env = HashMap::new();
        env.insert("X", "v");
        let err = substitute("http.request", &args, &lookup(env)).unwrap_err();
        assert!(matches!(err, PlaceholderError::DisallowedSlot(ref k) if k == "path"));
        assert!(err.to_string().contains("path"));
    }

    #[test]
    fn nested_placeholders_replaced_and_collected_as_secrets() {
        let body = obj(vec![(
            "auth",
            Value::Object(obj(vec![("token", Value::String("${TOKEN}".into()))])),
        )]);
        let headers = obj(vec![(
            "Authorization",
            Value::String("Bearer ${TOKEN}".into()),
        )]);
        let args = obj(vec![
            ("body", Value::Object(body)),
            ("headers", Value::Object(headers)),
            ("method", Value::String("GET".into())),
            ("path", Value::String("/x".into())),
        ]);
        let mut env = HashMap::new();
        env.insert("TOKEN", "s3cr3t");
        let got = substitute("http.request", &args, &lookup(env)).expect("substitute");

        let body = got.args["body"].as_object().unwrap();
        let auth = body["auth"].as_object().unwrap();
        assert_eq!(auth["token"], Value::String("s3cr3t".into()));
        let headers = got.args["headers"].as_object().unwrap();
        assert_eq!(
            headers["Authorization"],
            Value::String("Bearer s3cr3t".into())
        );
        assert!(!got.secrets.is_empty());
        for s in &got.secrets {
            assert_eq!(s, "s3cr3t");
        }
    }

    #[test]
    fn unknown_placeholder_in_body() {
        let args = obj(vec![(
            "body",
            Value::Object(obj(vec![("x", Value::String("${MISSING}".into()))])),
        )]);
        let err = substitute("http.request", &args, &lookup(HashMap::new())).unwrap_err();
        assert!(err.to_string().contains("MISSING"));
        assert_eq!(err.unknown_name(), Some("MISSING"));
    }

    #[test]
    fn sql_params_refused() {
        let args = obj(vec![
            ("query", Value::String("SELECT ?".into())),
            (
                "params",
                Value::Array(vec![Value::String("${ZRIZ_CANARY}".into())]),
            ),
        ]);
        let mut env = HashMap::new();
        env.insert("ZRIZ_CANARY", "canary-val");
        let err = substitute("sql.query", &args, &lookup(env)).unwrap_err();
        assert!(matches!(err, PlaceholderError::DisallowedSlot(ref k) if k == "params"));
    }

    #[test]
    fn sql_query_slot_disallowed() {
        let args = obj(vec![
            ("query", Value::String("SELECT ${X}".into())),
            ("params", Value::Array(vec![])),
        ]);
        let mut env = HashMap::new();
        env.insert("X", "v");
        let err = substitute("sql.query", &args, &lookup(env)).unwrap_err();
        assert!(matches!(err, PlaceholderError::DisallowedSlot(_)));
    }

    #[test]
    fn evidence_fetch_op_id_disallowed() {
        let args = obj(vec![("op-id", Value::String("${X}".into()))]);
        let mut env = HashMap::new();
        env.insert("X", "v");
        let err = substitute("evidence.fetch", &args, &lookup(env)).unwrap_err();
        assert!(matches!(err, PlaceholderError::DisallowedSlot(_)));
    }

    #[test]
    fn no_placeholders_leaves_args_unchanged_with_no_secrets() {
        let args = obj(vec![
            ("method", Value::String("GET".into())),
            ("path", Value::String("/x".into())),
            (
                "headers",
                Value::Object(obj(vec![("A", Value::String("b".into()))])),
            ),
        ]);
        let got = substitute("http.request", &args, &lookup(HashMap::new())).expect("substitute");
        assert_eq!(got.args, args);
        assert!(got.secrets.is_empty());
    }
}
