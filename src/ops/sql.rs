//! Implements the sql.query op kind: substitute args, enforce read-only when
//! the resource requires it, run the query, then shape the rows into a
//! Result.
//!
//! The actual driver call goes through the [`SqlConn`] trait rather than a
//! concrete `mysql_async` type, so tests can run the same value-conversion
//! and dispatch cases against a small fake connection instead of a real
//! database.

use super::{
    BoxFuture, Handler, OpenSql, SqlOpenError, capture, duration_ms, new_error, scrub_payload,
};
use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame, Timing};
use crate::{project, readonly};
use serde_json::{Map, Value};
use std::sync::Arc;

/// Errors parsing a MySQL DSN
/// (`user[:pass]@tcp(host[:port])/db[?params]`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DsnError {
    #[error("dsn: missing tcp(host[:port]) segment")]
    MissingTcp,
    #[error("dsn: missing database name")]
    MissingDb,
    #[error("dsn: invalid port {0:?}")]
    InvalidPort(String),
}

/// A MySQL DSN's pieces, parsed structurally (no driver-specific query
/// params are interpreted; the config format never sets any).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedDsn {
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    pub db_name: String,
}

/// Parses a DSN of the form `user[:pass]@tcp(host[:port])/db[?params]`, the
/// format `zriz-runner`'s config uses for a sql resource's `connection`
/// (the common MySQL driver grammar, for the pieces this runner needs).
/// Any `?params` suffix is accepted but ignored: the config never sets
/// one, and the driver defaults (no TLS, `parseTime=false`) are exactly
/// what this parser's callers assume.
pub fn parse_dsn(dsn: &str) -> std::result::Result<ParsedDsn, DsnError> {
    let (user_part, rest) = match dsn.split_once('@') {
        Some((u, r)) => (Some(u), r),
        None => (None, dsn),
    };

    let rest = rest.strip_prefix("tcp(").ok_or(DsnError::MissingTcp)?;
    let (host_port, rest) = rest.split_once(')').ok_or(DsnError::MissingTcp)?;
    let rest = rest.strip_prefix('/').ok_or(DsnError::MissingDb)?;
    let db_name = rest.split('?').next().unwrap_or("").to_string();
    if db_name.is_empty() {
        return Err(DsnError::MissingDb);
    }

    let (host, port) = parse_host_port(host_port)?;
    let (user, password) = match user_part {
        Some(u) => match u.split_once(':') {
            Some((usr, pw)) => (usr.to_string(), pw.to_string()),
            None => (u.to_string(), String::new()),
        },
        None => (String::new(), String::new()),
    };

    Ok(ParsedDsn {
        user,
        password,
        host,
        port,
        db_name,
    })
}

/// Most rows read from a driver for one query. `row-count` is the number
/// of rows read, so it never exceeds this.
pub const MAX_SQL_ROWS: usize = 1000;

const DEFAULT_MYSQL_PORT: u16 = 3306;

fn parse_host_port(host_port: &str) -> std::result::Result<(String, u16), DsnError> {
    if let Some(after_bracket) = host_port.strip_prefix('[') {
        let (host, tail) = after_bracket.split_once(']').ok_or(DsnError::MissingTcp)?;
        let port = match tail.strip_prefix(':') {
            Some(p) if !p.is_empty() => p
                .parse()
                .map_err(|_| DsnError::InvalidPort(p.to_string()))?,
            _ => DEFAULT_MYSQL_PORT,
        };
        return Ok((host.to_string(), port));
    }

    match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() => {
            let port: u16 = p
                .parse()
                .map_err(|_| DsnError::InvalidPort(p.to_string()))?;
            Ok((h.to_string(), port))
        }
        _ => {
            let host = if host_port.is_empty() {
                "127.0.0.1".to_string()
            } else {
                host_port.to_string()
            };
            Ok((host, DEFAULT_MYSQL_PORT))
        }
    }
}

/// Errors executing a query through a [`SqlConn`]. The message never
/// repeats the query text, so a driver error can't leak SQL (or values it
/// might carry) into the reply.
#[derive(Debug, thiserror::Error)]
#[error("sql: query failed")]
pub struct SqlOpError;

/// One live connection (or pool) against a sql resource. Implementations
/// must run `query` inside a read-only transaction when `read_only` is
/// true, so the driver itself
/// refuses any write the readonly pre-check missed.
pub trait SqlConn: Send + Sync {
    /// Returns at most [`MAX_SQL_ROWS`] rows. Real drivers stop reading at
    /// the cap and release the rest of the result.
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [Value],
        read_only: bool,
    ) -> BoxFuture<'a, std::result::Result<Vec<Map<String, Value>>, SqlOpError>>;

    /// Closes this connection/pool, once per sql
    /// resource when the handler closes. Returns whether
    /// it closed cleanly rather than an error message: a driver's close
    /// error could in principle echo back DSN text, and this runner never
    /// logs anything but ids, kinds, reasons and timings.
    fn close<'a>(&'a self) -> BoxFuture<'a, bool>;
}

/// A [`SqlConn`] backed by a real `mysql_async` pool.
pub struct MysqlConn {
    pool: mysql_async::Pool,
}

impl SqlConn for MysqlConn {
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [Value],
        read_only: bool,
    ) -> BoxFuture<'a, std::result::Result<Vec<Map<String, Value>>, SqlOpError>> {
        Box::pin(async move {
            use mysql_async::prelude::Queryable;
            let sql_params: Vec<mysql_async::Value> =
                params.iter().map(json_to_sql_value).collect();
            let mut conn = self.pool.get_conn().await.map_err(|_| SqlOpError)?;

            if read_only {
                let mut opts = mysql_async::TxOpts::default();
                opts.with_readonly(true);
                let mut tx = conn.start_transaction(opts).await.map_err(|_| SqlOpError)?;
                let rows: Vec<mysql_async::Row> = tx
                    .exec(query, mysql_async::Params::Positional(sql_params))
                    .await
                    .map_err(|_| SqlOpError)?;
                let result = rows_to_maps(rows);
                tx.commit().await.map_err(|_| SqlOpError)?;
                Ok(result)
            } else {
                let rows: Vec<mysql_async::Row> = conn
                    .exec(query, mysql_async::Params::Positional(sql_params))
                    .await
                    .map_err(|_| SqlOpError)?;
                Ok(rows_to_maps(rows))
            }
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.pool.clone().disconnect().await.is_ok() })
    }
}

fn rows_to_maps(rows: Vec<mysql_async::Row>) -> Vec<Map<String, Value>> {
    rows.into_iter().map(row_to_map).collect()
}

fn row_to_map(row: mysql_async::Row) -> Map<String, Value> {
    let columns = row.columns();
    let types: Vec<mysql_async::consts::ColumnType> =
        columns.iter().map(|c| c.column_type()).collect();
    let names: Vec<String> = columns.iter().map(|c| c.name_str().into_owned()).collect();
    let values = row.unwrap();

    let mut out = Map::with_capacity(names.len());
    for ((name, value), col_type) in names.into_iter().zip(values).zip(types) {
        out.insert(name, sql_value_to_json(value, col_type));
    }
    out
}

/// Converts a JSON arg value into the `mysql_async::Value` bound as a query
/// parameter. Strings and JSON numbers are both sent as their text form:
/// This is deliberate: a numeric string parameter, not a native int/float
/// bind, so the server coerces by the column type.
fn json_to_sql_value(v: &Value) -> mysql_async::Value {
    match v {
        Value::Null => mysql_async::Value::NULL,
        Value::Bool(b) => mysql_async::Value::Int(i64::from(*b)),
        Value::Number(n) => mysql_async::Value::Bytes(n.to_string().into_bytes()),
        Value::String(s) => mysql_async::Value::Bytes(s.clone().into_bytes()),
        other => mysql_async::Value::Bytes(other.to_string().into_bytes()),
    }
}

/// Converts one returned column value into JSON, : `[]byte` (MySQL `TEXT`/`VARCHAR`/`DECIMAL`, and — with
/// the driver's default `parseTime=false`, which this DSN parser assumes —
/// every temporal type too) becomes a JSON string, `NULL` becomes `null`,
/// and integers/floats become JSON numbers. `mysql_async` always decodes
/// temporal columns into its structured `Date`/`Time` variants (the binary
/// protocol is always structured, so those two variants are formatted
/// back into text, keyed off the column's declared type. No golden test
/// pins a temporal column, so this formatting is best effort.
fn sql_value_to_json(v: mysql_async::Value, col_type: mysql_async::consts::ColumnType) -> Value {
    use mysql_async::consts::ColumnType::*;
    match v {
        mysql_async::Value::NULL => Value::Null,
        mysql_async::Value::Bytes(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
        mysql_async::Value::Int(i) => Value::Number(i.into()),
        mysql_async::Value::UInt(u) => Value::Number(u.into()),
        mysql_async::Value::Float(f) => number_or_null(f64::from(f)),
        mysql_async::Value::Double(d) => number_or_null(d),
        mysql_async::Value::Date(year, month, day, hour, minute, second, micro) => {
            let date_only = matches!(col_type, MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE);
            Value::String(format_mysql_date(
                year, month, day, hour, minute, second, micro, date_only,
            ))
        }
        mysql_async::Value::Time(is_negative, days, hour, minute, second, micro) => Value::String(
            format_mysql_time(is_negative, days, hour, minute, second, micro),
        ),
    }
}

fn number_or_null(f: f64) -> Value {
    serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
}

#[allow(clippy::too_many_arguments)]
fn format_mysql_date(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micro: u32,
    date_only: bool,
) -> String {
    if date_only {
        format!("{year:04}-{month:02}-{day:02}")
    } else if micro == 0 {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micro:06}")
    }
}

fn format_mysql_time(
    is_negative: bool,
    days: u32,
    hour: u8,
    minute: u8,
    second: u8,
    micro: u32,
) -> String {
    let total_hours = days * 24 + u32::from(hour);
    let sign = if is_negative { "-" } else { "" };
    if micro == 0 {
        format!("{sign}{total_hours:02}:{minute:02}:{second:02}")
    } else {
        format!("{sign}{total_hours:02}:{minute:02}:{second:02}.{micro:06}")
    }
}

/// The cap on connections per sql resource: at most 2 open, up to 2 kept
/// idle, no lifetime or idle-time limit. Used for both bounds of the
/// `mysql_async` pool (with `mysql_async`'s own default
/// `inactive_connection_ttl` of zero, the pool keeps exactly `min` idle
/// connections around and drops the rest immediately on release; nothing
/// ever proactively evicts a live connection).
const SQL_POOL_MAX_CONNS: usize = 2;

/// Builds the `mysql_async` connection options for a parsed DSN: no TLS,
/// with the driver's defaults for a DSN with no
/// `?params`, and a pool capped at [`SQL_POOL_MAX_CONNS`]. Split out from [`default_open_sql`] so a test can inspect the
/// resulting [`mysql_async::PoolOpts`] without opening a real connection
/// (`mysql_async::Pool::new` never dials eagerly, but building one still
/// wants a runtime; converting straight to `Opts` needs neither).
fn build_opts(parsed: &ParsedDsn) -> mysql_async::OptsBuilder {
    let constraints = mysql_async::PoolConstraints::new(SQL_POOL_MAX_CONNS, SQL_POOL_MAX_CONNS)
        .unwrap_or_default();
    let pool_opts = mysql_async::PoolOpts::default().with_constraints(constraints);
    mysql_async::OptsBuilder::default()
        .ip_or_hostname(parsed.host.clone())
        .tcp_port(parsed.port)
        .user(Some(parsed.user.clone()))
        .pass(Some(parsed.password.clone()))
        .db_name(Some(parsed.db_name.clone()))
        .pool_opts(pool_opts)
}

/// The default [`OpenSql`]: parses the DSN and opens (lazily) a `mysql_async` pool built by [`build_opts`].
pub(crate) fn default_open_sql() -> OpenSql {
    Arc::new(
        |dsn: &str| -> BoxFuture<'static, std::result::Result<Arc<dyn SqlConn>, SqlOpenError>> {
            let dsn = dsn.to_string();
            Box::pin(async move {
                if super::sql_pg::is_postgres_dsn(&dsn) {
                    let conn = super::sql_pg::open(&dsn)?;
                    return Ok(Arc::new(conn) as Arc<dyn SqlConn>);
                }
                let parsed = parse_dsn(&dsn).map_err(|e| SqlOpenError(e.to_string()))?;
                let pool = mysql_async::Pool::new(build_opts(&parsed));
                Ok(Arc::new(MysqlConn { pool }) as Arc<dyn SqlConn>)
            })
        },
    )
}

impl Handler {
    /// Implements the sql.query op kind.
    pub(crate) async fn sql_query(
        &self,
        op: &contract::Op,
    ) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        let resource = match self.resource(&op.op_id, &op.resource, "sql") {
            Ok(r) => r,
            Err(e) => return (None, Some(e)),
        };
        let conn = match self.sql_conns.get(&op.resource) {
            Some(c) => Arc::clone(c),
            None => {
                return (
                    None,
                    Some(new_error(
                        &op.op_id,
                        "unknown-resource",
                        &format!("unknown resource {:?}", op.resource),
                    )),
                );
            }
        };

        let (args, secrets) = match self.substitute_args(op, "sql.query", &[]) {
            Ok(v) => v,
            Err(e) => return (None, Some(e)),
        };
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if resource.read_only
            && let Err(e) = readonly::check(&query)
        {
            return (
                None,
                Some(new_error(&op.op_id, "read-only", &e.to_string())),
            );
        }
        let params: Vec<Value> = match args.get("params") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };

        let start = (self.now)();
        let rows = conn.query(&query, &params, resource.read_only).await;
        let exec_ms = duration_ms((self.now)(), start);

        match rows {
            Ok(mut rows) => {
                rows.truncate(MAX_SQL_ROWS);
                self.shape_sql_result(op, rows, &secrets, exec_ms)
            }
            Err(_) => (
                None,
                Some(new_error(&op.op_id, "runner-error", "query failed")),
            ),
        }
    }

    fn shape_sql_result(
        &self,
        op: &contract::Op,
        rows: Vec<Map<String, Value>>,
        secrets: &[String],
        exec_ms: i64,
    ) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        let mut secrets = secrets.to_vec();
        let mut source = Map::new();
        source.insert(
            "rows".to_string(),
            Value::Array(rows.iter().cloned().map(Value::Object).collect()),
        );
        let caps = match self.apply_capture(op, &source, &mut secrets) {
            Ok(c) => c,
            Err(e) => return (None, Some(e)),
        };
        let secrets = &secrets[..];
        self.evidence_put_sql(op, &rows, secrets);

        let cols = derive_columns(&op.project);
        let projected = if cols.is_empty() {
            Vec::new()
        } else {
            project::select_columns(&rows, &cols)
        };

        let mut payload = Map::new();
        payload.insert(
            "rows".to_string(),
            Value::Array(projected.into_iter().map(Value::Object).collect()),
        );
        payload.insert("row-count".to_string(), Value::from(rows.len() as i64));

        capture::mask(&mut payload, &caps);

        let (scrubbed, count) = match scrub_payload(&payload, secrets) {
            Ok(v) => v,
            Err(_) => {
                return (
                    None,
                    Some(new_error(
                        &op.op_id,
                        "runner-error",
                        "response encoding failed",
                    )),
                );
            }
        };
        (
            Some(ResultFrame {
                op_id: op.op_id.clone(),
                status: "pass".to_string(),
                payload: scrubbed,
                scrubbed: count,
                timing: Timing { exec_ms },
            }),
            None,
        )
    }

    fn evidence_put_sql(&self, op: &contract::Op, rows: &[Map<String, Value>], secrets: &[String]) {
        let body = Value::Array(rows.iter().cloned().map(Value::Object).collect());
        self.evidence.put(
            &op.run_id,
            crate::evidence::Entry {
                op_id: op.op_id.clone(),
                status: 0,
                body,
                secrets: secrets.to_vec(),
            },
            (self.now)(),
        );
    }
}

/// Picks the column names a projection touches: for a path under "rows",
/// the first segment after the row index that isn't itself a digit;
/// "row-count" contributes nothing; any other path contributes its first
/// segment. Order is preserved and duplicates dropped.
fn derive_columns(paths: &[Vec<String>]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut cols = Vec::new();
    for p in paths {
        if let Some(col) = column_for(p)
            && seen.insert(col.clone())
        {
            cols.push(col);
        }
    }
    cols
}

fn column_for(p: &[String]) -> Option<String> {
    if p.is_empty() {
        return None;
    }
    match p[0].as_str() {
        "rows" => p[1..].iter().find(|seg| !is_all_digits(seg)).cloned(),
        "row-count" => None,
        _ => Some(p[0].clone()),
    }
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
#[path = "sql_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "sql_mysql_tests.rs"]
mod mysql_tests;
