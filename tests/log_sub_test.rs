//! The runner's subscriber: op failure lines, the panic line, URL
//! redaction, filter fallback, trace spans, bridged `log` records.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Arc;

use support::{Buf, TID, capture, op, pin};

/// A failing op (connection refused): one ERROR `op failed` line with a
/// fixed `error` word, and no path, URL or secret in it.
#[tokio::test(flavor = "current_thread")]
async fn op_failed_is_an_error_line_without_payload() {
    let _pin = pin();
    let buf = Buf::default();
    let (sub, logging) = zriz_runner::logfmt::subscriber_with_writer("info", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);
    let mut cfg = zriz_runner::config::Config::default();
    cfg.resources.insert(
        "web".to_string(),
        zriz_runner::config::Resource {
            r#type: "http".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            secrets: Some(vec!["S1".to_string()]),
            ..Default::default()
        },
    );
    let lookup: zriz_runner::ops::Lookup =
        Arc::new(|n| (n == "S1").then(|| "sekrit-value".to_string()));
    let opts = zriz_runner::ops::Options {
        lookup: Some(lookup),
        ..Default::default()
    };
    let handler = zriz_runner::ops::Handler::new(cfg, opts).await.unwrap();
    let mut o = op("http.request", "web");
    o.args.insert("method".into(), serde_json::json!("GET"));
    o.args
        .insert("path".into(), serde_json::json!("/private/path"));
    let (_, err) = handler.handle(o).await;
    let out = buf.text();
    assert!(err.is_some());
    assert!(out.contains(" ERROR trace_id=- "), "{out}");
    assert!(out.contains("runner.ops      op failed run_id=r1 step=3 op_id=o1 kind=http.request resource=web status=error reason=connection-error elapsed_ms="), "{out}");
    assert!(
        !out.contains("sekrit-value") && !out.contains("/private/path"),
        "{out}"
    );
    assert_eq!(logging.dropped(), 0, "{out}");
}

#[test]
fn panic_line_is_one_error_line_with_the_span_trace() {
    let _pin = pin();
    let out = capture("info", || {
        let span = tracing::error_span!("op", trace_id = %TID);
        let _e = span.enter();
        zriz_runner::logfmt::emit_panic("src/x.rs:88", "fixed text");
    });
    assert_eq!(out.lines().count(), 1);
    assert!(
        out.contains(&format!(
            " ERROR trace_id={TID} runner.main     panic location=src/x.rs:88 error=\"fixed text\""
        )),
        "{out}"
    );
}

/// A URL right after a multi-byte char is redacted, not a panic.
#[test]
fn url_after_multibyte_char_is_redacted() {
    let _pin = pin();
    for (msg, want) in [
        ("failed «https://u:p@h/x now", "error=\"failed «[url] now\""),
        ("é://x y", "error=\"é[url] y\""),
        ("ok ftp://h/x", "error=\"ok [url]\""),
    ] {
        let out = capture("warn", || {
            tracing::warn!(target: "hyper::client", "{msg}");
        });
        assert!(out.trim_end().ends_with(want), "{msg}: {out}");
    }
}

/// An invalid spec falls back to `DEFAULT_FILTER` (no lib INFO lines) and
/// says why; a valid one does not.
#[test]
fn invalid_filter_falls_back_to_the_default() {
    let _pin = pin();
    for (spec, lines, invalid) in [("runner=verbose", 1, true), ("info", 2, false)] {
        let buf = Buf::default();
        let (sub, logging) = zriz_runner::logfmt::subscriber_with_writer(spec, buf.clone());
        tracing::subscriber::with_default(sub, || {
            tracing::warn!(target: "hyper::client", "boom");
            tracing::info!(target: "hyper::client", "lib info");
            logging.report_config();
        });
        let out = buf.text();
        assert_eq!(logging.invalid().is_some(), invalid, "{spec}");
        let lib = out.lines().filter(|l| l.contains("runner.lib")).count();
        assert_eq!(lib, lines, "{spec}: {out}");
        let warned = out.contains("runner.main     config warning reason=invalid error=");
        assert_eq!(warned, invalid, "{spec}: {out}");
    }
}

/// The op span keeps its trace whatever the filter says about its target.
#[test]
fn trace_span_survives_any_filter() {
    let _pin = pin();
    for spec in [
        "warn,runner.ops=off",
        "hyper=warn",
        "runner.exchange=debug,warn",
    ] {
        let out = capture(spec, || {
            let span = tracing::error_span!(target: "runner.ops", "op", trace_id = %TID);
            let _e = span.enter();
            tracing::warn!(target: "hyper::client", "boom");
        });
        assert!(
            out.contains(&format!("trace_id={TID} runner.lib")),
            "{spec}: {out}"
        );
    }
}

/// A record from the `log` bridge (event target `log`, the real target in
/// `log.target`) becomes a `runner.lib` line for that crate, URLs redacted.
#[test]
fn bridged_log_record_is_a_redacted_lib_line() {
    let _pin = pin();
    let out = capture("warn", || {
        tracing::warn!(
            target: "log",
            message = "connect postgres://u:pw@db/x failed",
            log.target = "tokio_postgres::connection"
        );
    });
    assert!(
        out.trim_end().ends_with(
            " runner.lib      lib event location=tokio_postgres error=\"connect [url] failed\""
        ),
        "{out}"
    );
}
