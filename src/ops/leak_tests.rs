//! One test: the frames and the log lines of the new error paths carry no
//! planted secret and no planted driver or library text.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::config::{Config, Resource};
use crate::ops::tests::test_op;
use crate::ops::{Handler, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
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

const PLANT: &str = "PLANTED-9f3c";
const DRIVER: &str = "DRIVER-MSG-77aa";

struct Failing(crate::ops::SqlOpError);
impl crate::ops::SqlConn for Failing {
    fn query<'a>(
        &'a self,
        _q: &'a str,
        _p: &'a [Value],
        _ro: bool,
    ) -> crate::exchange::BoxFuture<
        'a,
        std::result::Result<Vec<serde_json::Map<String, Value>>, crate::ops::SqlOpError>,
    > {
        let e = self.0;
        Box::pin(async move { Err(e) })
    }
    fn close<'a>(&'a self) -> crate::exchange::BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
}

#[tokio::test]
async fn new_paths_leak_no_secret_and_no_driver_text() {
    let _pin = tracing::Dispatch::new(tracing_subscriber::registry());
    let buf = Buf::default();
    let (sub, _logging) = crate::logfmt::subscriber_with_writer("trace", buf.clone());
    let _guard = tracing::subscriber::set_default(sub);

    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let mut resources = HashMap::new();
    resources.insert(
        "api".to_string(),
        Resource {
            r#type: "http".into(),
            base_url: dead,
            ..Default::default()
        },
    );
    resources.insert(
        "db".to_string(),
        Resource {
            r#type: "sql".into(),
            connection: format!("{DRIVER}:{PLANT}@tcp(x:1)/y"),
            ..Default::default()
        },
    );
    let cfg = Config {
        resources,
        ..Default::default()
    };
    let make = |e: crate::ops::SqlOpError| -> crate::ops::OpenSql {
        Arc::new(move |_| {
            let c: Arc<dyn crate::ops::SqlConn> = Arc::new(Failing(e));
            Box::pin(async move { Ok(c) })
        })
    };
    let mut frames = Vec::new();
    for fault in [
        crate::ops::SqlOpError::Database,
        crate::ops::SqlOpError::Connect,
        crate::ops::SqlOpError::Other,
    ] {
        let opts = Options {
            open_sql: Some(make(fault)),
            lookup: Some(Arc::new(|_| None)),
            ..Default::default()
        };
        let h = Handler::new(cfg.clone(), opts).await.unwrap();
        let args = HashMap::from([("query".to_string(), json!(format!("SELECT '{PLANT}'")))]);
        frames.push(
            h.handle(test_op("o1", "r1", "sql.query", "db", 2000, args, vec![]))
                .await,
        );
        let args = HashMap::from([("path".to_string(), json!(format!("/x?k={PLANT}")))]);
        frames.push(
            h.handle(test_op(
                "o2",
                "r1",
                "http.request",
                "api",
                2000,
                args,
                vec![],
            ))
            .await,
        );
        h.run_ended("r1", "").await;
        let args = HashMap::from([("path".to_string(), json!(format!("/x?k={PLANT}")))]);
        frames.push(
            h.handle(test_op(
                "o3",
                "r1",
                "http.request",
                "api",
                2000,
                args,
                vec![],
            ))
            .await,
        );
    }
    let all = serde_json::to_string(
        &frames
            .iter()
            .map(|(r, e)| json!([r, e]))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    for text in [&all, &logs] {
        assert!(!text.contains(PLANT), "{text}");
        assert!(!text.contains(DRIVER), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
    }
    assert!(all.contains("connection-error") && all.contains("run-closed"));
    assert!(
        logs.contains("op failed"),
        "the lines were captured: {logs}"
    );
}
