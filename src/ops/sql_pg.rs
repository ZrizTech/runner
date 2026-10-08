//! Postgres driver for the sql.query op kind, chosen by the DSN scheme
//! (`postgres://` or `postgresql://`); every other DSN goes to MySQL.
//!
//! Crate: `tokio-postgres` + `tokio-postgres-rustls` (rustls, no OpenSSL).
//! It is much lighter than `sqlx` here: no macro/any-driver layer, and the
//! pool this runner needs (max 2) is a semaphore plus an idle list.
//!
//! Params: the cloud always sends `?`. They are rewritten to `$1..$n`
//! outside single quotes, double quotes, `--` and `/* */` comments. Each
//! JSON value is bound by the type Postgres infers for its slot: ints and
//! floats accept a number or a numeric string, text types accept any scalar
//! as its text form, bool accepts a bool / `true` / `false` / 0 / 1, json
//! and jsonb take the value as is, uuid a string, date and timestamps an
//! RFC 3339 / `YYYY-MM-DD` string. Any other slot type fails the query.
//!
//! Rows to JSON (column name to value):
//! - int2/int4/int8, oid: number. float4/float8: number (NaN/inf: null).
//! - numeric: string (like MySQL DECIMAL).
//! - bool: JSON bool.
//! - text, varchar, bpchar, name, enum: string.
//! - json, jsonb: the parsed JSON value.
//! - uuid: string. bytea: `\x` + hex string.
//! - timestamptz: RFC 3339 in UTC (`2026-01-02T03:04:05Z`, fraction only if
//!   non-zero). timestamp (no zone): same, read as UTC. date: `YYYY-MM-DD`.
//! - null: null. Any other type: null.
//!
//! Errors never carry text: a failed query is a bare [`SqlOpError`].

use super::sql::SqlOpError;
use super::{BoxFuture, SqlOpenError};
use futures::StreamExt;
use serde_json::{Map, Value};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_postgres::types::{FromSql, Kind, ToSql, Type, accepts};
use tokio_postgres::{Client, Config, config::SslMode};

const POOL_MAX: usize = 2;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STATEMENT_TIMEOUT_MS: u64 = 30_000;

pub(crate) fn is_postgres_dsn(dsn: &str) -> bool {
    dsn.starts_with("postgres://") || dsn.starts_with("postgresql://")
}

pub(crate) struct PgConn {
    config: Config,
    tls: tokio_postgres_rustls::MakeRustlsConnect,
    idle: Mutex<Vec<Client>>,
    permits: Semaphore,
}

/// Parses the URL and builds the (lazy) pool. The error text is fixed: it
/// never repeats any part of the DSN.
pub(crate) fn open(dsn: &str) -> Result<PgConn, SqlOpenError> {
    let mut config: Config = dsn
        .parse()
        .map_err(|_| SqlOpenError("dsn: invalid postgres url".to_string()))?;
    config.connect_timeout(CONNECT_TIMEOUT);
    config.options(format!("-c statement_timeout={STATEMENT_TIMEOUT_MS}"));
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls_config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| SqlOpenError("tls: setup failed".to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(PgConn {
        config,
        tls: tokio_postgres_rustls::MakeRustlsConnect::new(tls_config),
        idle: Mutex::new(Vec::new()),
        permits: Semaphore::new(POOL_MAX),
    })
}

impl PgConn {
    async fn checkout(&self) -> Result<Client, SqlOpError> {
        loop {
            let cached = self.idle.lock().map_err(|_| SqlOpError)?.pop();
            match cached {
                Some(c) if !c.is_closed() => return Ok(c),
                Some(_) => continue,
                None => break,
            }
        }
        let (client, connection) = if self.config.get_ssl_mode() == SslMode::Disable {
            let (c, conn) = self
                .config
                .connect(tokio_postgres::NoTls)
                .await
                .map_err(|_| SqlOpError)?;
            (c, tokio::spawn(async move { conn.await.ok() }))
        } else {
            let (c, conn) = self
                .config
                .connect(self.tls.clone())
                .await
                .map_err(|_| SqlOpError)?;
            (c, tokio::spawn(async move { conn.await.ok() }))
        };
        drop(connection);
        Ok(client)
    }

    async fn run(
        &self,
        client: &mut Client,
        query: &str,
        params: &[Value],
        read_only: bool,
    ) -> Result<Vec<Map<String, Value>>, SqlOpError> {
        let sql = rewrite_placeholders(query);
        if read_only {
            let tx = client
                .build_transaction()
                .read_only(true)
                .start()
                .await
                .map_err(|_| SqlOpError)?;
            let rows = exec(&tx, &sql, params).await?;
            tx.commit().await.map_err(|_| SqlOpError)?;
            Ok(rows)
        } else {
            exec(&*client, &sql, params).await
        }
    }
}

async fn exec(
    c: &impl tokio_postgres::GenericClient,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Map<String, Value>>, SqlOpError> {
    let stmt = c.prepare(sql).await.map_err(|_| SqlOpError)?;
    if stmt.params().len() != params.len() {
        return Err(SqlOpError);
    }
    let bound: Vec<Box<dyn ToSql + Sync + Send>> = stmt
        .params()
        .iter()
        .zip(params)
        .map(|(t, v)| to_param(t, v))
        .collect::<Result<_, _>>()?;
    let refs: Vec<&(dyn ToSql + Sync)> = bound
        .iter()
        .map(|b| b.as_ref() as &(dyn ToSql + Sync))
        .collect();
    let stream = c
        .query_raw(&stmt, refs.iter().copied())
        .await
        .map_err(|_| SqlOpError)?;
    futures::pin_mut!(stream);
    let mut out = Vec::new();
    while out.len() < super::sql::MAX_SQL_ROWS {
        match stream.next().await {
            Some(Ok(row)) => out.push(row_to_map(&row)),
            Some(Err(_)) => return Err(SqlOpError),
            None => break,
        }
    }
    Ok(out)
}

impl super::sql::SqlConn for PgConn {
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [Value],
        read_only: bool,
    ) -> BoxFuture<'a, Result<Vec<Map<String, Value>>, SqlOpError>> {
        Box::pin(async move {
            let _permit = self.permits.acquire().await.map_err(|_| SqlOpError)?;
            let mut client = self.checkout().await?;
            let out = self.run(&mut client, query, params, read_only).await;
            if out.is_ok()
                && !client.is_closed()
                && let Ok(mut idle) = self.idle.lock()
            {
                idle.push(client);
            }
            out
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.permits.close();
            match self.idle.lock() {
                Ok(mut idle) => idle.clear(),
                Err(_) => return false,
            }
            true
        })
    }
}

/// Rewrites every `?` outside quotes and comments to `$1..$n`.
pub(crate) fn rewrite_placeholders(q: &str) -> String {
    let b = q.as_bytes();
    let mut out = String::with_capacity(q.len() + 8);
    let mut n = 0;
    let mut i = 0;
    let mut start = 0; // start of pending verbatim run
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let quote = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == quote {
                        // a doubled quote is an escaped quote: stay inside
                        if b.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b'?' => {
                out.push_str(&q[start..i]);
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    let end = q.len();
    out.push_str(&q[start.min(end)..end]);
    out
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn to_param(t: &Type, v: &Value) -> Result<Box<dyn ToSql + Sync + Send>, SqlOpError> {
    if v.is_null() {
        return Ok(Box::new(Option::<String>::None));
    }
    let bad = || SqlOpError;
    let txt = scalar_text(v);
    Ok(match *t {
        Type::INT2 => Box::new(
            txt.ok_or_else(bad)?
                .trim()
                .parse::<i16>()
                .map_err(|_| bad())?,
        ),
        Type::INT4 => Box::new(
            txt.ok_or_else(bad)?
                .trim()
                .parse::<i32>()
                .map_err(|_| bad())?,
        ),
        Type::INT8 => Box::new(
            txt.ok_or_else(bad)?
                .trim()
                .parse::<i64>()
                .map_err(|_| bad())?,
        ),
        Type::FLOAT4 => Box::new(
            txt.ok_or_else(bad)?
                .trim()
                .parse::<f32>()
                .map_err(|_| bad())?,
        ),
        Type::FLOAT8 => Box::new(
            txt.ok_or_else(bad)?
                .trim()
                .parse::<f64>()
                .map_err(|_| bad())?,
        ),
        Type::BOOL => Box::new(match txt.as_deref() {
            Some("true" | "1") => true,
            Some("false" | "0") => false,
            _ => return Err(bad()),
        }),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => Box::new(txt.ok_or_else(bad)?),
        Type::JSON | Type::JSONB => Box::new(v.clone()),
        Type::UUID => Box::new(
            txt.ok_or_else(bad)?
                .parse::<uuid::Uuid>()
                .map_err(|_| bad())?,
        ),
        Type::DATE => Box::new(
            chrono::NaiveDate::parse_from_str(&txt.ok_or_else(bad)?, "%Y-%m-%d")
                .map_err(|_| bad())?,
        ),
        Type::TIMESTAMPTZ => Box::new(
            chrono::DateTime::parse_from_rfc3339(&txt.ok_or_else(bad)?)
                .map_err(|_| bad())?
                .with_timezone(&chrono::Utc),
        ),
        Type::TIMESTAMP => Box::new(
            chrono::DateTime::parse_from_rfc3339(&txt.ok_or_else(bad)?)
                .map_err(|_| bad())?
                .naive_utc(),
        ),
        _ => return Err(bad()),
    })
}

/// Numeric read as its decimal string.
struct Numeric(String);

impl<'a> FromSql<'a> for Numeric {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let rd = |i: usize| -> Result<u16, Box<dyn std::error::Error + Sync + Send>> {
            raw.get(i..i + 2)
                .map(|s| u16::from_be_bytes([s[0], s[1]]))
                .ok_or_else(|| "short numeric".into())
        };
        let ndigits = rd(0)? as usize;
        let weight = rd(2)? as i16;
        let sign = rd(4)?;
        let dscale = rd(6)? as usize;
        if sign == 0xC000 {
            return Ok(Numeric("NaN".to_string()));
        }
        let digit = |k: usize| rd(8 + 2 * k);
        let mut int_part = String::new();
        let mut frac_part = String::new();
        for k in 0..ndigits {
            let d = digit(k)?;
            let pos = weight - k as i16; // >=0: integer group
            if pos >= 0 {
                if int_part.is_empty() {
                    int_part.push_str(&d.to_string());
                } else {
                    int_part.push_str(&format!("{d:04}"));
                }
            } else {
                frac_part.push_str(&format!("{d:04}"));
            }
        }
        // integer groups skipped at the tail (trailing zero groups)
        if ndigits > 0 && weight >= 0 {
            let present = (ndigits as i16).min(weight + 1);
            for _ in present..=weight {
                int_part.push_str("0000");
            }
        }
        // leading zero groups in the fraction (weight < -1)
        if weight < -1 {
            let pad = "0000".repeat((-weight - 1) as usize);
            frac_part = format!("{pad}{frac_part}");
        }
        if int_part.is_empty() {
            int_part.push('0');
        }
        while frac_part.len() < dscale {
            frac_part.push('0');
        }
        frac_part.truncate(dscale);
        let mut s = String::new();
        if sign == 0x4000 {
            s.push('-');
        }
        s.push_str(&int_part);
        if dscale > 0 {
            s.push('.');
            s.push_str(&frac_part);
        }
        Ok(Numeric(s))
    }
    accepts!(NUMERIC);
}

/// Text-encoded custom types (enums) read as a string.
struct EnumText(String);

impl<'a> FromSql<'a> for EnumText {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(EnumText(std::str::from_utf8(raw)?.to_string()))
    }
    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Enum(_))
    }
}

fn row_to_map(row: &tokio_postgres::Row) -> Map<String, Value> {
    let mut out = Map::with_capacity(row.columns().len());
    for (i, col) in row.columns().iter().enumerate() {
        out.insert(col.name().to_string(), cell(row, i, col.type_()));
    }
    out
}

fn cell(row: &tokio_postgres::Row, i: usize, t: &Type) -> Value {
    fn num(f: f64) -> Value {
        serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
    fn ts<Tz: chrono::TimeZone>(d: chrono::DateTime<Tz>) -> Value {
        Value::String(
            d.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        )
    }
    macro_rules! get {
        ($ty:ty) => {
            row.try_get::<_, Option<$ty>>(i).ok().flatten()
        };
    }
    match *t {
        Type::INT2 => get!(i16).map_or(Value::Null, |v| Value::from(i64::from(v))),
        Type::INT4 => get!(i32).map_or(Value::Null, |v| Value::from(i64::from(v))),
        Type::INT8 => get!(i64).map_or(Value::Null, Value::from),
        Type::OID => get!(u32).map_or(Value::Null, |v| Value::from(u64::from(v))),
        Type::FLOAT4 => get!(f32).map_or(Value::Null, |v| num(f64::from(v))),
        Type::FLOAT8 => get!(f64).map_or(Value::Null, num),
        Type::BOOL => get!(bool).map_or(Value::Null, Value::Bool),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
            get!(String).map_or(Value::Null, Value::String)
        }
        Type::JSON | Type::JSONB => get!(Value).unwrap_or(Value::Null),
        Type::UUID => get!(uuid::Uuid).map_or(Value::Null, |u| Value::String(u.to_string())),
        Type::NUMERIC => get!(Numeric).map_or(Value::Null, |n| Value::String(n.0)),
        Type::BYTEA => get!(Vec<u8>).map_or(Value::Null, |b| {
            let mut s = String::from("\\x");
            for x in b {
                s.push_str(&format!("{x:02x}"));
            }
            Value::String(s)
        }),
        Type::TIMESTAMPTZ => get!(chrono::DateTime<chrono::Utc>).map_or(Value::Null, ts),
        Type::TIMESTAMP => get!(chrono::NaiveDateTime).map_or(Value::Null, |d| ts(d.and_utc())),
        Type::DATE => get!(chrono::NaiveDate).map_or(Value::Null, |d| {
            Value::String(d.format("%Y-%m-%d").to_string())
        }),
        _ if matches!(t.kind(), Kind::Enum(_)) => {
            get!(EnumText).map_or(Value::Null, |e| Value::String(e.0))
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
#[path = "sql_pg_tests.rs"]
mod tests;
