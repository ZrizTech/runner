//! Log format v3, runner side: the shared cases (test A), real events
//! through the real subscriber (test B), the lists, the call sites.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use serde_json::Value as J;
use support::{Buf, TID, op};
use tracing::Instrument;
use zriz_runner::logfmt::{COMPONENTS, KEYS, Record, Value, format_line, map_library};

fn read(name: &str) -> J {
    let path = format!("{}/contract/log/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn value(key: &str, v: &J) -> Option<Value> {
    Some(match v {
        J::Null => return None,
        J::Bool(b) => Value::Bool(*b),
        J::String(s) => Value::Str(s.clone()),
        J::Number(n) => Value::Int(i128::from(n.as_i64().unwrap())),
        J::Object(o) => Value::Micros(o["micros"].as_u64().unwrap_or_else(|| panic!("{key}"))),
        J::Array(_) => panic!("array field"),
    })
}

/// Test A: every case, byte for byte. Cases with `product: cloud` are the
/// cloud's library map and are skipped; `product: runner` ones go through
/// the runner's own `map_library`.
#[test]
fn every_case_matches_byte_for_byte() {
    let cases = read("cases.json");
    let (mut ran, mut skipped) = (0, 0);
    for c in cases["cases"].as_array().unwrap() {
        let i = &c["input"];
        let product = i["product"].as_str();
        if product == Some("cloud") {
            skipped += 1;
            continue;
        }
        let fields: Vec<(String, Value)> = i["fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| Some((f[0].as_str()?.to_string(), value(f[0].as_str()?, &f[1])?)))
            .collect();
        let mut rec = Record {
            time_ns: i["time_ns"].as_str().unwrap().parse().unwrap(),
            level: i["level"].as_str().unwrap().parse().unwrap(),
            target: i["target"].as_str().unwrap(),
            trace: i["trace"].as_str(),
            event: i["event"].as_str().unwrap(),
            fields,
        };
        if product == Some("runner") {
            rec = map_library(rec);
        }
        let (line, dropped) = format_line(&rec);
        let name = c["name"].as_str().unwrap();
        assert_eq!(line, c["expect_line"].as_str().unwrap(), "case {name}");
        assert_eq!(
            dropped as u64,
            c["expect_dropped"].as_u64().unwrap(),
            "{name}"
        );
        ran += 1;
    }
    assert!(ran > 100 && skipped == 5, "ran {ran}, skipped {skipped}");
}

#[test]
fn key_and_component_lists_match_lists_json() {
    let l = read("lists.json");
    let strs = |v: &J| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(strs(&l["keys"]["order"]), KEYS);
    assert_eq!(strs(&l["components"]["runner"]), COMPONENTS);
}

/// Test B: real op lines from the real `Handler`, the trace taken from the
/// span, library events mapped, nothing dropped, no Debug leftovers.
#[tokio::test(flavor = "current_thread")]
async fn real_events_through_the_real_subscriber() {
    let _pin = support::pin();
    let buf = Buf::default();
    let (sub, logging) = zriz_runner::logfmt::subscriber_with_writer("debug", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);

    let handler = zriz_runner::ops::Handler::new(Default::default(), Default::default())
        .await
        .unwrap();
    let span = tracing::error_span!("op", trace_id = %TID);
    handler
        .handle(op("http.request", "nope"))
        .instrument(span)
        .await;
    handler.handle(op("teleport", "nope")).await; // no span: trace `-`
    tracing::warn!(target: "hyper::client", "dns error for https://user:pw@x.io/a?k=1 failed");
    tracing::info!(target: "tokio_util::codec", "frame decoded");
    tracing::debug!(target: "sqlx::query", "select 1");

    let out = buf.text();
    let lines: Vec<&str> = out.lines().collect();
    println!("{out}");
    assert_eq!(lines.len(), 5, "{out}");
    let re = format!(
        " WARN  trace_id={TID} runner.ops      op refused run_id=r1 step=3 op_id=o1 kind=http.request resource=nope status=error reason=unknown-resource elapsed_ms="
    );
    assert!(lines[0].contains(&re), "{}", lines[0]);
    assert!(lines[1].contains(" WARN  trace_id=-                                    runner.ops      op refused run_id=r1 step=3 op_id=o1 kind=teleport resource=nope status=error reason=unknown-kind elapsed_ms="), "{}", lines[1]);
    assert!(
        lines[2].ends_with(
            " runner.lib      lib event location=hyper error=\"dns error for [url] failed\""
        ),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].ends_with(" runner.lib      lib event location=tokio_util"),
        "{}",
        lines[3]
    );
    assert!(
        lines[4].ends_with(" runner.db       query done"),
        "{}",
        lines[4]
    );
    assert!(
        !out.contains("Some(") && !out.contains("None") && !out.contains("\"\\\""),
        "{out}"
    );
    assert!(!out.contains("pw@"), "{out}");
    assert_eq!(logging.dropped(), 0, "own call sites drop nothing: {out}");
}

#[test]
fn every_tracing_macro_in_src_is_on_the_lists() {
    let l = read("lists.json");
    let keys: Vec<&str> = KEYS.to_vec();
    let mut seen = 0;
    for file in rs_files(&format!("{}/src", env!("CARGO_MANIFEST_DIR"))) {
        let text = std::fs::read_to_string(&file).unwrap();
        for level in ["error", "warn", "info", "debug", "trace"] {
            for pat in [format!("tracing::{level}!("), format!(" {level}!(")] {
                let mut rest = text.as_str();
                while let Some(at) = rest.find(&pat) {
                    let body = balanced(&rest[at + pat.len()..]);
                    rest = &rest[at + pat.len() + body.len()..];
                    check(&file, level, &body, &l, &keys);
                    seen += 1;
                }
            }
        }
    }
    assert!(seen >= 20, "found {seen} macros; the scan is broken");
}

fn rs_files(dir: &str) -> Vec<String> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let s = p.to_str().unwrap().to_string();
        if p.is_dir() {
            out.extend(rs_files(&s));
        } else if s.ends_with(".rs") {
            out.push(s);
        }
    }
    out
}

/// The text up to the matching `)` (strings and chars respected).
fn balanced(s: &str) -> String {
    let (mut depth, mut in_str, mut esc) = (1, false, false);
    let mut out = String::new();
    for c in s.chars() {
        if in_str {
            if esc {
                esc = false
            } else if c == '\\' {
                esc = true
            } else if c == '"' {
                in_str = false
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return out;
            }
        }
        out.push(c);
    }
    panic!("unbalanced macro");
}

fn split_top(body: &str) -> Vec<String> {
    let (mut parts, mut cur, mut depth, mut in_str, mut esc) =
        (vec![], String::new(), 0, false, false);
    for c in body.chars() {
        if in_str {
            if esc {
                esc = false
            } else if c == '\\' {
                esc = true
            } else if c == '"' {
                in_str = false
            }
        } else {
            match c {
                '"' => in_str = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(std::mem::take(&mut cur).trim().to_string());
                    continue;
                }
                _ => {}
            }
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

fn check(file: &str, level: &str, body: &str, l: &J, keys: &[&str]) {
    let parts = split_top(body);
    let here = format!("{file}: {level}!({body})");
    let target = parts[0]
        .strip_prefix("target:")
        .unwrap_or_else(|| panic!("no target: {here}"));
    let target = target.trim().trim_matches('"');
    let event = parts.last().unwrap().trim_matches('"');
    let spec = &l["events"][target][event];
    assert!(
        spec.is_object(),
        "event `{event}` not listed for {target}: {here}"
    );
    let levels: Vec<String> = spec["levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_lowercase())
        .collect();
    assert!(
        levels.iter().any(|x| x == level),
        "level {level} not allowed for {event}: {here}"
    );
    for p in &parts[1..parts.len() - 1] {
        assert!(
            !p.contains("= ?") && !p.starts_with('?'),
            "Debug (?) value: {here}"
        );
        let key = p.split('=').next().unwrap().trim();
        if key == "trace_id" {
            continue; // the trace column, not a field
        }
        let literal = p.split('=').nth(1).unwrap().trim();
        if key == "reason" && literal.starts_with('"') {
            let v = literal.trim_matches('"');
            if let Some(reasons) = spec["reasons"].as_array() {
                assert!(
                    reasons.iter().any(|r| r == v),
                    "reason `{v}` not in enum: {here}"
                );
            }
        }
        assert!(keys.contains(&key), "key `{key}` not on the list: {here}");
        let allowed = spec["keys"].as_array().unwrap();
        assert!(
            allowed.iter().any(|k| k == key),
            "key `{key}` not allowed on {event}: {here}"
        );
    }
}
