import test from 'node:test'
import assert from 'node:assert/strict'
import net from 'node:net'
import { mkdtempSync, existsSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createCliHost } from '../src/cli.js'
import { startServer, stopServer } from '../src/server.js'
import { validateResponse } from '../src/schema.js'

const NODE = process.execPath
let n = 0
const mkHost = (over) => createCliHost({ root: mkdtempSync(join(tmpdir(), 'zc-')), ...over })
const pol = (over = {}) => ({ resource: 'res', path: NODE, 'timeout-ms': 8000, 'max-life-ms': 30000, 'max-output-bytes': 65536, ...over })
const req = (args, over = {}) => ({ v: 1, 'op-id': `op-${++n}`, run: 'r-1', kind: 'cli.exec', policy: pol(), args, 'deadline-ms': 10000, ...over })
const js = (code) => ['-e', code]
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const alive = (pid) => { try { process.kill(pid, 0); return true } catch { return false } }

test('J3: a run op in flight is stopped and its folder removed when run.close arrives', async () => {
  const h = mkHost()
  const op = h.handle(req({ mode: 'run', args: js("console.log('PID '+process.pid); setInterval(()=>{},1000)") }))
  await sleep(300)
  const dir = h.dirOf('r-1', 'res')
  assert.ok(existsSync(dir))
  const t0 = Date.now()
  await h.closeRun('r-1')
  const r = await op
  assert.ok(Date.now() - t0 < 4000, 'the process ran to its timeout')
  assert.equal(r.reason, 'context-lost')
  assert.equal(r.why, 'run-closed')
  assert.equal(validateResponse(r), true, JSON.stringify(validateResponse.errors))
  assert.equal(existsSync(dir), false)
  await h.closeAll()
})

test('J3: a cli op of an ended run is refused at once and makes nothing', async () => {
  const h = mkHost()
  await h.closeRun('r-1')
  const r = await h.handle(req({ mode: 'run', args: js('1') }))
  assert.equal(r.reason, 'context-lost')
  assert.equal(r.why, 'run-closed')
  assert.equal(h.hasDir('r-1', 'res'), false)
  await h.closeAll()
})

test('J8: a handle swept as idle answers no-handle with why idle; a never started handle answers plain no-handle', async () => {
  let t = 1000
  const h = mkHost({ now: () => t })
  const start = await h.handle(req({ mode: 'start', handle: 'srv', args: js('setInterval(()=>{},1000)') }, { policy: pol({ 'idle-ms': 1000 }) }))
  assert.equal(start.ok, true)
  t += 5000
  await h.sweep()
  const r = await h.handle(req({ mode: 'read', handle: 'srv' }, { policy: pol({ 'idle-ms': 1000 }) }))
  assert.equal(r.reason, 'no-handle')
  assert.equal(r.why, 'idle')
  assert.equal(validateResponse(r), true, JSON.stringify(validateResponse.errors))
  const never = await h.handle(req({ mode: 'read', handle: 'other' }))
  assert.equal(never.reason, 'no-handle')
  assert.equal('why' in never, false)
  await h.closeAll()
})

test('J7: a folder that cannot be removed does not fail run.close; a later sweep retries it', async () => {
  let t = 1000
  let fail = true
  const removed = []
  const lines = []
  const rm = (d) => { if (fail) throw new Error('EBUSY /secret/path'); removed.push(d); rmSync(d, { recursive: true, force: true }) }
  const h = mkHost({ now: () => t, rm, log: { emit: (...a) => lines.push(a) } })
  assert.equal((await h.handle(req({ mode: 'run', args: js('1') }))).ok, true)
  const dir = h.dirOf('r-1', 'res')
  await h.closeRun('r-1') // must not throw
  assert.equal(h.hasDir('r-1', 'res'), false)
  assert.ok(existsSync(dir))
  assert.equal(lines.length, 1)
  assert.equal(JSON.stringify(lines).includes('/secret/path'), false)
  assert.deepEqual(lines[0].slice(0, 3), ['WARN', 'worker.cli', 'folder not removed'])
  fail = false
  t += 10 * 60 * 1000
  await h.sweep()
  assert.deepEqual(removed, [dir])
  assert.equal(existsSync(dir), false)
  await h.closeAll()
})

test('J2: the server gives the handler a signal that aborts when the socket closes', async () => {
  const path = join(mkdtempSync(join(tmpdir(), 'zw-')), 'w.sock')
  let sig
  const handler = (r, o) => new Promise((res) => { sig = o.signal; sig.addEventListener('abort', () => res({ v: 1, ok: false, reason: 'internal', message: 'x' })) })
  const server = await startServer(path, { handler })
  const s = net.connect(path)
  s.on('error', () => {})
  s.write(JSON.stringify({ v: 1, 'op-id': 'op-1', run: 'r-1', kind: 'browser.page', policy: { resource: 'w', 'base-url': 'http://x.test', origins: ['http://x.test'] }, args: { commands: [{ do: 'goto', path: '/' }] }, 'deadline-ms': 5000 }) + '\n')
  await sleep(100)
  assert.equal(sig.aborted, false)
  s.destroy()
  await sleep(100)
  assert.equal(sig.aborted, true)
  await stopServer(server, path)
})
