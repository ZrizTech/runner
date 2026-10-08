//! One log line format (log format v3, `contract/log/`), the runner's own formatter.
//!
//! `format_line` is pure: a record in, one line out. `sub` adapts real
//! `tracing` events (and `log` records) to it and installs the subscriber:
//! stdout, filter from `ZRIZ_LOG`, default `DEFAULT_FILTER`. Nothing here
//! ever panics.

mod sub;

pub use sub::{
    DEFAULT_FILTER, Logging, emit_panic, init, install_panic_hook, subscriber_with_writer,
};

/// The env var holding the filter spec.
pub const ENV_FILTER: &str = "ZRIZ_LOG";

/// A duration as whole microseconds, saturating: the value `elapsed_ms`
/// and `avg_ms` take.
pub fn micros(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// A duration as whole milliseconds, saturating (frame `ts`, `exec-ms`).
pub fn millis(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

use tracing::Level;

/// Keys in print order (`lists.json` keys.order; a test compares).
pub const KEYS: [&str; 54] = [
    "org_id",
    "account_id",
    "run_id",
    "notice_id",
    "delivery_id",
    "step",
    "op_id",
    "pipeline_id",
    "token_id",
    "runner",
    "listener",
    "port",
    "method",
    "route",
    "path",
    "http_status",
    "env",
    "step_type",
    "kind",
    "channel",
    "resource",
    "cmd",
    "mode",
    "exit_code",
    "frame",
    "status",
    "reason",
    "auth_method",
    "provider",
    "browser",
    "ip",
    "op",
    "table",
    "rows",
    "sent",
    "skipped",
    "received",
    "count",
    "dropped",
    "refused",
    "attempts",
    "writes",
    "timeout_ms",
    "location",
    "client",
    "client_trace_id",
    "cloud",
    "build",
    "requests",
    "runs",
    "avg_ms",
    "window_s",
    "error",
    "elapsed_ms",
];

/// The runner's own components (= tracing targets).
pub const COMPONENTS: [&str; 5] = [
    "runner.main",
    "runner.exchange",
    "runner.ops",
    "runner.db",
    "runner.lib",
];

const MAX_CHARS: usize = 256;
const COMPONENT_WIDTH: usize = 15;
const TRACE_WIDTH: usize = 45;

/// A field value. `Micros` is integer microseconds, only valid for
/// `elapsed_ms` and `avg_ms`, which in turn accept nothing else.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i128),
    Bool(bool),
    Micros(u64),
}

/// One event, ready to format.
pub struct Record<'a> {
    pub time_ns: u128,
    pub level: Level,
    pub target: &'a str,
    /// `None` prints `trace_id=-`.
    pub trace: Option<&'a str>,
    pub event: &'a str,
    /// Missing values are simply absent from this list.
    pub fields: Vec<(String, Value)>,
}

/// Formats one line (no trailing newline). Returns the line and how many
/// fields were dropped (unknown key, repeated key, `ip` off `cloud.auth`,
/// `elapsed_ms`/`avg_ms` not in microseconds, `micros` on another key).
pub fn format_line(rec: &Record<'_>) -> (String, usize) {
    let mut dropped = 0usize;
    let mut slots: Vec<Option<String>> = vec![None; KEYS.len()];
    for (key, value) in &rec.fields {
        let Some(idx) = KEYS.iter().position(|k| k == key) else {
            dropped += 1;
            continue;
        };
        let timing = key == "elapsed_ms" || key == "avg_ms";
        let text = match value {
            _ if slots[idx].is_some() => None,
            _ if key == "ip" && rec.target != "cloud.auth" => None,
            Value::Micros(us) if timing => Some(format!("{}.{:03}", us / 1000, us % 1000)),
            Value::Micros(_) => None,
            _ if timing => None,
            Value::Int(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Str(s) => Some(quote(s)),
        };
        match text {
            Some(t) => slots[idx] = Some(t),
            None => dropped += 1,
        }
    }

    let mut line = String::with_capacity(160);
    push_time(&mut line, rec.time_ns);
    line.push(' ');
    let level = rec.level.as_str();
    line.push_str(level);
    pad(&mut line, 5usize.saturating_sub(level.len()));
    line.push(' ');
    let start = line.len();
    line.push_str("trace_id=");
    line.push_str(rec.trace.unwrap_or("-"));
    let used = line.len() - start;
    pad(&mut line, TRACE_WIDTH.saturating_sub(used));
    line.push(' ');
    line.push_str(rec.target);
    pad(
        &mut line,
        COMPONENT_WIDTH.saturating_sub(rec.target.chars().count()),
    );
    line.push(' ');
    line.push_str(rec.event);
    for (key, text) in KEYS.iter().zip(slots) {
        if let Some(t) = text {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(&t);
        }
    }
    (line, dropped)
}

fn pad(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n(' ', n));
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC; milliseconds are cut, not rounded.
fn push_time(out: &mut String, time_ns: u128) {
    let ms = time_ns / 1_000_000;
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX / 2);
    let milli = ms % 1000;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    out.push_str(&format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    ));
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Cut, then quote and escape when needed.
fn quote(s: &str) -> String {
    let cut: String = if s.chars().count() > MAX_CHARS {
        s.chars().take(MAX_CHARS - 1).chain(['…']).collect()
    } else {
        s.to_string()
    };
    let needs = cut.is_empty()
        || cut
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '=' || c == '\\' || c.is_control());
    if !needs {
        return cut;
    }
    let mut out = String::with_capacity(cut.len() + 2);
    out.push('"');
    for c in cut.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{{{:02x}}}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Turns a library event (a target we do not own) into a runner line.
/// Own components pass through unchanged. `sqlx::query` becomes
/// `runner.db` (the runner has no sqlx; the shared cases in `cases.json`
/// still pin this mapping for every product) with only `op`, `table`, `rows`, `elapsed_ms`; every other
/// crate becomes `runner.lib` `lib event` with `location=<crate>` and, at
/// WARN and above, its `message` as `error`. Their own fields are dropped
/// without counting. `message` arrives as a field named `message`.
pub fn map_library(rec: Record<'_>) -> Record<'_> {
    if COMPONENTS.contains(&rec.target) {
        return rec;
    }
    if rec.target == "sqlx::query" {
        let event = if rec.event == "query slow" || rec.level == Level::WARN {
            "query slow"
        } else {
            "query done"
        };
        let fields = rec
            .fields
            .into_iter()
            .filter(|(k, _)| matches!(k.as_str(), "op" | "table" | "rows" | "elapsed_ms"))
            .collect();
        return Record {
            target: "runner.db",
            event,
            fields,
            ..rec
        };
    }
    let location = rec.target.split("::").next().unwrap_or_default();
    let mut fields = vec![("location".to_string(), Value::Str(location.to_string()))];
    if rec.level <= Level::WARN {
        let message = rec.fields.iter().find_map(|(k, v)| match (k.as_str(), v) {
            ("message", Value::Str(s)) => Some(s.clone()),
            _ => None,
        });
        if let Some(m) = message {
            fields.push(("error".to_string(), Value::Str(m)));
        }
    }
    Record {
        target: "runner.lib",
        event: "lib event",
        fields,
        ..rec
    }
}
