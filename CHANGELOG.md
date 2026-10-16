# Changelog

## nearhand R1 (unreleased): refused key, config error, cloud down
- `key-env` only (no `token-env` left in code, examples or ops files). Images: `ops/docker-compose.yml` runner is `restart: on-failure:3`; the unit has `RestartPreventExitStatus=2 3`. Rebuild no image: the compose and unit files are copied to the server; the binary changed, so a new runner release is needed for the exit code 3.
- Refused key (401/403): the runner reads `code` of the API error body (`key-revoked`, `key-unknown`, `key-expired`, `key-missing`, `no-right`), writes ONE `refused` line with cause and fix (`zz runners init --env <env>`, restart command), and exits 3. It no longer waits idle (that looked healthy to systemd and Docker). Docker's `on-failure` restarts on any non-zero code, so we chose `on-failure:3` (documented in the compose file and README).
- Config error: exit 2 (as before); the line names the file or the field and the fix (`config error in <path>: ...`, `ZRIZ_RUNNER_KEY ... is not set; put it in runner.env`). No `stopped` line after it.
- Cloud down: the first failed poll logs one WARN `poll failed`; later ones at most one a minute; the first good poll after logs one INFO `reconnected` with `elapsed_ms` of the outage.
- `api-error` schema registered; `contract/log/lists.json` synced (`key_id`), plus a local entry `reconnected` (runner.exchange, INFO, keys runner, elapsed_ms) that the contract repo must carry.

## 0.4.0 (2026-10-16): group-commit sender
- exchange: one result request in flight at a time; results that finish meanwhile go together in the next one (no wire change). Idle: a result goes at once.
- exchange: the sender frees its gate under the queue lock (a result queued as the loop ends is no longer left); the main poll never carries results (`reject` starts the sender); the gate is freed if the sender task panics; `contract/log/lists.json` synced (levels, `write skipped`).

## 0.3.0 (unreleased): round-10 fix (s-fix12), 2026-10-14
- `zrunner`: `wait_connected` reads the logs once into a variable and greps it (no `grep -q` in a pipe under `pipefail`). `rollback` exits 1 when the cloud refused the key.

## 0.3.0 (unreleased): round-9 fix (s-fix11), 2026-10-14
- `zrunner`: the `refused` and `runner connected` checks match the padded log line (`runner\.main +refused`). `wait_connected` returns 1 for a timeout and 2 for a refused key; `deploy` exits 1 on a refused key. `restart` now waits and says connected or refused.
- ops README: `deploy` runs `up -d --force-recreate`.

## 0.3.0 (unreleased): round-8 fix (s-fix10), 2026-10-14
- `zrunner restart` makes the container again (`up -d --force-recreate runner`), so a new key or secret in `.env` is read. Help and ops README say so.
- `zrunner deploy`: `wait_connected` says the cloud refused the key and returns 1 when the log has the `refused` line.
- README: "Docker still shows the container Up".

## 0.3.0 (unreleased): round-7 fix (s-fix9), 2026-10-14
- After a new key, make the container again (`docker rm -f` and `docker run`, or `docker compose up -d --force-recreate runner`); `docker restart` keeps the old key. README, compose comment and the free text of the `refused` log line say so. systemd: restart reads `runner.env` again.
- README: exit 2 is a bad config only; a refused key waits, and a stop exits 0. Doc fixes in `exchange::run` and `wait_refused`.

## 0.3.0 (unreleased): round-6 fix (s-fix8), 2026-10-14
- Contract synced: log event `refused` on `runner.main` (the log-format test was red).
- After a refusal (401/403), a stop (TERM or INT) now exits 0, a normal stop; systemd does not mark the unit failed. Exit 2 stays for a bad config at start. Test, unit comment and README updated; README says Docker and systemd still show the service as running while it waits, and where to see it.

## 0.3.0 (unreleased): round-5 fix (s-fix7), 2026-10-14
- `ops/docker-compose.yml`: `env_file` with `path` and `format: raw` (Docker Compose 2.30 or later), so `$` and ` #` in a secret stay as they are. The README says so.

## 0.3.0 (unreleased): round-5 fix (s-fix6), 2026-10-14
- The binary, on a refused key (401) or no right (403): writes one error line (no key), then waits idle (no more requests) until TERM or INT, then exits 2. The embed API is unchanged (the loop still ends at once).
- `ops/docker-compose.yml` and the README `docker run`: back to `restart: unless-stopped` (`on-failure:3` counted crashes over the whole life of the container). The reboot notes are removed. Docs: a refused key stops the requests; revoke it, make a new key, restart the runner.
- Unit: `RestartPreventExitStatus=2` stays for a bad config at start; the comment says so.
- Docs: README names `install.sh` at the repo root; CLAUDE.md: `Forbidden` comes on the first 403.

## 0.3.0 (unreleased): round-4 fixes (s-fix5), 2026-10-14
- `ops/docker-compose.yml` and the README `docker run`: `restart: on-failure:3` (was `unless-stopped`). A runner whose key is refused exits 2 and must not restart for ever (the cloud can ban the IP). A comment and the README say: after a host reboot, start the container again or use the systemd unit (Docker may not start an `on-failure` container after a reboot).
- Release workflow: the draft message says "Re-run failed jobs" (not "run again").
- README and unit comment: exit 2 = key refused (401), no right (403), or bad config. Doc of `exchange::run`: a 401 and a 403 end the loop.

## 0.3.0 (unreleased): round-3 low fixes (s-fix3), 2026-10-14
- `exchange::ExchangeError::Forbidden` (new variant): a 403 on the exchange ends the loop like a 401 (no retry); the binary exits 2 for it. Additive for callers that use `_`; a `match` without `_` must add the arm.
- `embed::Handle` cancels its loop when dropped (`stop()` stays).
- `install.sh`: temp file by `mktemp` in the target folder; a failed download says to check the version (not "use root").
- release workflow: a draft that already has files is not completed (delete it and run again); SHA256SUMS is uploaded last; `--latest` only for the highest stable tag; same Rust toolchain step as CI; `publish` needs `binary` and `image`.
- Contract copy synced (`canceled` text, `runner-unavailable` doc).

## 0.3.0 (unreleased): round-2 low fixes (s-fix2), 2026-10-14
- README: pinned install is `sudo ZRIZ_RUNNER_VERSION=... sh install.sh`; the `runner.env` value rule is stated without the false claim (unit comment too).
- `embed`: the key guard is `guarded_lookup`, with a test; test that `Handle::stop` returns `Panicked`. Docs of `ExchangeError` and CLAUDE.md fixed.
- release workflow: the release is a draft until all files are uploaded, then published; a re-run completes a draft (never replaces a file).
- Contract copy synced (`time-budget.ops` has no `count*`).

## 0.3.0 (unreleased): simplify review fixes (s-fix1), 2026-10-14
- `config::check(&Config, insecure_ok)` is the one config check (cloud URL scheme, resources, key-name guard); `config::load` and `embed::start` both call it.
- `embed::start` returns `embed::StartError` (`Config` or `Handler`) and no longer accepts a config that `config::load` would refuse. It never lets a lookup return the runner key as a secret value.
- `Handle::stop` maps a panic of the exchange task to the new `ExchangeError::Panicked` (was `Ok`).
- `exchange::Config::token` is now `key` (R1 missed it); inner lock and docs say key.
- `install.sh`: temp file in the target folder, atomic `mv`, trap cleanup, `curl --proto '=https' --tlsv1.2 --retry 3` (`file://` base allowed only for tests).
- `ops/zriz-runner.service`: `RestartPreventExitStatus=2` (key refused), `RestartSec=10`. README: `config.json` mode 644 (no secret in it), `runner.env` mode 600 with a plain `NAME=value` format.
- release workflow: binary build with `contents: read` and no stored token; separate `publish` job with `contents: write`; same on-main and new-version checks as the image job; no `--clobber`.
- `tests/install_sh_test.rs` removes its temp folders.

## simplify clean-up, 2026-10-14
- No more start WARN `no allowlist` for an http resource without `secrets`: `secrets` is optional (nothing to scrub), and the origin check always applies.
- Contract copy synced.

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
