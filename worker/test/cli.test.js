import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, existsSync, readdirSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createCliHost } from '../src/cli.js'
import { validateRequest, validateResponse } from '../src/schema.js'

const NODE = process.execPath
let n = 0
const mkHost = (over) => createCliHost({ root: mkdtempSync(join(tmpdir(), 'zc-')), ...over })
const pol = (over = {}) => ({ resource: 'res', path: NODE, 'timeout-ms': 5000, 'max-life-ms': 30000, 'max-output-bytes': 65536, ...over })
const req = (policy, args, over = {}) => ({ v: 1, 'op-id': `op-${++n}`, run: 'r-1', kind: 'cli.exec', policy, args, 'deadline-ms': 10000, ...over })
const call = async (h, policy, args, over) => {
  const r = req(policy, args, over)
  assert.equal(validateRequest(r), true, JSON.stringify(validateRequest.errors))
  const resp = await h.handle(r)
  assert.equal(validateResponse(resp), true, JSON.stringify(validateResponse.errors))
  return resp
}
const alive = (pid) => { try { process.kill(pid, 0); return true } catch { return false } }
const waitDead = async (pid) => { for (let i = 0; i < 100 && alive(pid); i++) await new Promise((r) => setTimeout(r, 30)); return !alive(pid) }
const js = (code, ...rest) => ['-e', code, ...rest]

test('run: argv passed exactly, no shell expansion', async () => {
  const h = mkHost()
  const tail = ['$HOME', 'a;b', '*', '`id`', '$(id)', 'x y']
  const r = await call(h, pol(), { mode: 'run', args: js('process.stdout.write(JSON.stringify(process.argv.slice(1)))', ...tail) })
  assert.equal(r.out['exit-code'], 0)
  assert.deepEqual(JSON.parse(r.out.stdout), tail)
  assert.equal(r.out.truncated, false)
  await h.closeAll()
})

test('argv-prefix comes first', async () => {
  const h = mkHost()
  const r = await call(h, pol({ 'argv-prefix': ['-e', 'process.stdout.write(process.argv[1])'] }), { mode: 'run', args: ['tail'] })
  assert.equal(r.out.stdout, 'tail')
  await h.closeAll()
})

test('env is cleared; only given env, fixed PATH/HOME, cwd is the run dir', async () => {
  process.env.ZRIZ_PARENT_SECRET = 'parent-value'
  const h = mkHost()
  const r = await call(h, pol({ env: { FIXED: 'f' } }), {
    mode: 'run', env: { GIVEN: 'g', HOME: '/evil' },
    args: js('process.stdout.write(JSON.stringify({env: process.env, cwd: process.cwd()}))'),
  })
  const o = JSON.parse(r.out.stdout)
  assert.equal(o.env.ZRIZ_PARENT_SECRET, undefined)
  assert.equal(o.env.GIVEN, 'g')
  assert.equal(o.env.FIXED, 'f')
  assert.equal(o.env.PATH, '/usr/bin:/bin')
  assert.equal(o.env.HOME, h.dirOf('r-1', 'res'))
  assert.equal(await import('node:fs').then((f) => f.realpathSync(o.cwd)), await import('node:fs').then((f) => f.realpathSync(h.dirOf('r-1', 'res'))))
  assert.deepEqual(Object.keys(o.env).filter((k) => k !== '__CF_USER_TEXT_ENCODING').sort(), ['FIXED', 'GIVEN', 'HOME', 'LANG', 'PATH'])
  delete process.env.ZRIZ_PARENT_SECRET
  await h.closeAll()
})

test('output cap per stream keeps the first bytes and sets truncated', async () => {
  const h = mkHost()
  const r = await call(h, pol({ 'max-output-bytes': 10 }), {
    mode: 'run', args: js("process.stdout.write('0123456789ABCDEF'); process.stderr.write('xy')"),
  })
  assert.equal(r.out.stdout, '0123456789')
  assert.equal(r.out.stderr, 'xy')
  assert.equal(r.out.truncated, true)
  await h.closeAll()
})

test('run: exit code and timeout kill', async () => {
  const h = mkHost()
  const r1 = await call(h, pol(), { mode: 'run', args: js('process.exit(3)') })
  assert.equal(r1.out['exit-code'], 3)
  const r2 = await call(h, pol({ 'timeout-ms': 300 }), { mode: 'run', args: js('setInterval(()=>{},1000)') })
  assert.equal(r2.out['timed-out'], true)
  await h.closeAll()
})

test('extract takes the token after the marker', async () => {
  const h = mkHost()
  const r = await call(h, pol(), {
    mode: 'run', args: js("console.error('Visit /device and enter ABCD-1234 now')"),
    extract: { code: { stream: 'stderr', after: ' and enter ' }, none: { stream: 'stdout', after: 'zzz' } },
  })
  assert.deepEqual(r.out.extract, { code: 'ABCD-1234' })
  await h.closeAll()
})

test('start + until marker + read + stop, no orphan', async () => {
  const h = mkHost()
  const script = "console.error('go and enter CODE-9 now'); setTimeout(()=>console.log('later'),400); setInterval(()=>{},1000)"
  const s = await call(h, pol(), {
    mode: 'start', handle: 'login', args: js(script),
    until: { stream: 'stderr', after: ' and enter ' }, extract: { c: { stream: 'stderr', after: ' and enter ' } },
  })
  assert.equal(s.out.running, true)
  assert.match(s.out.stderr, /CODE-9/)
  assert.equal(s.out.extract.c, 'CODE-9')
  const busy = await call(h, pol(), { mode: 'start', handle: 'login', args: js('1') })
  assert.equal(busy.reason, 'handle-busy')
  const r = await call(h, pol(), { mode: 'read', handle: 'login', until: { stream: 'stdout', after: 'later' } })
  assert.equal(r.out.stdout.startsWith('later'), true)
  assert.equal(r.out.stderr, '') // only new output since the last read
  assert.equal(r.out.running, true)
  const pid = [...(h._pids?.() ?? [])][0]
  const st = await call(h, pol(), { mode: 'stop', handle: 'login' })
  assert.equal(typeof st.out['exit-code'], 'number')
  assert.equal(h.size(), 0)
  assert.equal((await call(h, pol(), { mode: 'read', handle: 'login' })).reason, 'no-handle')
  void pid
  await h.closeAll()
})

test('stop kills the whole group; no orphan child', async () => {
  const h = mkHost()
  const script = "const c=require('node:child_process').spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'inherit'}); console.log('CHILD '+c.pid+' ok'); setInterval(()=>{},1000)"
  const s = await call(h, pol(), {
    mode: 'start', handle: 'g', args: js(script),
    until: { stream: 'stdout', after: 'CHILD ' }, extract: { pid: { stream: 'stdout', after: 'CHILD ' } },
  })
  const pid = Number(s.out.extract.pid)
  assert.equal(alive(pid), true)
  await call(h, pol(), { mode: 'stop', handle: 'g' })
  assert.equal(await waitDead(pid), true)
  await h.closeAll()
})

test('stop escalates to SIGKILL when SIGTERM is ignored', async () => {
  const h = mkHost()
  await call(h, pol(), {
    mode: 'start', handle: 'k', args: js("process.on('SIGTERM',()=>{}); console.log('ready'); setInterval(()=>{},1000)"),
    until: { stream: 'stdout', after: 'ready' },
  })
  const t0 = Date.now()
  const st = await call(h, pol(), { mode: 'stop', handle: 'k' })
  assert.equal(st.out['exit-code'], 137)
  assert.ok(Date.now() - t0 >= 1900)
  await h.closeAll()
})

test('wait returns exit code and drops the handle', async () => {
  const h = mkHost()
  await call(h, pol(), { mode: 'start', handle: 'w', args: js("setTimeout(()=>{console.log('bye');process.exit(4)},100)") })
  const w = await call(h, pol(), { mode: 'wait', handle: 'w' })
  assert.equal(w.out['exit-code'], 4)
  assert.equal(w.out.stdout, 'bye\n')
  assert.equal(h.size(), 0)
  await h.closeAll()
})

test('max-life-ms kills a background process', async () => {
  const h = mkHost()
  const s = await call(h, pol({ 'max-life-ms': 300 }), {
    mode: 'start', handle: 'l', args: js("console.log('PID '+process.pid+' x'); setInterval(()=>{},1000)"),
    until: { stream: 'stdout', after: 'PID ' }, extract: { pid: { stream: 'stdout', after: 'PID ' } },
  })
  const pid = Number(s.out.extract.pid)
  assert.equal(await waitDead(pid), true)
  const r = await call(h, pol({ 'max-life-ms': 300 }), { mode: 'read', handle: 'l' })
  assert.equal(r.out.running, false)
  await h.closeAll()
})

test('idle sweep kills the handle and removes the run dir', async () => {
  const h = mkHost()
  const p = pol({ 'idle-ms': 100 })
  const s = await call(h, p, {
    mode: 'start', handle: 'i', args: js("console.log('PID '+process.pid+' x'); setInterval(()=>{},1000)"),
    until: { stream: 'stdout', after: 'PID ' }, extract: { pid: { stream: 'stdout', after: 'PID ' } },
  })
  const pid = Number(s.out.extract.pid)
  const dir = h.dirOf('r-1', 'res')
  assert.equal(existsSync(dir), true)
  await new Promise((r) => setTimeout(r, 250))
  await h.sweep()
  assert.equal(await waitDead(pid), true)
  assert.equal(h.size(), 0)
  await new Promise((r) => setTimeout(r, 150))
  await h.sweep()
  assert.equal(existsSync(dir), false)
  assert.equal(h.hasDir('r-1', 'res'), false)
  await h.closeAll()
})

test('run dir survives between commands of one run (credentials stay)', async () => {
  const h = mkHost()
  await call(h, pol(), { mode: 'run', args: js("require('fs').writeFileSync(process.env.HOME+'/cred.json','1')") })
  const r = await call(h, pol(), { mode: 'run', args: js("process.stdout.write(String(require('fs').existsSync(process.env.HOME+'/cred.json')))") })
  assert.equal(r.out.stdout, 'true')
  await h.closeAll()
})

test('handle limits per run and unknown binary', async () => {
  const h = mkHost()
  const long = js('setInterval(()=>{},1000)')
  await call(h, pol(), { mode: 'start', handle: 'a', args: long })
  await call(h, pol(), { mode: 'start', handle: 'b', args: long })
  const refused = await call(h, pol(), { mode: 'start', handle: 'c', args: long })
  assert.equal(refused.reason, 'too-many-handles')
  assert.equal(typeof refused.limit, 'number')
  assert.equal((await call(h, pol({ path: '/nonexistent/shopctl' }), { mode: 'run', args: [] })).reason, 'spawn-failed')
  await h.closeAll()
  assert.equal(h.size(), 0)
})

test('closeAll kills live handles and removes dirs', async () => {
  const root = mkdtempSync(join(tmpdir(), 'zc-'))
  const h = createCliHost({ root })
  const s = await call(h, pol(), {
    mode: 'start', handle: 'z', args: js("console.log('PID '+process.pid+' x'); setInterval(()=>{},1000)"),
    until: { stream: 'stdout', after: 'PID ' }, extract: { pid: { stream: 'stdout', after: 'PID ' } },
  })
  await h.closeAll()
  assert.equal(await waitDead(Number(s.out.extract.pid)), true)
  assert.deepEqual(readdirSync(root).flatMap((d) => readdirSync(join(root, d))), [])
})
