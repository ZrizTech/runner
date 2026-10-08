//! The real panic hook (process-global, so its own test binary): a literal
//! message is logged, a formatted one (it may quote data) is withheld.
#![allow(clippy::unwrap_used, clippy::panic)]

mod support;

use support::{Buf, pin};

#[test]
fn panic_hook_logs_only_literal_messages() {
    let _pin = pin();
    let buf = Buf::default();
    let (sub, _) = zriz_runner::logfmt::subscriber_with_writer("info", buf.clone());
    tracing::subscriber::with_default(sub, || {
        zriz_runner::logfmt::install_panic_hook();
        let secret = "sekrit-row-value".to_string();
        let a = std::panic::catch_unwind(|| panic!("fixed text"));
        let b = std::panic::catch_unwind(move || panic!("bad {secret}"));
        let _ = std::panic::take_hook();
        assert!(a.is_err() && b.is_err());
    });
    let out = buf.text();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].ends_with("error=\"fixed text\""), "{out}");
    assert!(
        lines[1].ends_with("error=\"formatted panic message withheld\""),
        "{out}"
    );
    assert!(!out.contains("sekrit"), "{out}");
}
