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

## Browser policing, caps, places (BC-3b)

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
- `policy.max-contexts` (default 3): the limit of that resource; only its contexts count. A run holds its place until
  the run ends (`run.close`) or its context is idle for `idle-ms`. The worker never closes a context to make room.
  With all places in use, the op waits for a place (FIFO: the place goes to the oldest waiter at the moment it is
  freed by `run.close` or the idle sweep) for at most `min(max(0, deadline-ms - 1000), 5000)` ms (`MAX_WAIT_MS`), so
  that the worker's own answer comes before the runner's timer. The wait counts against the op's deadline. After that,
  or at once when the bound is 0: response error `at-capacity` with the integers `max-contexts`, `busy`, `waited-ms`.
- Closed socket: when the runner closes the socket before the answer, a waiting request leaves the queue and an op in
  flight stops (browser commands, cli `run` process), so nobody holds a place for an op that nobody waits for.
- `run.close` (`run`, `trace-id`): closes every context and cli handle of the run; answers `closed` (a count). The
  worker remembers the run as ended (1000 ids at most, oldest out). A later `browser.page` or `cli.exec` of that run is
  refused at once with `context-lost`, `why` `run-closed`, and makes nothing. A waiter, a context still being made, an
  op in flight and a cli process of that run are removed, closed or stopped; their answer is the same.
- Cookies fail closed: if the cookie values of a context cannot be read (the context is gone), no page data
  (`reads`, `title`, `url`) leaves. The answer is `context-lost` / `run-closed` when `run.close` closed it, else `internal`.
- Lost context: the idle sweep closes a context and the host remembers the (run, resource), 1000 at most. The next op
  of that pair gets `context-lost` once with `why` `idle` (log line `context closed`); `run.close` forgets it.
  If the browser process itself goes away, its contexts are dropped and the next op of each answers `internal` once;
  the next launch is tried again by the next op.
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
- Errors: `handle-busy`, `no-handle`, `too-many-handles` (2 per run, 8 total), `spawn-failed`. A handle that the idle
  sweep removed answers `no-handle` with `why` `idle` (1000 handles remembered at most; `run.close` forgets them). A
  handle never started answers plain `no-handle`. After `run.close`, `cli.exec` answers `context-lost` / `run-closed`.
- Folders: `run.close` stops the run's processes (handles and `run` ops in flight) and removes its folders. A folder
  that cannot be removed gives one WARN line (`run_id`, `resource`) and is retried by the next sweep.
- Caps: first N bytes per stream; `max-life-ms` kills the group; idle sweep kills and cleans; the group is
  SIGKILLed when the leader exits, so no orphans.
- Log: `cmd`, `mode`, `exit`, `reason`, `ms`, ids. Never argv, env or output.
