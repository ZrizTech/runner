# zriz-runner
Runs inside the customer's environment, executes ops the cloud sends, holds
every secret. Public, Apache 2 (LICENSE). Never imports from the cloud.

## Modules (crate `zriz-runner`, lib + bin `zriz-runner`, no internal/)
- `main` (`src/main.rs`) — bin: config from
  `ZRIZ_RUNNER_CONFIG`; `run()` takes ctx (`CancellationToken`)/lookup/stderr as inputs, all exit
  paths covered by `main_tests.rs`; SIGINT (ctrl-c) or SIGTERM cancels cleanly.
- `config` — JSON config, `${NAME}` from process env; missing name errors naming the key.
- `contract` (+ `contract/frame.rs`, `contract/schema.rs`) — frame types, embedded JSON schemas
  vendored in `contract/` (fixtures in `contract/fixtures/frames/`); `tests/contract_test.rs` validates them.
- `placeholder` — `${NAME}` substitution into allowed slots; collects secrets for scrubbing. Every op's scrub list also holds every value named in any resource's `secrets` (resolved once in `Handler::new`, `listed_secrets`) and the run's vault.
- `project` — select-paths, select-columns (capped at 100 rows).
- `scrub` — replace secret values, and their URL- and base64-encoded forms, with `[scrubbed]`;
  longest form first, deduped across forms and secrets.
- `origin` — resolves an op's request path against a resource's base URL via `url::Url`;
  scheme/host/port must match or it's host-not-allowed; redirect targets are policed the same way.
- `readonly` — SQL pre-check; treats `/*!` as code, not comment; write-keyword list includes
  INTO/OUTFILE/DUMPFILE/LOAD/HANDLER/LOCK/CALL/SET/RENAME (fail closed).
- `evidence` — bounded TTL store, 120s, 1000 entries max, per run-id.
- `ops` — `mod.rs` (dispatch, closed arg keys), `http.rs` (+ `jar.rs` cookie jar per run and
  resource, response headers projected, `set-cookie` values masked, `redirect: none`), `sql.rs` +
  `sql_pg.rs` (MySQL, or Postgres by DSN scheme; `?`→`$n`; read-only resources in `BEGIN READ
  ONLY`), `capture.rs` + `vault.rs` (sealed capture: values stay here, `[captured]` in payloads,
  `${NAME}` resolved from the run's vault first), `browser.rs` / `cli.rs` + `worker.rs` (ops sent
  to the Node worker sidecar over a unix socket; `argshape.rs` checks cli argv; `${NAME}` only in
  `fill` values / cli `env`, names from the resource `secrets` or the vault), `announce.rs` (kinds
  and browser/cli resources announced only while the worker answers ping), `subst.rs`, `evidence.rs`.
- `worker/` + `worker-contract/` — the Node sidecar (Playwright 1.63.0 Chromium, cli spawn without
  a shell) and its closed, versioned protocol; it holds no runner secret. `npm test` there.
- http.request's own redirect loop checks `origin::check_origin` on every hop (the client is
  always built with `redirect::Policy::none()`); sql.query goes through the `SqlConn` trait (tests
  use a fake connection).
- `exchange` (`mod.rs`, `frames.rs`, `token.rs`, `test_support.rs`, `conformance_tests.rs`) —
  long-poll loop, bounded pool, extra exchange on op completion, `ExchangeError::Unauthorized` on
  a second consecutive 401, never `process::exit`; `Config::on_connected`, if set, fires exactly
  once, after the first successful exchange; backoff doubles from 1s to a default max of 10s;
  `conformance_tests.rs` drives the runner against a fake cloud with the real slot semantics
  (immediate first exchange, hold, drain by max-inflight, late frame dropped, restart with a
  re-issued op, a 20s outage) and pins exactly-once answers, bounded backoff and one
  `on_connected`.

Other crates import `config`, `exchange`, `ops` as a library. Keep those
APIs stable.

## Rules
`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean. Workspace lints
(`Cargo.toml`): `unsafe_code = "forbid"`; clippy `unwrap_used`/`expect_used`/`panic`/`dbg_macro`/
`print_stdout`/`print_stderr` all `"deny"` outside tests (`clippy.toml`:
`allow-{unwrap,expect,panic,dbg,print}-in-tests = true`). `thiserror` errors in the library,
`anyhow` never needed here since `main` owns its own exit-code plumbing. `tracing` only: op-id,
run-id, step-index, kind, resource, reason, timing, status code — never args, bodies, rows,
tokens, URLs with creds. No global mutable state, no package-level init. File <= 400 lines, fn
<= 60 as a guide. Table-driven tests. Allowed crates: tokio, serde, serde_json, thiserror, url,
base64, jsonschema, reqwest (rustls, no default features), mysql_async (rustls), tokio-postgres +
tokio-postgres-rustls + rustls (ring) + webpki-roots + chrono (no default features; Postgres
driver), flate2, futures, tracing, tracing-subscriber, uuid, tokio-util; test only: tempfile,
wiremock.

## Logging
One line format for all products (log format v3): `src/logfmt/` (pure `format_line`, tracing
adapter, panic hook), stdout, `ZRIZ_LOG` (default `warn,runner=info`; invalid spec falls back to it with a WARN;
`log`-crate records bridged in). Own events use explicit targets
`runner.*`, values with `%` or primitives (never `?`), keys from `contract/log/lists.json`
(`tests/log_format_test.rs` greps src/ for it). The worker has its own `worker/src/logfmt.js`.
Build id `<semver>+<sha>` (`+dev` locally; `build.rs`, `ZRIZ_BUILD_SHA` build arg): logged as `build=` on `started`,
sent as `User-Agent: runner/<build id>` on exchange requests only (never on ops calls to customer systems).
The op frame's `trace-id` goes into the op's span and the worker request.

## Contract
`contract/` is vendored from the upstream contract repo: schemas (`*.json`), `fixtures/frames/`, `log/`. Do not edit by hand.
`make sync-contract` (maintainers; `CONTRACT_DIR` defaults to `../zriz-contract`) copies all three; the drift check lives upstream. Then `cargo test` must pass.

## Verify
`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && docker build -t zriz-runner:dev .`
and `cd worker && npm test` (worker; `docker build -f worker/Dockerfile .` from the repo root).
`examples/config.json` is loaded by a config test; keep it valid.
