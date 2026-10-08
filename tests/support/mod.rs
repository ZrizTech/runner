//! Shared helpers for the log tests: an in-memory writer, a sample op.
#![allow(dead_code, clippy::unwrap_used)]

use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
pub struct Buf(Arc<Mutex<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl MakeWriter<'_> for Buf {
    type Writer = Buf;
    fn make_writer(&self) -> Buf {
        self.clone()
    }
}

impl Buf {
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Keep the result alive for the whole test. With only one dispatcher,
/// tracing lets a thread that has no subscriber cache "never" for a
/// callsite it reaches first; a second one keeps interest re-checked.
pub fn pin() -> tracing::Dispatch {
    tracing::Dispatch::new(tracing_subscriber::registry())
}

/// Runs `f` under a fresh subscriber for `spec`; returns the lines written.
pub fn capture(spec: &str, f: impl FnOnce()) -> String {
    let buf = Buf::default();
    let (sub, _) = zriz_runner::logfmt::subscriber_with_writer(spec, buf.clone());
    tracing::subscriber::with_default(sub, f);
    buf.text()
}

pub const TID: &str = "6916eece-8a3c-43b0-8280-a90a4ff00b15";

pub fn op(kind: &str, resource: &str) -> zriz_runner::contract::Op {
    zriz_runner::contract::Op {
        op_id: "o1".to_string(),
        run_id: "r1".to_string(),
        step_index: 3,
        kind: kind.to_string(),
        resource: resource.to_string(),
        timeout_ms: 2000,
        args: Default::default(),
        project: vec![],
        trace_id: Some(TID.to_string()),
    }
}
