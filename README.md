# zriz runner

**Code is cheap. Correctness is not.**

Agents now write code faster than anyone can read it. [zriz](https://zriz.io) proves your product still works, end to end, before your customers find out. The runner is the part that lives in your network: it makes the calls and holds every secret, so none of them reach the cloud. Docs: [zriz.io/docs/runner](https://zriz.io/docs/runner).

## What it is, and what it trusts

The runner polls the zriz cloud for ops (http, sql, browser, cli), runs them against your systems, and sends back the results.

- It only makes outbound connections. The runner opens no port. (The worker listens on a unix socket only.)
- It runs only inside its allowlist: every URL is checked against the resource's origin (redirects too), SQL is read-only where you declare it, op arguments have closed key sets, and `${NAME}` placeholders work only in the allowed slots.
- Secrets live in the runner's environment. The runner sends the cloud no secret, no connection string and no database driver.
- Results are cut down to the fields the op asks for. Then every value named in any resource's `secrets` list, and every value the run captured, is scrubbed from the result before it leaves. The cloud redacts again on write.

Browser and cli ops run in a separate Node sidecar, the worker (`worker/`). It keeps no secret: it has no runner environment and no config. It sees a secret value only inside the op that uses it, and the runner scrubs the result.

## Trust model

Where things sit:

```
       YOUR NETWORK                          |        ZRIZ CLOUD
                                             |
 +-------------+  calls  +---------------+   |
 | HTTP API    |<--------|    runner     |---+--> results: projected,
 | database    |         |  every secret |   |    scrubbed
 +-------------+         |  (its env)    |<--+--- ops: ${NAME} slots,
                         +-------+-------+   |    never values
                                 | unix      |
                                 | socket    |    The cloud holds no
 +-------------+  calls  +-------+-------+   |    secret, no driver,
 | browser site|<--------|    worker     |   |    no connection string.
 | cli tools   |         | no secret kept|   |
 +-------------+         +---------------+   |
                                             |
 Only link out: runner -> cloud, HTTPS long-poll. No port is open.
```

The path of one op through the runner. A failed check ends the op with an error. Nothing later runs.

```
 op from the cloud
   |
   v
 1 closed arg keys     a key not in the kind's list: unknown-arg
 2 placeholder slots   ${NAME} only in allowed slots, only listed names
 3 check by kind       http: origin check, again on every redirect hop
                       sql:  read-only pre-check, if the resource says so
                       cli:  command and argument shape
                       browser: the worker blocks off-origin requests
 4 execute             read-only sql runs in a read-only transaction
 5 project             keep only the fields the op asks for
 6 mask and scrub      captured values to [captured]; secrets scrubbed
   |
   v
 result to the cloud
```

How to check each claim. Paths are in this repository. `cargo test` and `cd worker && npm test` run the tests.

| Claim | Enforced in | Pinned by |
| --- | --- | --- |
| The runner only connects out; it opens no port | `src/exchange/mod.rs` (client only; `src/` has no listener) | `tests/no_listener_test.rs` `the_runner_only_connects_out_and_opens_no_port`, `cargo_toml_has_no_server_crate` |
| Every URL stays on the resource origin, redirects too | `src/origin.rs`, `src/ops/http.rs` (own redirect loop; the client is built with `redirect::Policy::none()`) | `src/origin.rs` `check_origin_table`; `src/ops/http_tests.rs` `host_not_allowed`, `redirect_different_host_refused` |
| A browser page cannot reach another origin | `worker/src/browser.js` (route filter) | `worker/test/policy.test.js` `off-origin fetch is blocked and counted`, `main-frame redirect off-origin fails with host-not-allowed` |
| Read-only SQL is checked, then run in a read-only transaction | `src/readonly.rs`; `src/ops/sql.rs`, `src/ops/sql_pg.rs` | `src/readonly.rs` `check_table`; `src/ops/sql_tests.rs` `read_only_refuses_write_before_reaching_conn`; `write_on_read_only_is_refused_and_nothing_written` in `src/ops/sql_pg_tests.rs` (Postgres) and `src/ops/sql_mysql_tests.rs` (MySQL). They need a real database. CI runs both and does not let them skip. |
| Op arguments have closed key sets | `src/ops/mod.rs` `closed_arg_keys` | `src/ops/ops_tests.rs` `dispatch_unknowns`; `src/ops/cli_tests.rs` `unknown_command_and_closed_keys`; `src/ops/browser_tests.rs` `closed_arg_keys` |
| `${NAME}` works only in allowed slots, and only for listed names | `src/placeholder.rs`, `src/ops/subst.rs` | `src/placeholder.rs` `disallowed_slot`; `src/ops/http_tests.rs` `placeholder_in_disallowed_slot`, `secrets_list_allows_only_listed_names`; `src/ops/sql_tests.rs` `sql_takes_no_placeholders_even_when_listed`; `src/ops/cli_tests.rs` `placeholders_only_in_env_for_allowed_names` |
| Results hold only the projected fields | `src/project.rs` | `src/ops/http_tests.rs` `status_only_projection_nulls_body`, `response_header_projected_only_when_asked`; `src/ops/sql_tests.rs` `empty_projection_yields_no_rows` |
| Secrets are scrubbed, also in URL- and base64-encoded forms | `src/scrub.rs`, `src/ops/mod.rs` `scrub_payload` | `src/scrub.rs` `scrub_table`; `src/ops/http_tests.rs` `auth_register_scrubs_secret`; `src/ops/listed_secrets_tests.rs` `sql_row_holding_listed_http_secret_is_scrubbed` |
| Captured values stay in the runner and show as `[captured]` | `src/ops/capture.rs`, `src/ops/vault.rs` | `src/ops/http_capture_tests.rs` `capture_masks_stores_and_scrubs_later`, `other_run_cannot_resolve` |
| The token env name cannot be listed in `secrets` or used as `${NAME}` | `src/config.rs`, `src/ops/subst.rs` | `src/config_tests.rs` `secrets_listing_the_token_env_is_refused`; `src/ops/http_tests.rs` `runner_token_name_is_never_substitutable` |
| `cloud.url` must be https (http only for loopback or the test switch) | `src/config.rs` `check_cloud_scheme` | `src/config_tests.rs` `cloud_url_scheme_rules` |
| Logs carry no payloads | `src/logfmt/`, `src/ops/log.rs` (closed key list) | `tests/log_sub_test.rs` `op_failed_is_an_error_line_without_payload`; `tests/log_format_test.rs` `every_tracing_macro_in_src_is_on_the_lists`; `worker/test/server.test.js` `secret in args or in bad fields never reaches the log` |
| A cli command runs without a shell, with a cleared env, and only with a declared argument shape | `worker/src/cli.js`; `src/argshape.rs`, `src/ops/cli.rs` | `worker/test/cli.test.js` `run: argv passed exactly, no shell expansion`, `env is cleared; only given env, fixed PATH/HOME, cwd is the run dir`; `src/argshape.rs` `classes_accept_and_refuse`, `shapes_match_exactly`; `src/ops/cli_tests.rs` `argv_shape_refusals` |
| The worker holds no runner secret | `ops/docker-compose.yml` (the worker has no `env_file`); `worker/src/` reads no secret | `worker/test/no-runner-secret.test.js` `the worker reads only the closed set of environment names and never passes its env on`, `compose gives the worker service no env_file and only ZRIZ_WORKER_SOCKET` |
| No `unsafe` code in the runner | `Cargo.toml` `unsafe_code = "forbid"` | the compiler, in every build; no separate test |

What the runner does not protect against:

- The cloud chooses the ops. Inside the allowlist it can read whatever your resources return to a query it picks, minus the scrubbed secrets. A secret listed on a resource can be sent to any path of that resource's origin.
- Scrubbing is by value. It finds a secret as is, URL-encoded and base64-encoded, and in no other form (hex, for example). A short or common secret also hides matching text that is not the secret. Use long random secrets.
- Data that is not a listed or captured secret (a row, a name, an email) is not scrubbed. If an op projects it, it goes to the cloud.
- A resource not marked `read-only: true` gets no SQL check. For one that is marked, the database account is still your first guard: give it read rights only.
- A secret you pass to the worker (a `fill` value, a cli `env` value) is visible to that worker process and to the cli command you allowed.
- The runner trusts the config you write. A wide `base-url`, a loose cli `shapes` list, or a binary that does more than you think, widens what the cloud can do.

## Container image

The image is `ghcr.io/zriztech/runner`. It is public. You do not need to log in to pull it. The worker image, for `browser` and `cli` resources, is `ghcr.io/zriztech/worker`.

The images exist from the first release tag on. Before that, build from source (next section).

Both images are built for `linux/amd64` and `linux/arm64`. Tags:

- `1.2.3` never changes. The release workflow refuses to publish a version twice. Pin this one.
- `1.2` and `latest` move only when the new release is the highest stable version so far. A pre-release (`1.3.0-rc1`) gets its own tag only. Do not use `latest` in a guide or a Compose file.

Write the token and the passwords in a file named `runner.env`, with an editor (one `NAME=value` per line), so they stay out of your shell history:

    ZRIZ_RUNNER_TOKEN=zrt_...
    SHOP_DB_PASSWORD=...

Then:

    chmod 600 runner.env
    docker run -d --restart unless-stopped --name zriz-runner \
      --env-file runner.env \
      -e ZRIZ_RUNNER_CONFIG=/config/config.json \
      -v "$PWD/config.json:/config/config.json:ro" \
      ghcr.io/zriztech/runner:<version>

Do not write `-e ZRIZ_RUNNER_TOKEN=value` on the command line. The shell keeps it in its history.

### Verify the image

A release runs only after CI passes, and only from a commit on `main`. Each release image is signed without a key (Sigstore keyless) and has a build record (provenance) and an SBOM. The runner binary is built with `cargo-auditable`, so the SBOM lists its Rust crates. Replace `<version>` with the version you use, for example `0.1.0`.

Check the signature. It must come from the release workflow of this repository, on a `v` tag:

    cosign verify ghcr.io/zriztech/runner:<version> \
      --certificate-identity-regexp '^https://github\.com/ZrizTech/runner/\.github/workflows/release\.yml@refs/tags/v' \
      --certificate-oidc-issuer https://token.actions.githubusercontent.com

Check the GitHub build attestation:

    gh attestation verify oci://ghcr.io/zriztech/runner:<version> --owner ZrizTech

To pin the exact image, take the digest from the output and run `ghcr.io/zriztech/runner@sha256:<digest>`. The same two commands work for `ghcr.io/zriztech/worker`.

The runner logs its build id at start as `build=<version>+<12-char commit>`. A local build shows `+dev`.

## Quick start (build from source)

Build the images from this folder:

    docker build -t zriz-runner .
    docker build -f worker/Dockerfile -t zriz-worker .

Copy `examples/config.json`, edit it, and write `runner.env` as in the section above (the token and every password the config uses). Then:

    chmod 600 runner.env
    docker run --rm \
      --env-file runner.env \
      -e ZRIZ_RUNNER_CONFIG=/config/config.json \
      -v "$PWD/config.json:/config/config.json:ro" \
      zriz-runner

The token comes from your zriz account (`zrt_...`). Every `${NAME}` the config uses must be set in the container environment, or the runner exits with code 2 and names the missing variable. If you use `browser` or `cli` resources, run the worker too and share its socket (`worker.socket`) through a volume. `ops/docker-compose.yml` does this; see `ops/README.md`.

## Config reference

One JSON file, path in `ZRIZ_RUNNER_CONFIG`. In these keys, `${NAME}` is replaced from the process environment at load: `cloud.url`, `cloud.token-env`, `worker.socket`, and each resource's `base-url`, `connection` and `origins`. A missing name is a startup error. Unknown keys are ignored. A full example is `examples/config.json`.

Top level:

| Key | Meaning |
| --- | --- |
| `cloud.url` | Cloud base URL. Required. Must be https. |
| `cloud.token-env` | Name of the env var that holds the runner token. Required. |
| `evidence` | `"none"` turns off `evidence.fetch` (the cloud asking for a prior op's excerpt). Any other value keeps it on; excerpts are redacted, kept 120 s, at most 1000 entries. |
| `worker.socket` | Path of the worker's unix socket. Required if any resource is `browser` or `cli`. |
| `resources` | Map of resource id to resource. |

Every resource has `type`: `http`, `sql`, `browser` or `cli`. An unknown type is an error.

`secrets` (all types except `sql`): the list of env var names an op may use as `${NAME}` on this resource. Omitted or empty: none. It must not list the token env name.

**http**

| Key | Meaning |
| --- | --- |
| `base-url` | Required. Scheme and host. Requests must stay on this origin (scheme, host, port). |
| `cookies` | `true` keeps a cookie jar per run and resource. Values stay in the runner. Default `false`. |
| `secrets` | See above. `${NAME}` is allowed in `headers`, `body` and `query-params`. |

**sql**

| Key | Meaning |
| --- | --- |
| `connection` | MySQL: `user:pass@tcp(host:3306)/db`. Postgres: a `postgres://` or `postgresql://` URL. Put the password in `${NAME}`. |
| `read-only` | `true`: statements are checked for writes, and run in a read-only transaction. Default `false`. |

At most 2 connections per resource. A query reads at most 1000 rows (`row-count` is the number read). A projection returns at most 100 rows.

**browser** (needs `worker.socket`)

| Key | Meaning |
| --- | --- |
| `base-url` | Required. A bare origin: no path, query or fragment. |
| `origins` | Extra allowed origins, bare, at most 15. Any other request the page makes is blocked. |
| `secrets` | `${NAME}` is allowed in `fill` and `press-seq` values. |
| `max-contexts` | 1 to 16 (worker default 3). The limit of this resource: only its contexts count. A run holds its place until the run ends or its context is idle for `idle-ms`; the worker never closes a context to make room. When all places are in use, a new op waits up to 5 s (or until just before its own timeout), then fails with `runner-at-capacity` and the numbers. |
| `idle-ms` | 1000 to 3600000 (worker default 10 minutes). A context idle for this time is closed; the next op of that run fails once with `context-lost`. |
| `viewport` | `{"width": 1280, "height": 800}`. |

**cli** (needs `worker.socket`)

| Key | Meaning |
| --- | --- |
| `commands` | Required, non-empty. Map of command name (`[A-Za-z0-9_-]`, up to 64) to the command below. |
| `secrets` | `${NAME}` is allowed in `env` values only. |
| `max-handles` | Most live background processes per run, 1 to 8. |
| `idle-ms` | 1 to 3600000. |

A command:

| Key | Meaning |
| --- | --- |
| `path` | Absolute path of the binary inside the worker image. No `..`. |
| `argv-prefix` | Always the first arguments. At most 16. |
| `shapes` | Allowed argument lists after the prefix, at least one. A token is a literal or a slot `{name:class}`. Classes: `slug`, `word`, `int`, `relpath`. No class accepts a leading `-` or `..`. An op's arguments must equal one shape. |
| `env` | Fixed environment of the process. At most 32. |
| `env-allow` | Env names an op may set (values may hold `${NAME}`). Not `HOME`, not a name in `env`. At most 16. |
| `timeout-ms` | Required. 1 to 600000. |
| `max-life-ms` | Required. Hard kill for a background process. 1 to 86400000. |
| `max-output-bytes` | Required. Cap per output stream. 1 to 1048576. |

The process starts without a shell, with a cleared environment.

Notes on `cli` shapes:

- The class `slug` permits only lowercase letters, digits and `-`. A value with an uppercase letter or `_` needs the class `word`.
- The class `relpath` refuses a path that starts with `.zriz`. It takes only `slug` segments and a `.json` end, so a dot folder never matches.
- Each command starts with `HOME` equal to the run folder.

## Security rules

- `${NAME}` works only for names in the resource's `secrets` (or values the run captured), and only in the slots listed above. Never in SQL. A placeholder anywhere else is refused.
- Secret values should be long and random. A short or common value listed in `secrets` is scrubbed wherever it appears in results.
- Never list the token env name in `secrets`. The config is rejected if you do.
- `cloud.url` must be https. Plain http is accepted only for `localhost`, `127.0.0.1` and `::1`, or when `ZRIZ_RUNNER_INSECURE_CLOUD=1`. Use that only for local tests.
- An op runs at most 10 minutes. An op that asks for more, or for nothing, gets 10 minutes.
- Use a database account that can only read, and still set `read-only: true`.
- Give the worker no secrets. Do not put secrets in `env` of a cli command; use `secrets` and `env-allow`.

## Environment variables

| Name | Use |
| --- | --- |
| `ZRIZ_RUNNER_CONFIG` | Path of the config file. Required. |
| The name in `cloud.token-env` (for example `ZRIZ_RUNNER_TOKEN`) | The runner token. Required. |
| `ZRIZ_LOG` | Log filter, for example `debug` or `warn,runner=info` (the default). |
| `ZRIZ_RUNNER_INSECURE_CLOUD` | `1` allows a plain http cloud URL. Local tests only. |
| `ZRIZ_WORKER_SOCKET` | Worker only: the unix socket path it serves. |

Logs go to stdout, one line per event, never payloads: only ids, kinds, reasons and timings.

## Error frames

A failed op gives an error frame: `{"op-id", "reason", "details"}`. `reason` is one word of the closed list in `contract/error.json`. `details` has ids and numbers only (for example `resource`, `name`, `limit`, `busy`, `waited-ms`) and can be empty. The frame has no free text and no `message`: the cloud makes the message from the reason and `details`. A reason the runner cannot prove is `runner-error`, with one fixed `where` word in `details`. Only the faults of the runner itself make its health `degraded`: `worker-error`, and `runner-error` with the words `op-handler`, `response-encoding`, `worker-word`, `evidence-excerpt` or `worker-deadline`. A failed connect to the target is `connection-error`; a query the database refuses is a failed result (the cloud reads it as `sql-error`). When the cloud refuses a request, the runner sends the frames of it one by one, and drops a frame only after three refusals of that frame alone, spaced by the backoff.

Example:

    {"op-id": "0c7d6f0e-0a51-4a7b-8f0e-5a1f0d1c2b3a", "reason": "runner-at-capacity",
     "details": {"resource": "shop-browser", "limit-name": "max-contexts", "limit": 8, "busy": 8, "waited-ms": 5000}}

## Release note: cloud version

This runner needs a cloud with the run-cause contract (the cloud of the same release). It is not compatible with an older cloud: the error frame has `details` and no `message`, the reasons are the new closed list, and the exchange request has `health`.

## Build and test

You need Rust (see `rust-version` in `Cargo.toml`) and, for the worker, Node 22 or newer.

    cargo build --release
    cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
    cd worker && npm ci && npm test

The browser tests need Chromium: `npx playwright install chromium` in `worker/`.

Run the binary. Read the token without echo, so it stays out of your shell history, then start it:

    read -rs ZRIZ_RUNNER_TOKEN && export ZRIZ_RUNNER_TOKEN
    ZRIZ_RUNNER_CONFIG=config.json cargo run --release

Set every other `${NAME}` the config uses in the same way.

## More

- Docs: [zriz.io/docs/runner](https://zriz.io/docs/runner)
- Deploy to your own server: `ops/README.md`
- Wire protocol (shared with the cloud): `contract/README.md`
- Security: `SECURITY.md`. Contributing: `CONTRIBUTING.md`.

## License

The runner source in this repository, the runner image and the worker source are Apache License 2.0. See `LICENSE`. Copyright ZrizTech.

The worker image holds more than that. It also holds Chromium, Node.js and Debian packages, each under its own license. The worker image is therefore not all Apache-2.0.
