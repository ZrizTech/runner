import test from 'node:test'
import assert from 'node:assert/strict'
import net from 'node:net'
import { existsSync, mkdtempSync, readFileSync } from 'node:fs'
import { spawn, spawnSync } from 'node:child_process'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { startServer, stopServer } from '../src/server.js'
import { makeLogger } from '../src/log.js'
import { errorResponse } from '../src/protocol.js'
import { parseFilter } from '../src/logfmt.js'
import { validateResponse } from '../src/schema.js'

const SECRET = 'sk-live-SECRET-value-12345'

function call(path, line) {
  return new Promise((resolve, reject) => {
    const s = net.connect(path)
    let out = ''
    s.on('data', (d) => (out += d))
    s.on('end', () => resolve(out))
    s.on('error', reject)
    s.write(line + '\n')
  })
}

async function setup() {
  const path = join(mkdtempSync(join(tmpdir(), 'zw-')), 'w.sock')
  const logs = []
  const server = await startServer(path, { log: makeLogger({ write: (s) => logs.push(s), filter: parseFilter('debug') }) })
  return { path, logs, server }
}

const op = (over = {}) =>
  JSON.stringify({ v: 1, 'op-id': 'op-1', run: 'r-1', kind: 'cli.exec', policy: { resource: 'r', path: '/nonexistent/zriz-bin', 'timeout-ms': 1000, 'max-life-ms': 1000, 'max-output-bytes': 100 }, args: { mode: 'run', args: [SECRET] }, 'deadline-ms': 1000, ...over })

test('ping round trip', async () => {
  const { path, server } = await setup()
  const r = JSON.parse(await call(path, '{"v":1,"kind":"ping"}'))
  assert.deepEqual(r.browser, [])
  assert.deepEqual(r.cli, { busy: 0, limit: 8 })
  assert.match(r['boot-id'], /^b-[0-9a-f]{12}$/)
  assert.equal(validateResponse(r), true, JSON.stringify(validateResponse.errors))
  const again = JSON.parse(await call(path, '{"v":1,"kind":"ping"}'))
  assert.equal(again['boot-id'], r['boot-id'])
  await stopServer(server, path)
})

test('run.close closes the contexts and the handles of one run only; ping has the numbers', async () => {
  const path = join(mkdtempSync(join(tmpdir(), 'zw-')), 'w.sock')
  const logs = []
  const closed = []
  const host = {
    stats: () => [{ resource: 'web', busy: 2 }],
    closeRun: async (run) => { closed.push(run); return run === 'r-1' ? 2 : 0 },
    handle: async () => ({}),
    closeAll: async () => {},
  }
  const cli = { size: () => 1, limit: 8, closeRun: async (run) => (run === 'r-1' ? 1 : 0), closeAll: async () => {} }
  const server = await startServer(path, { host, cli, log: makeLogger({ write: (x) => logs.push(x), filter: parseFilter('debug') }) })
  const TRACE = '6916eece-8a3c-43b0-8280-a90a4ff00b15'
  const ping = JSON.parse(await call(path, '{"v":1,"kind":"ping"}'))
  assert.deepEqual(ping.browser, [{ resource: 'web', busy: 2 }])
  assert.deepEqual(ping.cli, { busy: 1, limit: 8 })
  const close = (run) => call(path, JSON.stringify({ v: 1, kind: 'run.close', run, 'trace-id': TRACE }))
  const r = JSON.parse(await close('r-1'))
  assert.deepEqual(r, { v: 1, kind: 'run.close', ok: true, closed: 3 })
  assert.equal(validateResponse(r), true)
  assert.deepEqual(JSON.parse(await close('r-unknown')), { v: 1, kind: 'run.close', ok: true, closed: 0 })
  assert.deepEqual(closed, ['r-1', 'r-unknown'])
  assert.match(logs.find((l) => l.includes('run closed')), new RegExp(` INFO  trace_id=${TRACE} worker.main {5}run closed run_id=r-1 count=3\n$`))
  await stopServer(server, path)
})

test('a capacity refusal puts busy and cap on the op failed line', async () => {
  const path = join(mkdtempSync(join(tmpdir(), 'zw-')), 'w.sock')
  const logs = []
  const handler = async (req) => errorResponse('at-capacity', req['op-id'], { 'max-contexts': 2, busy: 2, 'waited-ms': 5 })
  const server = await startServer(path, { handler, log: makeLogger({ write: (x) => logs.push(x), filter: parseFilter('debug') }) })
  const req = { v: 1, 'op-id': 'op-1', run: 'r-1', kind: 'browser.page', policy: { resource: 'web', 'base-url': 'http://x.test', origins: ['http://x.test'] }, args: { commands: [{ do: 'goto', path: '/' }], 'command-timeout-ms': 1000 }, 'deadline-ms': 1000 }
  const r = JSON.parse(await call(path, JSON.stringify(req)))
  assert.equal(r.reason, 'at-capacity')
  assert.match(logs[0], / WARN .* worker.browser {2}op failed .* reason=at-capacity busy=2 cap=2 elapsed_ms=/)
  await stopServer(server, path)
})

test('fixed errors: bad v, unknown kind, extra key, bad json', async () => {
  const { path, server } = await setup()
  const cases = [
    ['{"v":2,"kind":"ping"}', 'bad-version'],
    [op({ kind: 'sql.query' }), 'unknown-kind'],
    [op({ extra: 1 }), 'bad-request'],
    ['{"v":1,"kind":"ping","x":1}', 'bad-request'],
    ['not json', 'bad-json'],
  ]
  for (const [line, reason] of cases) {
    const r = JSON.parse(await call(path, line))
    assert.equal(r.ok, false)
    assert.equal(r.reason, reason)
    assert.equal(validateResponse(r), true)
  }
  await stopServer(server, path)
})

test('valid op kind is not implemented in phase 2', async () => {
  const { path, server } = await setup()
  const r = JSON.parse(await call(path, op()))
  assert.equal(r.reason, 'spawn-failed')
  assert.equal(r['op-id'], 'op-1')
  await stopServer(server, path)
})

test('secret in args or in bad fields never reaches the log', async () => {
  const { path, logs, server } = await setup()
  await call(path, op())
  await call(path, op({ extra: SECRET }))
  await call(path, op({ 'op-id': SECRET + ' x' }))
  await call(path, op({ kind: SECRET }))
  await call(path, `{"v":1,"kind":"ping","${SECRET}":1}`)
  await call(path, SECRET)
  assert.ok(logs.length >= 6)
  assert.equal(logs.join('').includes(SECRET), false)
  assert.equal(logs.join('').includes('SECRET'), false)
  await stopServer(server, path)
})

test('log lines are v3 lines with the trace from the request', async () => {
  const { path, logs, server } = await setup()
  const TRACE = '6916eece-8a3c-43b0-8280-a90a4ff00b15'
  await call(path, op({ 'trace-id': TRACE }))
  await call(path, op({ 'op-id': 'op-2' }))
  await call(path, '{"v":1,"kind":"ping"}')
  const re = readFileSync(new URL('../../contract/log/line.regex', import.meta.url), 'utf8').trim()
  for (const l of logs) assert.equal(spawnSync('grep', ['-E', re], { input: l }).status, 0, l) // POSIX ERE: grep, not RegExp
  assert.match(logs[0], new RegExp(` WARN  trace_id=${TRACE} worker.cli {6}op failed run_id=r-1 op_id=op-1 kind=cli.exec mode=run status=error reason=spawn-failed elapsed_ms=\\d+\\.\\d{3}\n$`))
  assert.match(logs[1], / trace_id=- {35} worker.cli /)
  assert.match(logs[2], / DEBUG trace_id=- {35} worker.main {5}ping done elapsed_ms=\d+\.\d{3}\n$/)
  await call(path, 'not json')
  assert.match(logs.at(-1), / WARN  trace_id=- {35} worker.main {5}op failed reason=bad-json elapsed_ms=\d+\.\d{3}\n$/)
  await stopServer(server, path)
})

test('SIGTERM closes the socket and removes the file', async () => {
  const path = join(mkdtempSync(join(tmpdir(), 'zw-')), 'w.sock')
  const child = spawn(process.execPath, [new URL('../src/index.js', import.meta.url).pathname], {
    env: { ...process.env, ZRIZ_WORKER_SOCKET: path },
    stdio: ['ignore', 'pipe', 'ignore'],
  })
  await new Promise((res) => child.stdout.once('data', res))
  assert.ok(existsSync(path))
  const exited = new Promise((res) => child.on('exit', (code) => res(code)))
  child.kill('SIGTERM')
  assert.equal(await exited, 0)
  assert.equal(existsSync(path), false)
})
