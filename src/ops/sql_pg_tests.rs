//! Tests against a real Postgres (`ZRIZ_TEST_PG`, default the local
//! `zriz-rs-pg` container). If it is not reachable, a test prints a note and
//! passes, unless `ZRIZ_TEST_DB_REQUIRED=1` (CI), then it fails. Each test makes its own scratch schema and drops it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::ops::SqlConn;
use serde_json::json;

fn dsn() -> String {
    std::env::var("ZRIZ_TEST_PG")
        .unwrap_or_else(|_| "postgres://zriz:zriz@localhost:5435/zriz?sslmode=disable".to_string())
}

/// Returns a read-write conn and a fresh schema name, or None if no DB.
async fn setup() -> Option<(PgConn, String)> {
    let conn = open(&dsn()).unwrap();
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    match conn
        .query(&format!("CREATE SCHEMA {schema}"), &[], false)
        .await
    {
        Ok(_) => Some((conn, schema)),
        Err(_) => {
            crate::ops::sql::tests::database_unreachable("postgres", "ZRIZ_TEST_PG");
            None
        }
    }
}

async fn drop_schema(conn: &PgConn, schema: &str) {
    conn.query(&format!("DROP SCHEMA {schema} CASCADE"), &[], false)
        .await
        .unwrap();
}

#[test]
fn rewrite_outside_quotes_and_comments() {
    assert_eq!(
        rewrite_placeholders("select * from t where a = ? and b = ?"),
        "select * from t where a = $1 and b = $2"
    );
    assert_eq!(
        rewrite_placeholders("select '?', \"a?\", 'it''s ?' , ? -- why?\n, ? /* ? */ , ?"),
        "select '?', \"a?\", 'it''s ?' , $1 -- why?\n, $2 /* ? */ , $3"
    );
    assert_eq!(rewrite_placeholders("select 'é', ?"), "select 'é', $1");
}

#[test]
fn dsn_scheme_and_open_errors_hide_secret() {
    assert!(is_postgres_dsn("postgres://a@h/d"));
    assert!(is_postgres_dsn("postgresql://a@h/d"));
    assert!(!is_postgres_dsn("u:p@tcp(h:3306)/d"));
    let err = open("postgres://u:s3cretpw@h:notaport/d").err().unwrap();
    assert!(!err.0.contains("s3cretpw"), "{}", err.0);
    assert!(!err.0.contains("notaport"), "{}", err.0);
}

#[tokio::test]
async fn select_two_params_and_literal_question_mark() {
    let Some((c, s)) = setup().await else { return };
    c.query(
        &format!("CREATE TABLE {s}.t (id int, name text)"),
        &[],
        false,
    )
    .await
    .unwrap();
    c.query(
        &format!("INSERT INTO {s}.t VALUES (1,'a?'),(2,'b'),(3,'c')"),
        &[],
        false,
    )
    .await
    .unwrap();
    let rows = c
        .query(
            &format!(
                "SELECT id, name, '?' AS q FROM {s}.t WHERE id >= ? AND name <> ? ORDER BY id"
            ),
            &[json!(1), json!("b")],
            true,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], json!(1));
    assert_eq!(rows[0]["name"], json!("a?"));
    assert_eq!(rows[0]["q"], json!("?"));
    assert_eq!(rows[1]["id"], json!(3));
    // numeric string binds to an int slot, like MySQL's text binding
    let rows = c
        .query(
            &format!("SELECT id FROM {s}.t WHERE id = ?"),
            &[json!("2")],
            true,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    drop_schema(&c, &s).await;
}

#[tokio::test]
async fn types_mapping() {
    let Some((c, s)) = setup().await else { return };
    let q = "SELECT 7::int2 AS i2, 8::int4 AS i4, 9::int8 AS i8, 1.5::float8 AS f8, \
             12345678901234567890.0012::numeric(30,4) AS n, 0.05::numeric AS n2, -1200::numeric AS n3, \
             true AS b, 'x'::varchar AS v, '{\"a\":[1]}'::jsonb AS j, \
             '6f1c1c7e-3a5b-4d3e-9a55-0b2f5d3b7c11'::uuid AS u, \
             '2026-01-02 03:04:05+00'::timestamptz AS tz, '2026-01-02 03:04:05.5'::timestamp AS ts, \
             '2026-01-02'::date AS d, NULL::text AS nul, '\\xdead'::bytea AS by";
    let rows = c.query(q, &[], true).await.unwrap();
    let r = &rows[0];
    assert_eq!(r["i2"], json!(7));
    assert_eq!(r["i4"], json!(8));
    assert_eq!(r["i8"], json!(9));
    assert_eq!(r["f8"], json!(1.5));
    assert_eq!(r["n"], json!("12345678901234567890.0012"));
    assert_eq!(r["n2"], json!("0.05"));
    assert_eq!(r["n3"], json!("-1200"));
    assert_eq!(r["b"], json!(true));
    assert_eq!(r["v"], json!("x"));
    assert_eq!(r["j"], json!({"a": [1]}));
    assert_eq!(r["u"], json!("6f1c1c7e-3a5b-4d3e-9a55-0b2f5d3b7c11"));
    assert_eq!(r["tz"], json!("2026-01-02T03:04:05Z"));
    assert_eq!(r["ts"], json!("2026-01-02T03:04:05.500Z"));
    assert_eq!(r["d"], json!("2026-01-02"));
    assert_eq!(r["nul"], Value::Null);
    assert_eq!(r["by"], json!("\\xdead"));
    drop_schema(&c, &s).await;
}

#[tokio::test]
async fn write_on_read_only_is_refused_and_nothing_written() {
    let Some((c, s)) = setup().await else { return };
    c.query(&format!("CREATE TABLE {s}.t (id int)"), &[], false)
        .await
        .unwrap();
    // A data-modifying CTE gets past a keyword pre-check; the transaction stops it.
    let sneaky = format!("WITH x AS (INSERT INTO {s}.t VALUES (1) RETURNING id) SELECT id FROM x");
    assert!(c.query(&sneaky, &[], true).await.is_err());
    assert!(
        c.query(&format!("INSERT INTO {s}.t VALUES (2)"), &[], true)
            .await
            .is_err()
    );
    let rows = c
        .query(&format!("SELECT count(*) AS n FROM {s}.t"), &[], false)
        .await
        .unwrap();
    assert_eq!(rows[0]["n"], json!(0));
    // the pool still works after the failures
    assert!(c.query("SELECT 1 AS one", &[], true).await.is_ok());
    drop_schema(&c, &s).await;
}

#[tokio::test]
async fn unknown_table_is_a_bare_error() {
    let Some((c, s)) = setup().await else { return };
    let err = c
        .query(&format!("SELECT * FROM {s}.nope"), &[], true)
        .await
        .err()
        .unwrap();
    assert_eq!(err.to_string(), "sql: query failed");
    drop_schema(&c, &s).await;
}

#[tokio::test]
async fn bad_credentials_and_dead_host_never_leak_the_secret() {
    let conn = open("postgres://zriz:s3cretpw@localhost:1/zriz?sslmode=disable").unwrap();
    let err = conn.query("SELECT 1", &[], true).await.err().unwrap();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("s3cretpw"), "{text}");
    // wrong password against the real server
    let dsn = dsn().replace("zriz:zriz@", "zriz:s3cretpw@");
    let conn = open(&dsn).unwrap();
    let err = conn.query("SELECT 1", &[], true).await.err().unwrap();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("s3cretpw"), "{text}");
}

#[tokio::test]
async fn result_is_capped_at_max_sql_rows_while_reading() {
    let Some((c, s)) = setup().await else { return };
    for read_only in [true, false] {
        let rows = c
            .query(
                "SELECT g AS n FROM generate_series(1, 5000) AS g",
                &[],
                read_only,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), crate::ops::sql::MAX_SQL_ROWS);
    }
    drop_schema(&c, &s).await;
}
