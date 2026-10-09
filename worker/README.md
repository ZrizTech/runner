# zriz-worker

Node sidecar next to the runner. It serves `browser.page` and `cli.exec` ops. It holds no runner
secret.

## How the runner reaches it

A unix socket on a shared tmpfs volume. The path comes from `ZRIZ_WORKER_SOCKET`. No port listens.
One request per connection: the runner writes one JSON line, the worker answers one JSON line and
closes. Max request line: 1 MiB. Schema: `../worker-contract/` (`worker-request.json`,
`worker-response.json`, fixtures). Field `v` is the protocol version (now `1`).

## Error codes

`bad-json`, `bad-version`, `unknown-kind`, `bad-request`, `request-too-large`, `not-implemented`,
`internal`. Messages are fixed strings. No free text from Playwright or from the request.

## Log rule

One line per request on stdout, in the zriz log format (log format v3, `../contract/log/`):
`op done` (INFO) / `op failed` (WARN) on `worker.browser` / `worker.cli`, `ping done` (DEBUG) on
`worker.main`, with `run_id op_id kind cmd mode exit_code status reason elapsed_ms`. The trace is
the request's `trace-id`, else `-`. Never args, values or output. Ids are checked against
`^[A-Za-z0-9_.:-]{1,64}$` and reasons against an enum before they can be logged; only keys on the
list are printed (`src/logfmt.js`). `ZRIZ_LOG` filters (`info` default, `debug,worker.cli=warn`).
`uncaughtException` / `unhandledRejection` print one ERROR `panic` line (location, error class),
then exit 1. A test greps the captured log for a secret-looking value.

## Run

    npm install
    npm test
    ZRIZ_WORKER_SOCKET=/run/zriz/worker.sock npm start

SIGTERM closes the socket and removes the file.

## Browser policing, caps, eviction (BC-3b)

- Every request the page makes is checked against `policy.origins` (the base origin is always in): documents,
  subresources, fetch/XHR, iframes, redirects, WebSockets. Others are aborted and counted in `out.blocked`
  (a number, never a URL). `data:`, `blob:`, `about:` pass; `file:`, `chrome:` and the rest are refused.
  Redirects are not re-offered to a route by Chromium, so allowed requests are fetched with no redirect
  and a 3xx to a foreign origin is refused before the page sees it. Cost: page traffic goes through the worker.
- A blocked main-frame navigation (redirect, click on an off-origin link) fails the command with
  `host-not-allowed`; so does a page left on a foreign origin. Popups are closed at once.
- Cookies: no header (`Cookie`, `Set-Cookie`) ever reaches `out`. The only ways to get text out are the fixed
  `read` kinds (`url` path only, `title`, `text`, `value`, `attr:*`, `count`, `visible`, `overflow-x`); there is
  no eval. After each op the worker replaces every value (length >= 4) of the context's own cookies with
  `[cookie]` in all strings it returns (reads, title, url). The runner cannot know these values, so only the
  worker can. Runner secrets are scrubbed by the runner.
- `policy.max-contexts` (default 3): the limit of that resource; only its contexts count. The worker never closes a
  context to make room. With all places in use, the op waits for a place (FIFO, woken by `run.close` or the idle
  sweep) for at most the smaller of its own `deadline-ms` and 5000 ms (`MAX_WAIT_MS`). The wait counts against the
  op's deadline. After that: response error `at-capacity` with the integers `max-contexts`, `busy`, `waited-ms`.
- `run.close` (`run`, `trace-id`): closes every context and cli handle of the run; answers `closed` (a count).
- Lost context: the idle sweep closes a context and the host remembers the (run, resource), 1000 at most. The next op
  of that pair gets `context-lost` once with `why` `idle` (log line `context closed`); `run.close` forgets it.
- `policy.idle-ms` (default 10 min): contexts idle longer are closed by a sweep on every request and by an
  unref'd 60 s interval. Busy contexts are never swept.
- `ping` answers `browser` (`resource`, `busy` = places in use), `cli` (`busy`, `limit`) and `boot-id`.

## cli.exec (BC-5)

- `policy` (schema `cli-policy`): `resource`, `path` (absolute), `argv-prefix`, `env` (fixed), `cwd: "run"`,
  `timeout-ms`, `max-life-ms`, `max-output-bytes`, optional `command`, `max-handles` (default 2), `idle-ms`
  (default 10 min). `args` (schema `cli-args`): `mode` run/start/read/wait/stop, `args` (argv tail),
  `handle`, `until`/`extract` `{stream, after}`, `env` (resolved values).
- Spawn: `shell: false`, `detached: true`, env cleared then `PATH=/usr/bin:/bin`, `HOME`=run dir, `LANG`,
  policy env, op env (`HOME` cannot be overridden). cwd = `/tmp/zriz-run/<sha256(run)[..16]>/<resource>/`.
  The dir is shared by the run's commands (credentials stay) and removed only when the resource has no live
  handle and no use for `idle-ms`, or on `closeRun`/`closeAll`.
- `out`: `run` -> `exit-code`, `stdout`, `stderr`, `truncated`, `extract`, `timed-out` (exit-code -1);
  `start` -> `running`, `pid-alive` (with `until`: plus output so far); `read` -> `running`, new `stdout`/`stderr`
  since the last read, `truncated`, `extract`, `exit-code` once exited; `wait` -> `exit-code`, output (handle
  dropped); on timeout `running: true, timed-out: true`; `stop` -> `exit-code` (SIGTERM group, SIGKILL after 2 s).
- Errors: `handle-busy`, `no-handle`, `too-many-handles` (2 per run, 8 total), `spawn-failed`.
- Caps: first N bytes per stream; `max-life-ms` kills the group; idle sweep kills and cleans; the group is
  SIGKILLed when the leader exits, so no orphans.
- Log: `cmd`, `mode`, `exit`, `reason`, `ms`, ids. Never argv, env or output.
