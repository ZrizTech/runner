use super::*;
use crate::config::{Cloud, Config, Resource};
use crate::ops::tests::{fixed_now, test_lookup, test_op};
use crate::ops::{Handler, Options};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// A canned answer for one expected call to [`FakeSqlConn::query`], plus
/// the assertions to run against what was actually asked for — the
/// an expectation on the fake connection.
pub(crate) struct Expectation {
    pub want_read_only: bool,
    pub rows: std::result::Result<Vec<Map<String, Value>>, SqlOpError>,
}

/// A fake [`SqlConn`] used in place of a real database: each call to
/// `query` pops the next queued [`Expectation`] and asserts `read_only`
/// matched what the test expected. `close_count`, shared across every
/// [`FakeSqlConn`] an [`OpenSql`] hands out, lets a test count how many
/// resources actually got closed.
pub(crate) struct FakeSqlConn {
    expectations: Mutex<Vec<Expectation>>,
    close_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl FakeSqlConn {
    pub(crate) fn new(expectations: Vec<Expectation>) -> Arc<Self> {
        Arc::new(Self {
            expectations: Mutex::new(expectations),
            close_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    fn with_close_count(close_count: Arc<std::sync::atomic::AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            expectations: Mutex::new(Vec::new()),
            close_count,
        })
    }
}

impl SqlConn for FakeSqlConn {
    fn query<'a>(
        &'a self,
        _query: &'a str,
        _params: &'a [Value],
        read_only: bool,
    ) -> BoxFuture<'a, std::result::Result<Vec<Map<String, Value>>, SqlOpError>> {
        Box::pin(async move {
            let exp = {
                let mut guard = self.expectations.lock().unwrap_or_else(|e| e.into_inner());
                if guard.is_empty() {
                    panic!("unexpected sql call: no expectation queued");
                }
                guard.remove(0)
            };
            assert_eq!(exp.want_read_only, read_only, "read_only flag mismatch");
            exp.rows
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.close_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        })
    }
}

pub(crate) fn fake_open_sql() -> super::super::OpenSql {
    Arc::new(move |_dsn: &str| {
        Box::pin(async move { Ok(FakeSqlConn::new(Vec::new()) as Arc<dyn SqlConn>) })
            as BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>>
    })
}

/// Like [`fake_open_sql`], but every [`FakeSqlConn`] it hands out increments
/// `close_count` when closed, so a test can prove `Handler::close` reached
/// every sql resource it opened.
pub(crate) fn fake_open_sql_counting_closes(
    close_count: Arc<std::sync::atomic::AtomicUsize>,
) -> super::super::OpenSql {
    Arc::new(move |_dsn: &str| {
        let close_count = Arc::clone(&close_count);
        Box::pin(async move { Ok(FakeSqlConn::with_close_count(close_count) as Arc<dyn SqlConn>) })
            as BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>>
    })
}

/// An [`OpenSql`] that succeeds for its first `n_success` calls (handing
/// out a [`FakeSqlConn`] wired to `close_count`) and fails every call after
/// that — order-independent proof that `Handler::new` closes every
/// resource it already opened once a later one fails, regardless of which
/// resource id a `HashMap` iterates over first (this pins
/// the failing resource by call order, since Rust's `Config::resources` iterates in
/// an unspecified order).
pub(crate) fn fake_open_sql_fail_after(
    n_success: usize,
    close_count: Arc<std::sync::atomic::AtomicUsize>,
) -> super::super::OpenSql {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Arc::new(move |_dsn: &str| {
        let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let close_count = Arc::clone(&close_count);
        Box::pin(async move {
            if call < n_success {
                Ok(FakeSqlConn::with_close_count(close_count) as Arc<dyn SqlConn>)
            } else {
                Err(SqlOpenError("fake open failure".to_string()))
            }
        }) as BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>>
    })
}

fn sql_config() -> Config {
    let mut resources = HashMap::new();
    resources.insert(
        "db".to_string(),
        Resource {
            r#type: "sql".to_string(),
            connection: "dsn-db".to_string(),
            read_only: true,
            cookies: false,
            ..Default::default()
        },
    );
    resources.insert(
        "db-rw".to_string(),
        Resource {
            r#type: "sql".to_string(),
            connection: "dsn-db-rw".to_string(),
            read_only: false,
            cookies: false,
            ..Default::default()
        },
    );
    Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    }
}

/// Builds a Handler wired to fake sql connections for "db" (read-only)
/// and "db-rw" (not), each answering with `db_rows`/`db_rw_rows` in
/// order.
async fn fixture(
    db_expectations: Vec<Expectation>,
    db_rw_expectations: Vec<Expectation>,
) -> Handler {
    let db_conn = FakeSqlConn::new(db_expectations);
    let db_rw_conn = FakeSqlConn::new(db_rw_expectations);
    let open_sql: super::super::OpenSql = Arc::new(move |dsn: &str| {
        let conn: Arc<dyn SqlConn> = if dsn == "dsn-db" {
            Arc::clone(&db_conn) as Arc<dyn SqlConn>
        } else {
            Arc::clone(&db_rw_conn) as Arc<dyn SqlConn>
        };
        Box::pin(async move { Ok(conn) })
            as BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>>
    });

    let mut env = HashMap::new();
    env.insert("TOKEN", "s3cret-token");
    env.insert("ZRIZ_CANARY", "canary-value");
    Handler::new(
        sql_config(),
        Options {
            open_sql: Some(open_sql),
            lookup: Some(test_lookup(env)),
            now: Some(fixed_now(SystemTime::UNIX_EPOCH)),
        },
    )
    .await
    .expect("Handler::new")
}

#[tokio::test]
async fn read_only_refuses_write_before_reaching_conn() {
    let h = fixture(vec![], vec![]).await;
    let mut args = HashMap::new();
    args.insert(
        "query".to_string(),
        Value::String("INSERT INTO t VALUES (1)".to_string()),
    );
    let (result, err) = h
        .handle(test_op(
            "op-insert",
            "run-s",
            "sql.query",
            "db",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    assert_eq!(err.unwrap().reason, "read-only");
}

#[tokio::test]
async fn read_only_query_projects_rows_in_transaction() {
    let mut row0 = Map::new();
    row0.insert("id".to_string(), Value::from(1));
    row0.insert("email".to_string(), Value::String("a@b".to_string()));
    let mut row1 = Map::new();
    row1.insert("id".to_string(), Value::from(2));
    row1.insert("email".to_string(), Value::String("c@d".to_string()));

    let h = fixture(
        vec![Expectation {
            want_read_only: true,
            rows: Ok(vec![row0, row1]),
        }],
        vec![],
    )
    .await;

    let mut args = HashMap::new();
    args.insert(
        "query".to_string(),
        Value::String("SELECT id, email FROM users WHERE email = ?".to_string()),
    );
    args.insert(
        "params".to_string(),
        Value::Array(vec![Value::String("a@b".to_string())]),
    );
    let project = vec![
        vec!["rows".to_string(), "0".to_string(), "email".to_string()],
        vec!["row-count".to_string()],
    ];
    let (result, err) = h
        .handle(test_op(
            "op-select",
            "run-s",
            "sql.query",
            "db",
            2000,
            args,
            project,
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");

    let rows = result
        .payload
        .get("rows")
        .and_then(|v| v.as_array())
        .expect("rows array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("email").and_then(|v| v.as_str()), Some("a@b"));
    assert_eq!(rows[1].get("email").and_then(|v| v.as_str()), Some("c@d"));
    assert_eq!(
        result.payload.get("row-count").and_then(|v| v.as_i64()),
        Some(2)
    );
}

#[tokio::test]
async fn rows_capped_at_100() {
    let rows: Vec<Map<String, Value>> = (0..150)
        .map(|i| {
            let mut m = Map::new();
            m.insert("id".to_string(), Value::from(i));
            m
        })
        .collect();
    let h = fixture(
        vec![Expectation {
            want_read_only: true,
            rows: Ok(rows),
        }],
        vec![],
    )
    .await;

    let mut args = HashMap::new();
    args.insert(
        "query".to_string(),
        Value::String("SELECT id FROM big".to_string()),
    );
    let project = vec![vec!["rows".to_string(), "0".to_string(), "id".to_string()]];
    let (result, err) = h
        .handle(test_op(
            "op-big",
            "run-s",
            "sql.query",
            "db",
            2000,
            args,
            project,
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    let rows = result
        .payload
        .get("rows")
        .and_then(|v| v.as_array())
        .expect("rows");
    assert_eq!(rows.len(), 100);
    assert_eq!(
        result.payload.get("row-count").and_then(|v| v.as_i64()),
        Some(150)
    );
}

#[tokio::test]
async fn empty_projection_yields_no_rows() {
    let mut row0 = Map::new();
    row0.insert("id".to_string(), Value::from(1));
    let mut row1 = Map::new();
    row1.insert("id".to_string(), Value::from(2));
    let h = fixture(
        vec![Expectation {
            want_read_only: true,
            rows: Ok(vec![row0, row1]),
        }],
        vec![],
    )
    .await;

    let mut args = HashMap::new();
    args.insert(
        "query".to_string(),
        Value::String("SELECT id FROM t".to_string()),
    );
    let (result, err) = h
        .handle(test_op(
            "op-empty-proj",
            "run-s",
            "sql.query",
            "db",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    let rows = result
        .payload
        .get("rows")
        .and_then(|v| v.as_array())
        .expect("rows");
    assert!(rows.is_empty());
    assert_eq!(
        result.payload.get("row-count").and_then(|v| v.as_i64()),
        Some(2)
    );
}

#[tokio::test]
async fn read_write_resource_skips_read_only_check() {
    let h = fixture(
        vec![],
        vec![Expectation {
            want_read_only: false,
            rows: Ok(vec![]),
        }],
    )
    .await;

    let mut args = HashMap::new();
    args.insert(
        "query".to_string(),
        Value::String("INSERT INTO t VALUES (1)".to_string()),
    );
    let (result, err) = h
        .handle(test_op(
            "op-insert-rw",
            "run-s",
            "sql.query",
            "db-rw",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    assert_eq!(result.expect("result").status, "pass");
}

#[tokio::test]
async fn driver_error_hides_query_text() {
    let query = "SELECT id FROM secret_table_name";
    let h = fixture(
        vec![Expectation {
            want_read_only: true,
            rows: Err(SqlOpError::Other),
        }],
        vec![],
    )
    .await;

    let mut args = HashMap::new();
    args.insert("query".to_string(), Value::String(query.to_string()));
    let (result, err) = h
        .handle(test_op(
            "op-err",
            "run-s",
            "sql.query",
            "db",
            2000,
            args,
            vec![],
        ))
        .await;
    assert!(result.is_none());
    let err = err.expect("error");
    assert_eq!(err.reason, "runner-error");
    assert_eq!(err.details["where"], "sql-driver");
    assert!(!serde_json::to_string(&err).expect("json").contains(query));
}

async fn run_with_fault(fault: SqlOpError) -> (Option<ResultFrame>, Option<ErrorFrame>) {
    let h = fixture(
        vec![Expectation {
            want_read_only: true,
            rows: Err(fault),
        }],
        vec![],
    )
    .await;
    let mut args = HashMap::new();
    args.insert("query".to_string(), Value::String("SELECT 1".into()));
    h.handle(test_op(
        "op-f",
        "run-s",
        "sql.query",
        "db",
        2000,
        args,
        vec![],
    ))
    .await
}

#[tokio::test]
async fn a_database_error_is_a_failed_result_with_no_text() {
    let (result, err) = run_with_fault(SqlOpError::Database).await;
    assert!(err.is_none(), "{err:?}");
    let r = result.expect("result");
    assert_eq!(r.status, "fail");
    assert_eq!(r.payload["rows"], serde_json::json!([]));
    assert_eq!(r.payload["row-count"], 0);
    assert_eq!(r.scrubbed, 0);
}

#[tokio::test]
async fn a_failed_connect_is_connection_error_and_other_faults_stay_driver() {
    let (r, err) = run_with_fault(SqlOpError::Connect).await;
    let err = err.expect("error");
    assert!(r.is_none());
    assert_eq!(
        (err.reason.as_str(), err.details.len()),
        ("connection-error", 0)
    );
    let (_, err) = run_with_fault(SqlOpError::Other).await;
    assert_eq!(err.expect("error").details["where"], "sql-driver");
}

// --- pure unit tests: DSN parsing and value conversion ---

#[test]
fn parse_dsn_table() {
    struct Case {
        name: &'static str,
        dsn: &'static str,
        want: std::result::Result<ParsedDsn, ()>,
    }
    let cases = vec![
        Case {
            name: "user, password, default port",
            dsn: "shop:hunter2@tcp(shop-mysql:3306)/shop",
            want: Ok(ParsedDsn {
                user: "shop".into(),
                password: "hunter2".into(),
                host: "shop-mysql".into(),
                port: 3306,
                db_name: "shop".into(),
            }),
        },
        Case {
            name: "no password",
            dsn: "shop@tcp(shop-mysql:3306)/shop",
            want: Ok(ParsedDsn {
                user: "shop".into(),
                password: String::new(),
                host: "shop-mysql".into(),
                port: 3306,
                db_name: "shop".into(),
            }),
        },
        Case {
            name: "no user",
            dsn: "tcp(shop-mysql:3306)/shop",
            want: Ok(ParsedDsn {
                user: String::new(),
                password: String::new(),
                host: "shop-mysql".into(),
                port: 3306,
                db_name: "shop".into(),
            }),
        },
        Case {
            name: "default port when omitted",
            dsn: "shop:hunter2@tcp(shop-mysql)/shop",
            want: Ok(ParsedDsn {
                user: "shop".into(),
                password: "hunter2".into(),
                host: "shop-mysql".into(),
                port: 3306,
                db_name: "shop".into(),
            }),
        },
        Case {
            name: "query params ignored",
            dsn: "shop:hunter2@tcp(shop-mysql:3306)/shop?parseTime=true&loc=UTC",
            want: Ok(ParsedDsn {
                user: "shop".into(),
                password: "hunter2".into(),
                host: "shop-mysql".into(),
                port: 3306,
                db_name: "shop".into(),
            }),
        },
        Case {
            name: "ipv6 host with port",
            dsn: "shop:hunter2@tcp([::1]:3306)/shop",
            want: Ok(ParsedDsn {
                user: "shop".into(),
                password: "hunter2".into(),
                host: "::1".into(),
                port: 3306,
                db_name: "shop".into(),
            }),
        },
        Case {
            name: "ipv6 host default port",
            dsn: "shop:hunter2@tcp([::1])/shop",
            want: Ok(ParsedDsn {
                user: "shop".into(),
                password: "hunter2".into(),
                host: "::1".into(),
                port: 3306,
                db_name: "shop".into(),
            }),
        },
        Case {
            name: "missing tcp segment",
            dsn: "shop:hunter2@shop-mysql:3306/shop",
            want: Err(()),
        },
        Case {
            name: "missing db name",
            dsn: "shop:hunter2@tcp(shop-mysql:3306)/",
            want: Err(()),
        },
        Case {
            name: "invalid port",
            dsn: "shop:hunter2@tcp(shop-mysql:notaport)/shop",
            want: Err(()),
        },
    ];

    for c in cases {
        let got = parse_dsn(c.dsn);
        match c.want {
            Ok(want) => assert_eq!(got, Ok(want), "case {}", c.name),
            Err(()) => assert!(got.is_err(), "case {}: want err, got {:?}", c.name, got),
        }
    }
}

#[test]
fn json_to_sql_value_table() {
    assert_eq!(json_to_sql_value(&Value::Null), mysql_async::Value::NULL);
    assert_eq!(
        json_to_sql_value(&Value::Bool(true)),
        mysql_async::Value::Int(1)
    );
    assert_eq!(
        json_to_sql_value(&Value::String("a@b".to_string())),
        mysql_async::Value::Bytes(b"a@b".to_vec())
    );
    assert_eq!(
        json_to_sql_value(&Value::from(42)),
        mysql_async::Value::Bytes(b"42".to_vec())
    );
}

#[test]
fn sql_value_to_json_table() {
    use mysql_async::consts::ColumnType::*;
    assert_eq!(
        sql_value_to_json(mysql_async::Value::NULL, MYSQL_TYPE_VARCHAR),
        Value::Null
    );
    assert_eq!(
        sql_value_to_json(
            mysql_async::Value::Bytes(b"a@b".to_vec()),
            MYSQL_TYPE_VARCHAR
        ),
        Value::String("a@b".to_string())
    );
    assert_eq!(
        sql_value_to_json(mysql_async::Value::Int(42), MYSQL_TYPE_LONGLONG),
        Value::from(42)
    );
    assert_eq!(
        sql_value_to_json(mysql_async::Value::UInt(7), MYSQL_TYPE_LONGLONG),
        Value::from(7)
    );
    assert_eq!(
        sql_value_to_json(mysql_async::Value::Double(1.5), MYSQL_TYPE_DOUBLE),
        Value::from(1.5)
    );
    assert_eq!(
        sql_value_to_json(
            mysql_async::Value::Date(2024, 1, 2, 0, 0, 0, 0),
            MYSQL_TYPE_DATE
        ),
        Value::String("2024-01-02".to_string())
    );
    assert_eq!(
        sql_value_to_json(
            mysql_async::Value::Date(2024, 1, 2, 15, 4, 5, 0),
            MYSQL_TYPE_DATETIME
        ),
        Value::String("2024-01-02 15:04:05".to_string())
    );
    assert_eq!(
        sql_value_to_json(
            mysql_async::Value::Time(false, 0, 1, 2, 3, 0),
            MYSQL_TYPE_TIME
        ),
        Value::String("01:02:03".to_string())
    );
}

/// Every sql resource is capped at 2 open and 2 idle connections.
/// `default_open_sql`'s pool must carry the same cap on both
/// bounds, not `mysql_async`'s own defaults (min 10, max 100).
#[test]
fn build_opts_caps_pool_at_go_max_open_conns() {
    let parsed = parse_dsn("user:pass@tcp(127.0.0.1:3306)/db").expect("valid dsn");
    let builder = build_opts(&parsed);
    let opts = mysql_async::Opts::from(builder);
    let constraints = opts.pool_opts().constraints();
    assert_eq!(constraints.min(), SQL_POOL_MAX_CONNS);
    assert_eq!(constraints.max(), SQL_POOL_MAX_CONNS);
    assert_eq!(SQL_POOL_MAX_CONNS, 2, "pool cap is 2");
}

#[tokio::test]
async fn capture_column_value() {
    let mut row = Map::new();
    row.insert("id".to_string(), Value::from(1));
    row.insert(
        "secret".to_string(),
        Value::String("col-secret-42".to_string()),
    );
    let h = fixture(
        vec![Expectation {
            want_read_only: true,
            rows: Ok(vec![row]),
        }],
        vec![],
    )
    .await;
    let mut args = HashMap::new();
    args.insert(
        "query".to_string(),
        Value::String("SELECT id, secret FROM t".to_string()),
    );
    args.insert(
        "capture".to_string(),
        serde_json::json!({"COLSEC": ["rows", "0", "secret"]}),
    );
    let project = vec![vec!["rows".to_string(), "0".to_string(), "id".to_string()]];
    let op = test_op("op", "run-s", "sql.query", "db", 2000, args, project);
    let (r, e) = h.handle(op).await;
    assert!(e.is_none(), "{e:?}");
    let r = r.expect("result");
    let text = serde_json::to_string(&r.payload).expect("json");
    assert!(!text.contains("col-secret-42"), "{text}");
    assert_eq!(
        r.payload["rows"][0]["secret"],
        serde_json::json!("[captured]")
    );
    assert_eq!(r.payload["rows"][0]["id"], serde_json::json!(1));
    assert_eq!(
        h.vault.get("run-s", "COLSEC", SystemTime::UNIX_EPOCH),
        Some("col-secret-42".to_string())
    );
}

#[tokio::test]
async fn sql_takes_no_placeholders_even_when_listed() {
    for secrets in [None, Some(vec!["ANY".to_string()])] {
        let mut cfg = sql_config();
        if let Some(r) = cfg.resources.get_mut("db") {
            r.secrets = secrets;
        }
        let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let lookup: crate::ops::Lookup = Arc::new(move |name: &str| {
            seen.lock().expect("lock").push(name.to_string());
            Some("env-value".to_string())
        });
        let open_sql: super::super::OpenSql = Arc::new(|_dsn: &str| {
            let conn: Arc<dyn SqlConn> = FakeSqlConn::new(vec![]) as Arc<dyn SqlConn>;
            Box::pin(async move { Ok(conn) })
                as BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>>
        });
        let h = Handler::new(
            cfg,
            Options {
                open_sql: Some(open_sql),
                lookup: Some(lookup),
                now: Some(fixed_now(SystemTime::UNIX_EPOCH)),
            },
        )
        .await
        .expect("Handler::new");
        // Building the handler resolves the listed names once.
        asked.lock().expect("lock").clear();
        let mut args = HashMap::new();
        args.insert(
            "query".to_string(),
            Value::String("SELECT ? AS v".to_string()),
        );
        args.insert(
            "params".to_string(),
            Value::Array(vec![Value::String("${ANY}".to_string())]),
        );
        // An empty expectation list makes any query call fail the test.
        let (result, err) = h
            .handle(test_op(
                "op-any",
                "run-s",
                "sql.query",
                "db",
                2000,
                args,
                vec![],
            ))
            .await;
        assert!(result.is_none());
        assert_eq!(err.expect("error").reason, "placeholder-in-disallowed-slot");
        assert!(asked.lock().expect("lock").is_empty(), "lookup was asked");
    }
}

#[tokio::test]
async fn rows_are_capped_at_max_sql_rows_in_result_and_evidence() {
    let rows: Vec<Map<String, Value>> = (0..5000)
        .map(|i| {
            let mut m = Map::new();
            m.insert("id".to_string(), Value::from(i));
            m
        })
        .collect();
    let h = fixture(
        vec![Expectation {
            want_read_only: true,
            rows: Ok(rows),
        }],
        vec![],
    )
    .await;
    let mut args = HashMap::new();
    args.insert(
        "query".to_string(),
        Value::String("SELECT id FROM big".to_string()),
    );
    let (result, err) = h
        .handle(test_op(
            "op-big",
            "run-big",
            "sql.query",
            "db",
            2000,
            args,
            vec![vec!["row-count".to_string()]],
        ))
        .await;
    assert!(err.is_none(), "unexpected error: {err:?}");
    let result = result.expect("result");
    assert_eq!(
        result.payload.get("row-count").and_then(|v| v.as_i64()),
        Some(1000)
    );
    let entry = h
        .evidence
        .get("run-big", fixed_now(SystemTime::UNIX_EPOCH)())
        .expect("evidence entry");
    let n = entry.body.as_array().map(|a| a.len()).unwrap_or(usize::MAX);
    assert!(n <= 1000, "evidence holds {n} rows");
}

/// Called when a real-database test cannot reach its database. Locally the
/// test skips with a note; with `ZRIZ_TEST_DB_REQUIRED=1` (set in CI) it fails,
/// so a test that guards a claim can never silently skip there.
pub(crate) fn database_unreachable(name: &str, env_var: &str) {
    assert!(
        std::env::var("ZRIZ_TEST_DB_REQUIRED").as_deref() != Ok("1"),
        "{name} not reachable at ${env_var} and ZRIZ_TEST_DB_REQUIRED=1: the test may not skip"
    );
    eprintln!("note: {name} not reachable, test skipped");
}
