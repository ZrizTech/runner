# zriz runner

**Your product keeps working, no matter who, or what, writes the code.**

[zriz](https://zriz.io) tests your real app, end to end, before your customers do. The runner is the part that lives in your network: it makes the calls and holds every secret, so none of them reach the cloud. Docs: [zriz.io/docs/runner](https://zriz.io/docs/runner).

## What it is, and what it trusts

The runner polls the zriz cloud for ops (http, sql, browser, cli), runs them against your systems, and sends back the results.

- It only makes outbound connections. Nothing listens on a port.
- It runs only inside its allowlist: every URL is checked against the resource's origin (redirects too), SQL is read-only where you declare it, op arguments have closed key sets, and `${NAME}` placeholders work only in the allowed slots.
- Secrets live in the runner's environment. The cloud never sees a secret, a connection string or a database driver.
- Results are cut down to the fields the checks need, then every value named in any resource's `secrets` list, and every value the run captured, is scrubbed from every result before it leaves. The cloud redacts again on write.

Browser and cli ops run in a separate Node sidecar, the worker (`worker/`). It holds no runner secret.

## Container image

The image is `ghcr.io/zriztech/runner`. It is public. You do not need to log in to pull it. The worker image, for `browser` and `cli` resources, is `ghcr.io/zriztech/worker`.

The images exist from the first release tag on. Before that, build from source (next section).

Both images are built for `linux/amd64` and `linux/arm64`. Tags:

- `1.2.3` never changes. Pin this one.
- `1.2` moves to the newest patch of that minor version.
- `latest` is the newest release that is not a pre-release. Do not use it in a guide or a Compose file.

Write the token and the passwords in a file with an editor (one `NAME=value` per line), so they stay out of your shell history:

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

Each release image is signed without a key (Sigstore keyless) and has a build record and an SBOM. Replace `<version>` with the version you use, for example `0.1.0`.

Check the signature. It must come from the release workflow of this repository, on a `v` tag:

    cosign verify ghcr.io/zriztech/runner:<version> \
      --certificate-identity-regexp '^https://github\.com/ZrizTech/runner/\.github/workflows/release\.yml@refs/tags/v' \
      --certificate-oidc-issuer https://token.actions.githubusercontent.com

Check the GitHub build attestation:

    gh attestation verify oci://ghcr.io/zriztech/runner:<version> --owner ZrizTech

To pin the exact image, take the digest from the output and run `ghcr.io/zriztech/runner@sha256:<digest>`. The same two commands work for `ghcr.io/zriztech/worker`.

## Quick start (build from source)

Build the images from this folder:

    docker build -t zriz-runner .
    docker build -f worker/Dockerfile -t zriz-worker .

Copy `examples/config.json`, edit it, and run:

    docker run --rm \
      -e ZRIZ_RUNNER_CONFIG=/config/config.json \
      -e ZRIZ_RUNNER_TOKEN=<token> \
      -e SHOP_DB_PASSWORD=<password> \
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

At most 2 connections per resource. At most 1000 rows per query.

**browser** (needs `worker.socket`)

| Key | Meaning |
| --- | --- |
| `base-url` | Required. A bare origin: no path, query or fragment. |
| `origins` | Extra allowed origins, bare, at most 15. Any other request the page makes is blocked. |
| `secrets` | `${NAME}` is allowed in `fill` and `press-seq` values. |
| `max-contexts` | Most live browser contexts in the worker (worker default 3). |
| `idle-ms` | Idle time before a context is closed (worker default 10 minutes). |
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

## Build and test

You need Rust (see `rust-version` in `Cargo.toml`) and, for the worker, Node 22 or newer.

    cargo build --release
    cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
    cd worker && npm ci && npm test

The browser tests need Chromium: `npx playwright install chromium` in `worker/`.

Run the binary: `ZRIZ_RUNNER_CONFIG=config.json ZRIZ_RUNNER_TOKEN=<token> cargo run --release`.

## More

- Docs: [zriz.io/docs/runner](https://zriz.io/docs/runner)
- Deploy to your own server: `ops/README.md`
- Wire protocol (shared with the cloud): `contract/README.md`
- Security: `SECURITY.md`. Contributing: `CONTRIBUTING.md`.

## License

Apache License 2.0. See `LICENSE`. Copyright ZrizTech.
