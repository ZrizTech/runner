# Changelog

## time budget, 2026-10-11
- `timing.target-ms` (optional integer, `Timing::target_ms`) in the result frame; vendored `contract/result.json` has the key
- sql.query (MySQL and Postgres): `SqlConn::query` returns `SqlReply { rows, target_ms }`; the number is the round trip of the statement and the row read, without the wait for a place, the connection, BEGIN/COMMIT and the Postgres prepare; MySQL `exec` prepares and executes in one call, so its prepare is inside; `exec-ms` is unchanged
- http.request: `timing.target-ms` = send to end of the body read, summed over redirect hops, without the connect (DNS, TCP, TLS); one measured `reqwest::Client` for each op in work (`ops/http_client.rs`, per-resource idle set, connector layer counts connect time); `exec-ms` is unchanged
