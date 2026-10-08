//! Tests against a real MySQL (`ZRIZ_TEST_MYSQL`, a DSN like
//! `user:pass@tcp(host:3306)/db`). If it is not reachable, a test prints a
//! note and passes, unless `ZRIZ_TEST_DB_REQUIRED=1` (CI), then it fails.
//! Each test makes its own scratch table and drops it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use serde_json::json;

fn conn() -> MysqlConn {
    let dsn = std::env::var("ZRIZ_TEST_MYSQL")
        .unwrap_or_else(|_| "zriz:zriz@tcp(localhost:3306)/zriz".to_string());
    let parsed = parse_dsn(&dsn).unwrap();
    MysqlConn {
        pool: mysql_async::Pool::new(build_opts(&parsed)),
    }
}

/// Returns a read-write conn and a fresh table name, or None if no DB.
async fn setup() -> Option<(MysqlConn, String)> {
    let c = conn();
    let table = format!("t_{}", uuid::Uuid::new_v4().simple());
    match c
        .query(&format!("CREATE TABLE {table} (id int)"), &[], false)
        .await
    {
        Ok(_) => Some((c, table)),
        Err(_) => {
            super::tests::database_unreachable("mysql", "ZRIZ_TEST_MYSQL");
            None
        }
    }
}

#[tokio::test]
async fn write_on_read_only_is_refused_and_nothing_written() {
    let Some((c, t)) = setup().await else { return };
    // The pre-check would stop these; here they go straight to the
    // transaction, which is the layer that must refuse them.
    for write in [
        format!("INSERT INTO {t} VALUES (1)"),
        format!("INSERT INTO {t} SELECT 2"),
        format!("UPDATE {t} SET id = 3"),
        format!("DELETE FROM {t}"),
    ] {
        assert!(c.query(&write, &[], true).await.is_err(), "{write}");
    }
    // the same write is accepted on a read-write connection: the refusal was the transaction
    assert!(
        c.query(&format!("INSERT INTO {t} VALUES (?)"), &[json!(9)], false)
            .await
            .is_ok()
    );
    let rows = c
        .query(&format!("SELECT id FROM {t}"), &[], true)
        .await
        .unwrap();
    assert_eq!(rows, vec![json!({"id": 9}).as_object().unwrap().clone()]);
    // the pool still works after the failures
    assert!(c.query("SELECT 1 AS one", &[], true).await.is_ok());
    c.query(&format!("DROP TABLE {t}"), &[], false)
        .await
        .unwrap();
}
