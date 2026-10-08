//! `filter_cases` from the shared cases, through the real `EnvFilter`.
//!
//! Every case is expressible: the grammar (`<level>[,<target>=<level>...]`)
//! is EnvFilter's own. Dotted targets (`cloud.db`) match as string prefixes,
//! which is exact for the closed component list.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{EnvFilter, Layer};

struct Count(Arc<AtomicUsize>);
impl<S: Subscriber> Layer<S> for Count {
    fn on_event(&self, _e: &Event<'_>, _c: Context<'_, S>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

macro_rules! at_level {
    ($t:literal, $lvl:expr) => {
        match $lvl {
            "error" => tracing::event!(target: $t, Level::ERROR, "x"),
            "warn" => tracing::event!(target: $t, Level::WARN, "x"),
            "info" => tracing::event!(target: $t, Level::INFO, "x"),
            "debug" => tracing::event!(target: $t, Level::DEBUG, "x"),
            "trace" => tracing::event!(target: $t, Level::TRACE, "x"),
            other => panic!("level {other}"),
        }
    };
}

fn emit(target: &str, level: &str) {
    match target {
        "cloud.runs" => at_level!("cloud.runs", level),
        "cloud.http" => at_level!("cloud.http", level),
        "cloud.db" => at_level!("cloud.db", level),
        "cloud.auth" => at_level!("cloud.auth", level),
        "zz.http" => at_level!("zz.http", level),
        "zz.run" => at_level!("zz.run", level),
        "runner.exchange" => at_level!("runner.exchange", level),
        "runner.ops" => at_level!("runner.ops", level),
        "worker.browser" => at_level!("worker.browser", level),
        "worker.main" => at_level!("worker.main", level),
        "worker.cli" => at_level!("worker.cli", level),
        "sqlx::query" => at_level!("sqlx::query", level),
        "hyper::client" => at_level!("hyper::client", level),
        "hyper::proto" => at_level!("hyper::proto", level),
        "tokio_util::codec" => at_level!("tokio_util::codec", level),
        other => panic!("add target {other} to this test"),
    }
}

#[test]
fn filter_cases() {
    let path = format!("{}/contract/log/cases.json", env!("CARGO_MANIFEST_DIR"));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cases = v["filter_cases"].as_array().unwrap();
    assert!(cases.len() > 40);
    for c in cases {
        let (spec, target, level) = (
            c["spec"].as_str().unwrap(),
            c["target"].as_str().unwrap(),
            c["level"].as_str().unwrap(),
        );
        let n = Arc::new(AtomicUsize::new(0));
        let sub = tracing_subscriber::registry()
            .with(Count(n.clone()))
            .with(EnvFilter::new(spec));
        tracing::subscriber::with_default(sub, || emit(target, level));
        let shown = n.load(Ordering::SeqCst) == 1;
        assert_eq!(
            shown,
            c["shown"].as_bool().unwrap(),
            "{spec} / {target} / {level}"
        );
    }
}
