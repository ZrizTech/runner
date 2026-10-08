//! The tracing adapter: a `FormatEvent` writing the v3 line, the `ZRIZ_LOG`
//! switch and the panic hook.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::{Event, Id, Level, Metadata, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use super::{Record, Value, format_line, map_library};

/// Handle to a live logger: counts fields the formatter dropped.
#[derive(Clone, Default)]
pub struct Logging {
    dropped: Arc<AtomicUsize>,
    /// Why the `ZRIZ_LOG` spec was refused (then [`DEFAULT_FILTER`] is used).
    invalid: Option<String>,
}

impl Logging {
    /// Fields dropped so far (unknown key, repeat, wrong type). Should be 0.
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Why the filter spec was refused, if it was.
    pub fn invalid(&self) -> Option<&str> {
        self.invalid.as_deref()
    }

    /// WARN `config warning reason=invalid` when the spec was refused.
    /// Call it with the subscriber active.
    pub fn report_config(&self) {
        if let Some(error) = &self.invalid {
            tracing::warn!(target: "runner.main", reason = "invalid", error = %error, "config warning");
        }
    }
}

struct Line {
    dropped: Arc<AtomicUsize>,
}

/// Collects one event: message = event name, `trace_id` = trace column.
/// Fields of the `log` bridge (`log.*`) are not ours and are skipped.
#[derive(Default)]
struct Collect {
    message: String,
    trace: Option<String>,
    log_target: Option<String>,
    fields: Vec<(String, Value)>,
}

impl Collect {
    fn put(&mut self, name: &str, value: Value) {
        match (name, value) {
            ("message", Value::Str(s)) => self.message = s,
            ("trace_id", Value::Str(s)) => self.trace = Some(s),
            ("log.target", Value::Str(s)) => self.log_target = Some(s),
            (n, _) if n.starts_with("log.") => {}
            (n, v) => self.fields.push((n.to_string(), v)),
        }
    }

    fn number(&mut self, name: &str, n: i128) {
        // In Rust an integer given as `elapsed_ms`/`avg_ms` is microseconds.
        let v = match (name, u64::try_from(n)) {
            ("elapsed_ms" | "avg_ms", Ok(us)) => Value::Micros(us),
            _ => Value::Int(n),
        };
        self.put(name, v);
    }
}

impl Visit for Collect {
    fn record_str(&mut self, f: &Field, v: &str) {
        self.put(f.name(), Value::Str(v.to_string()));
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.number(f.name(), i128::from(v));
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.number(f.name(), i128::from(v));
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        self.put(f.name(), Value::Bool(v));
    }
    fn record_f64(&mut self, f: &Field, _v: f64) {
        // No floats exist: an unknown key, so the formatter drops and counts it.
        self.fields
            .push((format!("{}!float", f.name()), Value::Int(0)));
    }
    fn record_debug(&mut self, f: &Field, v: &dyn fmt::Debug) {
        // `%value` and format_args! arrive here; their Debug is the Display.
        self.put(f.name(), Value::Str(format!("{v:?}")));
    }
}

/// The `trace_id` a span carries, kept in its extensions (a Java MDC).
struct SpanTrace(String);

/// Stores `trace_id` from span attributes; no other span field is kept.
struct TraceLayer;

struct FindTrace(Option<String>);
impl Visit for FindTrace {
    fn record_str(&mut self, f: &Field, v: &str) {
        if f.name() == "trace_id" {
            self.0 = Some(v.to_string());
        }
    }
    fn record_debug(&mut self, f: &Field, v: &dyn fmt::Debug) {
        if f.name() == "trace_id" {
            self.0 = Some(format!("{v:?}"));
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for TraceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut find = FindTrace(None);
        attrs.record(&mut find);
        if let (Some(t), Some(span)) = (find.0, ctx.span(id)) {
            span.extensions_mut().insert(SpanTrace(t));
        }
    }
}

/// `scheme://...` up to a space, quote or `)`: lib messages may carry URLs
/// with credentials, and the runner has no run-scoped secret list here.
fn redact_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("://") {
        let head = &rest[..at];
        let start = head
            .char_indices()
            .rev()
            .find(|&(_, c)| !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.')
            .map_or(0, |(i, c)| i + c.len_utf8());
        out.push_str(&rest[..start]);
        out.push_str("[url]");
        let tail = &rest[at + 3..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == ')' || c == '\'')
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let mut c = Collect::default();
        event.record(&mut c);
        let target = match (meta.target(), c.log_target.as_deref()) {
            ("log", Some(t)) => t,
            (t, _) => t,
        };
        let own = super::COMPONENTS.contains(&target);
        let mut fields = std::mem::take(&mut c.fields);
        if !own {
            let message = redact_urls(&c.message);
            fields.push(("message".to_string(), Value::Str(message)));
        }
        let trace = c.trace.clone().or_else(|| {
            ctx.event_scope()?
                .find_map(|span| span.extensions().get::<SpanTrace>().map(|t| t.0.clone()))
        });
        let time_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let rec = map_library(Record {
            time_ns,
            level: *meta.level(),
            target,
            trace: trace.as_deref(),
            event: if own { &c.message } else { "" },
            fields,
        });
        let (line, dropped) = format_line(&rec);
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        writeln!(writer, "{line}")
    }
}

/// A span that carries a trace (the op span). Kept whatever the filter says,
/// so a line inside it always has its `trace_id`. Such spans are error level.
fn is_trace_span(m: &Metadata<'_>) -> bool {
    m.is_span() && m.fields().field("trace_id").is_some()
}

/// Builds the subscriber for a filter spec and a writer (tests use a buffer).
/// An invalid spec falls back to [`DEFAULT_FILTER`]; [`Logging::invalid`]
/// says why. The filter sits on the output layer only: trace spans pass it
/// by [`is_trace_span`], and `TraceLayer` sees nothing else.
pub fn subscriber_with_writer<W>(
    spec: &str,
    writer: W,
) -> (impl Subscriber + Send + Sync + 'static, Logging)
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let (filter, invalid) = match EnvFilter::try_new(spec) {
        Ok(f) => (f, None),
        Err(e) => (EnvFilter::new(DEFAULT_FILTER), Some(e.to_string())),
    };
    let logging = Logging {
        invalid,
        ..Logging::default()
    };
    let spans = || filter_fn(is_trace_span).with_max_level_hint(Level::ERROR);
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .event_format(Line {
            dropped: logging.dropped.clone(),
        })
        .with_filter(filter.or(spans()));
    let sub = tracing_subscriber::registry()
        .with(TraceLayer.with_filter(spans()))
        .with(layer);
    (sub, logging)
}

/// Filter used when `ZRIZ_LOG` is unset or invalid (same as
/// `filter.default_filter.runner` in the shared lists.json; a test keeps
/// them equal).
pub const DEFAULT_FILTER: &str = "warn,runner=info";

/// Installs the global subscriber (stdout, `ZRIZ_LOG`, default `DEFAULT_FILTER`),
/// reports an invalid spec, installs the panic hook, then bridges `log`-crate
/// records in. The hook goes in whenever our subscriber did, even when some
/// other `log` logger was set first. Call once, first thing in `main`.
pub fn init(lookup: &dyn Fn(&str) -> Option<String>) -> Logging {
    let spec = lookup(super::ENV_FILTER)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_FILTER.to_string());
    let (sub, logging) = subscriber_with_writer(&spec, std::io::stdout);
    let was_set = tracing::dispatcher::has_been_set();
    // Err is either "a subscriber was already set" or "a log logger was".
    let _ = sub.try_init();
    if !was_set && tracing::dispatcher::has_been_set() {
        logging.report_config();
        install_panic_hook();
    }
    logging
}

/// One ERROR `panic` line with `location` and `error`; the trace comes from
/// the current span. The process goes on unwinding as usual. `error` is the
/// message only when it is a string literal from the code; a formatted
/// message may embed data (std's slicing panics quote the string), so it is
/// never logged.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
        let msg = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .unwrap_or("formatted panic message withheld");
        emit_panic(&location, msg);
    }));
}

/// The panic line itself (split out so a test can call it).
pub fn emit_panic(location: &str, error: &str) {
    tracing::error!(target: "runner.main", location = %location, error = %error, "panic");
}
