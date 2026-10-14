# Changelog

## simplify D3, 2026-10-14
- README: the runner key comes from `zz runners init` or the page `/app/runners`.

## simplify R2, 2026-10-14
- release workflow: new job `binary` builds static Linux binaries (`x86_64` and `aarch64`, musl, `cargo-zigbuild`) and attaches `zriz-runner-<version>-<arch>-linux` and `SHA256SUMS` to the GitHub release (created if missing). Nothing is published on a dry run.
- new `install.sh`: POSIX sh, no question, no stdin; downloads the latest release (or `ZRIZ_RUNNER_VERSION`), fails closed on a wrong or missing SHA-256 sum, installs to `/usr/local/bin/zriz-runner` (or `ZRIZ_RUNNER_BIN_DIR`). Test: `tests/install_sh_test.rs` (offline, `ZRIZ_RUNNER_BASE_URL=file://`).
- new `ops/zriz-runner.service`, an example systemd unit (`DynamicUser=yes`, hardened, no port).
- README: the binary shape first, then Docker.

## simplify R1b, 2026-10-14
- `embed::Config` has `lookup: Option<ops::Lookup>` (secret reader; `None` = process env) and `runner_id: Option<String>` (`None` = hostname); `embed::exchange_config` takes the runner id. The binary passes `None`.
- `NOTICE` file added.

## simplify R1, 2026-10-14
- renamed: `ZRIZ_RUNNER_TOKEN` is `ZRIZ_RUNNER_KEY`; config `cloud.token-env` is `cloud.key-env` (`config::Cloud::key_env`); `exchange::Config::token`/`token_source`/`TokenSource` are `key`/`key_source`/`KeySource`; `exchange/token.rs` is `key.rs`. No old name stays.
- new public module `embed`: `start(Config { runner, key }, on_connected) -> Result<Handle, ops::NewError>` and `Handle::stop()`. The key is a value, not a process variable. No worker, no port.
- vendored contract synced (reasons v2, pipeline inline call)

## time budget, 2026-10-11
- `timing.target-ms` (optional integer, `Timing::target_ms`) in the result frame; vendored `contract/result.json` has the key
- sql.query (MySQL and Postgres): `SqlConn::query` returns `SqlReply { rows, target_ms }`; the number is the round trip of the statement and the row read, without the wait for a place, the connection, BEGIN/COMMIT and the prepare (both drivers prepare first, then time only the execute; Postgres also turns rows into maps after the span); a query that fails at prepare has no `target-ms`; `exec-ms` is unchanged
- http.request: `timing.target-ms` = send to end of the body read, summed over redirect hops, without the connect (DNS, TCP, TLS); one measured `reqwest::Client` for each op in work (`ops/http_client.rs`, per-resource idle set, connector layer notes each new connection's addresses and connect time); the op is charged only for the connect of a connection one of its own responses came on (`HttpInfo` in the response extensions), so a connect that hyper-util races in the background no longer lowers `target-ms`; `exec-ms` is unchanged
- new crates (direct): `hyper-util` (feature `client-legacy` only) and `http`, both already in the tree through reqwest
- `ops/target.rs`: `worker_target_ms` (moved from `browser.rs`) and `Span`, a sum of timed waits; `Handler::new` no longer builds a throw-away client; `NewError::HttpClient` removed
- browser.page and cli.exec: the worker measures `target-ms` and the runner passes it on (`timing.target-ms`, `exec-ms` unchanged). `worker-response.json`: the ok answer has an optional top-level integer `target-ms` (>= 0, not in `out`); protocol `v` stays 1 (an optional key; an old runner or worker just has none). Browser: first command start to last (or failed) command end, without the queue wait, sweep, context/browser start, settle, title, cookies. CLI by mode: run spawn to end (timeout: to the end of the stop); start spawn to marker (no `until`: spawn only); read start of wait to marker (no `until`: 0); wait start of wait to end or timeout; stop signal to end. Both hosts take an injectable monotonic `mono` (`performance.now`); refusals have no `target-ms`
