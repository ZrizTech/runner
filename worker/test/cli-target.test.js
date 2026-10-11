import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createCliHost } from '../src/cli.js'
import { validateRequest, validateResponse } from '../src/schema.js'

// Time budget rules (spec 8.1) for cli.exec: the command waits D = 200 ms. A number passes when
// D - 15 <= t < D + S/2 = 700. The children are real, so the lower limit has the step of the wait.
const D = 200
const passes = (t) => D - 15 <= t && t < 700
const NODE = process.execPath
let n = 0
const mkHost = (over) => createCliHost({ root: mkdtempSync(join(tmpdir(), 'zt-')), ...over })
const pol = (over = {}) => ({ resource: 'res', path: NODE, 'timeout-ms': 5000, 'max-life-ms': 30000, 'max-output-bytes': 65536, ...over })
const call = async (h, policy, args, over) => {
  const r = { v: 1, 'op-id': `op-${++n}`, run: 'r-1', kind: 'cli.exec', policy, args, 'deadline-ms': 10000, ...over }
  assert.equal(validateRequest(r), true, JSON.stringify(validateRequest.errors))
  const resp = await h.handle(r)
  assert.equal(validateResponse(resp), true, JSON.stringify(validateResponse.errors))
  return resp
}
const js = (code) => ['-e', code]
const MARK = { stream: 'stdout', after: 'READY=' }
// Waits D, then prints the marker and stays up.
const READY = `setTimeout(() => { process.stdout.write('READY=tok1 '); setInterval(() => {}, 1000) }, ${D})`

// Waits for a file `go` in its folder, then acts D later: the test sets the start of the wait itself.
const gated = (act) => `const t = setInterval(() => { if (require('fs').existsSync('go')) { clearInterval(t); setTimeout(() => { ${act} }, ${D}) } }, 5)`
const go = (h) => writeFileSync(join(h.dirOf('r-1', 'res'), 'go'), '1')

test('run: from the spawn to the end of the command', async () => {
  const h = mkHost()
  const r = await call(h, pol(), { mode: 'run', args: js(`setTimeout(() => {}, ${D})`) })
  assert.ok(passes(r['target-ms']), `got ${r['target-ms']}`)
  assert.equal(Number.isInteger(r['target-ms']), true)
  assert.equal('target-ms' in r.out, false)
  await h.closeAll()
})

test('run: on a timeout, to the end of the stop of the command', async () => {
  const h = mkHost()
  const r = await call(h, pol({ 'timeout-ms': D }), { mode: 'run', args: js('setInterval(() => {}, 1000)') })
  assert.equal(r.out['timed-out'], true)
  assert.ok(passes(r['target-ms']), `got ${r['target-ms']}`)
  await h.closeAll()
})

test('start with until: from the spawn to the marker', async () => {
  const h = mkHost()
  const r = await call(h, pol(), { mode: 'start', handle: 'a', args: js(READY), until: MARK })
  assert.ok(passes(r['target-ms']), `got ${r['target-ms']}`)
  await h.closeAll()
})

test('start without until: the spawn only, below 100', async () => {
  const h = mkHost()
  const r = await call(h, pol(), { mode: 'start', handle: 'a', args: js(READY) })
  assert.ok(r['target-ms'] >= 0 && r['target-ms'] < 100, `got ${r['target-ms']}`)
  await h.closeAll()
})

test('read with until: from the start of the wait to the marker; without until: 0', async () => {
  const h = mkHost()
  await call(h, pol(), { mode: 'start', handle: 'a', args: js(`${gated("process.stdout.write('READY=tok1 ')")}; setInterval(() => {}, 1000)`) })
  go(h)
  const r = await call(h, pol(), { mode: 'read', handle: 'a', until: MARK })
  assert.ok(passes(r['target-ms']), `got ${r['target-ms']}`)
  const r0 = await call(h, pol(), { mode: 'read', handle: 'a' })
  assert.equal(r0['target-ms'], 0)
  await h.closeAll()
})

test('wait: from the start of the wait to the end of the command, or to the timeout', async () => {
  const h = mkHost()
  await call(h, pol(), { mode: 'start', handle: 'a', args: js(gated('process.exit(0)')) })
  go(h)
  const r = await call(h, pol(), { mode: 'wait', handle: 'a' })
  assert.ok(passes(r['target-ms']), `got ${r['target-ms']}`)
  await call(h, pol({ 'timeout-ms': D }), { mode: 'start', handle: 'b', args: js('setInterval(() => {}, 1000)') })
  const t = await call(h, pol({ 'timeout-ms': D }), { mode: 'wait', handle: 'b' })
  assert.equal(t.out['timed-out'], true)
  assert.ok(passes(t['target-ms']), `got ${t['target-ms']}`)
  await h.closeAll()
})

test('stop: from the stop signal to the end of the command', async () => {
  const h = mkHost()
  // Ignores SIGTERM for D, then exits itself.
  const code = `process.on('SIGTERM', () => setTimeout(() => process.exit(0), ${D})); process.stdout.write('up'); setInterval(() => {}, 1000)`
  await call(h, pol(), { mode: 'start', handle: 'a', args: js(code), until: { stream: 'stdout', after: 'up' } })
  const r = await call(h, pol(), { mode: 'stop', handle: 'a' })
  assert.ok(passes(r['target-ms']), `got ${r['target-ms']}`)
  await h.closeAll()
})

test('an injected monotonic clock gives the number, cut to whole ms and never negative', async () => {
  const ticks = [10.9, 250.2]
  const h = mkHost({ mono: () => ticks.shift() ?? 0 })
  const r = await call(h, pol(), { mode: 'run', args: js('0') })
  assert.equal(r['target-ms'], 239)
  const back = [50, 40]
  const h2 = mkHost({ mono: () => back.shift() ?? 0 })
  assert.equal((await call(h2, pol(), { mode: 'run', args: js('0') }))['target-ms'], 0)
  await h.closeAll()
  await h2.closeAll()
})

test('a refusal has no target-ms', async () => {
  const h = mkHost()
  const no = await call(h, pol(), { mode: 'read', handle: 'none' })
  assert.equal(no.reason, 'no-handle')
  assert.equal('target-ms' in no, false)
  const keep = js('setInterval(() => {}, 1000)')
  await call(h, pol({ 'max-handles': 1 }), { mode: 'start', handle: 'a', args: keep })
  const busy = await call(h, pol({ 'max-handles': 1 }), { mode: 'start', handle: 'a', args: keep })
  assert.equal(busy.reason, 'handle-busy')
  assert.equal('target-ms' in busy, false)
  const many = await call(h, pol({ 'max-handles': 1 }), { mode: 'start', handle: 'b', args: keep })
  assert.equal(many.reason, 'too-many-handles')
  assert.equal('target-ms' in many, false)
  await h.closeAll()
})
