import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, rmSync } from 'node:fs'
import { join } from 'node:path'
import { constants as osConst } from 'node:os'
import { VERSION, errorResponse } from './protocol.js'
import { createEnded } from './decide.js'

const DEFAULT_ROOT = '/tmp/zriz-run'
const DEFAULT_IDLE_MS = 10 * 60 * 1000
const DEFAULT_MAX_HANDLES = 2
const DEFAULT_MAX_TOTAL = 8
const SWEEP_EVERY_MS = 60 * 1000
const KILL_GRACE_MS = 2000
const TOKEN_GRACE_MS = 100
const POLL_MS = 15
const MAX_SWEPT = 1000

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const sig = (name) => 128 + (osConst.signals[name] ?? 0)

// Runs the runner's resolved command. No shell: spawn(path, argv, {shell: false, detached: true}).
// The child env is cleared, then PATH/HOME/LANG, the policy's fixed env and the op env.
// cwd and HOME are a per-(run, resource) dir under `root`, shared by the run's commands and
// removed when it has had no use and no live handle for the idle time. Handles are keyed
// (run, resource, handle). Killing always targets the whole process group.
// Only ids, mode and exit codes ever leave this file; never argv, env or output.
// `run.close` marks the run as ended (1000 ids at most): its new ops are refused with `context-lost`
// and `why` `run-closed`, and its processes (handles and `run` ops in flight) are stopped.
// A handle that the idle sweep removed is remembered (1000 at most): a later op on it gets
// `no-handle` with `why` `idle`. A folder that cannot be removed is retried by the next sweep.
export function createCliHost({ root = DEFAULT_ROOT, now = Date.now, maxTotal = DEFAULT_MAX_TOTAL, rm = (d) => rmSync(d, { recursive: true, force: true }), log } = {}) {
  const procs = new Map() // handleKey -> proc
  const live = new Set() // every process started and not yet finished
  const dirs = new Map() // run + resource -> { dir, last, idleMs, users }
  const ended = createEnded(MAX_SWEPT)
  const swept = new Set() // handleKey; handles removed by the idle sweep
  const stuck = new Map() // dir -> run; folders that could not be removed
  let sweepTimer = null

  const hkey = (run, resource, handle) => JSON.stringify([run, resource, handle])
  const dkey = (run, resource) => JSON.stringify([run, resource])

  // Best effort: a folder that stays is logged (ids only) and retried by the next sweep.
  function removeDir(dir, run, resource) {
    try { rm(dir); stuck.delete(dir) } catch {
      stuck.set(dir, run)
      log?.emit('WARN', 'worker.cli', 'folder not removed', [['run_id', run], ['resource', resource]])
    }
  }

  function useDir(run, policy) {
    const k = dkey(run, policy.resource)
    let d = dirs.get(k)
    if (!d) {
      const h = createHash('sha256').update(run).digest('hex').slice(0, 16)
      const dir = join(root, h, policy.resource)
      mkdirSync(dir, { recursive: true, mode: 0o700 })
      d = { dir, run, resource: policy.resource, last: now(), idleMs: DEFAULT_IDLE_MS, users: 0 }
      dirs.set(k, d)
    }
    d.idleMs = policy['idle-ms'] ?? DEFAULT_IDLE_MS
    d.last = now()
    return d
  }

  function killGroup(pid, s) {
    try { process.kill(-pid, s) } catch { /* group already gone */ }
  }

  // SIGTERM the group, SIGKILL after the grace. Resolves when the process has closed.
  async function terminate(p) {
    if (!p.exited) {
      killGroup(p.child.pid, 'SIGTERM')
      const t0 = Date.now()
      while (!p.exited && Date.now() - t0 < KILL_GRACE_MS) await sleep(POLL_MS)
    }
    if (!p.closed) killGroup(p.child.pid, 'SIGKILL')
    const t1 = Date.now()
    while (!p.closed && Date.now() - t1 < 1000) await sleep(POLL_MS)
  }

  function drop(p) {
    clearTimeout(p.lifeTimer)
    live.delete(p)
    if (p.hkey) procs.delete(p.hkey)
  }

  function launch(policy, args, dir, runId) {
    const env = { PATH: '/usr/bin:/bin', HOME: dir.dir, LANG: 'C.UTF-8', ...(policy.env ?? {}), ...(args.env ?? {}) }
    env.HOME = dir.dir
    const child = spawn(policy.path, [...(policy['argv-prefix'] ?? []), ...(args.args ?? [])], {
      shell: false, detached: true, env, cwd: dir.dir, stdio: ['ignore', 'pipe', 'pipe'],
    })
    const cap = policy['max-output-bytes']
    const p = {
      child, dir, exited: false, closed: false, code: null, signal: null, error: false,
      cap, out: { stdout: Buffer.alloc(0), stderr: Buffer.alloc(0) }, cursor: { stdout: 0, stderr: 0 },
      truncated: false, lastUse: now(), idleMs: dir.idleMs, hkey: null, lifeTimer: null, marks: new Map(), run: runId, endedRun: false,
    }
    live.add(p)
    dir.users++
    const feed = (name) => (buf) => {
      const have = p.out[name].length
      if (have >= cap) { p.truncated = true; return }
      const take = Math.min(cap - have, buf.length)
      if (take < buf.length) p.truncated = true
      p.out[name] = Buffer.concat([p.out[name], buf.subarray(0, take)])
    }
    child.stdout.on('data', feed('stdout'))
    child.stderr.on('data', feed('stderr'))
    child.stdout.on('error', () => {})
    child.stderr.on('error', () => {})
    child.on('error', () => { p.error = true; p.exited = true; p.closed = true })
    child.on('exit', (code, signal) => {
      p.exited = true
      p.code = code
      p.signal = signal
      killGroup(child.pid, 'SIGKILL') // stragglers of the group
      setTimeout(() => { child.stdout.destroy(); child.stderr.destroy(); p.closed = true }, 200).unref()
    })
    child.on('close', () => { p.closed = true })
    p.lifeTimer = setTimeout(() => { p.lifeKilled = true; terminate(p) }, policy['max-life-ms'])
    p.lifeTimer.unref()
    return p
  }

  const exitCode = (p) => (p.code !== null ? p.code : p.signal ? sig(p.signal) : -1)
  const text = (p, name, from = 0) => p.out[name].subarray(from).toString('utf8')

  function extractAll(p, spec) {
    if (!spec) return undefined
    const res = {}
    for (const [name, m] of Object.entries(spec)) {
      const s = text(p, m.stream)
      const i = s.indexOf(m.after)
      if (i < 0) continue
      const rest = s.slice(i + m.after.length)
      const tok = rest.split(/\s/, 1)[0]
      if (tok !== '') res[name] = tok
    }
    return res
  }

  // Polls until pred() or ms elapsed. Returns true when pred() held.
  async function until(pred, ms) {
    const end = Date.now() + ms
    for (;;) {
      if (pred()) return true
      if (Date.now() >= end) return false
      await sleep(POLL_MS)
    }
  }

  // Marker found in the unread part, and its token is complete (whitespace after it, exit, or a short grace).
  function markerReady(p, m) {
    const s = text(p, m.stream, p.cursor[m.stream])
    const i = s.indexOf(m.after)
    if (i < 0) return false
    if (p.exited || /\s/.test(s.slice(i + m.after.length))) return true
    p.marks.set(m, p.marks.get(m) ?? Date.now())
    return Date.now() - p.marks.get(m) >= TOKEN_GRACE_MS
  }

  function payload(p, args, { advance }) {
    const from = { stdout: p.cursor.stdout, stderr: p.cursor.stderr }
    const o = { stdout: text(p, 'stdout', advance ? from.stdout : 0), stderr: text(p, 'stderr', advance ? from.stderr : 0) }
    if (advance) { p.cursor.stdout = p.out.stdout.length; p.cursor.stderr = p.out.stderr.length }
    o.truncated = p.truncated
    const ex = extractAll(p, args.extract)
    if (ex) o.extract = ex
    return o
  }

  const endedResponse = (req) => errorResponse('context-lost', req['op-id'], { why: 'run-closed' })
  const ok = (req, out) => ({ v: VERSION, 'op-id': req['op-id'], ok: true, out })

  async function finishProc(p) {
    if (p.finished) return // run.close and the op itself may both finish a process
    p.finished = true
    await until(() => p.closed, 1000)
    drop(p)
    p.dir.users--
    p.dir.last = now()
  }

  async function run(req, dir, signal) {
    const { policy, args } = req
    let p
    try { p = launch(policy, args, dir, req.run) } catch { return errorResponse('spawn-failed', req['op-id']) }
    await until(() => p.child.pid !== undefined || p.error, 200)
    if (p.error || p.child.pid === undefined) {
      p.dir.users--
      drop(p)
      return errorResponse('spawn-failed', req['op-id'])
    }
    const ms = Math.min(policy['timeout-ms'], req['deadline-ms'])
    // The runner closed the socket (signal): nobody waits for the answer, stop the process.
    await until(() => p.endedRun || signal?.aborted || (p.exited && p.closed), ms)
    let timedOut = false
    if (!p.endedRun && !(p.exited && p.closed)) { timedOut = true; await terminate(p) }
    await finishProc(p)
    if (p.endedRun) return endedResponse(req)
    const o = { 'exit-code': timedOut ? -1 : exitCode(p), ...payload(p, args, { advance: false }) }
    if (timedOut) o['timed-out'] = true
    return ok(req, o)
  }

  async function start(req, dir) {
    const { policy, args, run: runId } = req
    const k = hkey(runId, policy.resource, args.handle)
    if (procs.has(k)) return errorResponse('handle-busy', req['op-id'])
    let mine = 0
    for (const p of procs.values()) if (p.run === runId) mine++
    const mineMax = policy['max-handles'] ?? DEFAULT_MAX_HANDLES
    if (mine >= mineMax || procs.size >= maxTotal) {
      return errorResponse('too-many-handles', req['op-id'], { limit: mine >= mineMax ? mineMax : maxTotal })
    }
    let p
    try { p = launch(policy, args, dir, runId) } catch { return errorResponse('spawn-failed', req['op-id']) }
    await until(() => p.child.pid !== undefined || p.error, 200)
    if (p.error || p.child.pid === undefined) {
      dir.users--
      drop(p)
      return errorResponse('spawn-failed', req['op-id'])
    }
    p.hkey = k
    swept.delete(k)
    procs.set(k, p)
    if (args.until) {
      await until(() => p.exited || markerReady(p, args.until), Math.min(policy['timeout-ms'], req['deadline-ms']))
      p.lastUse = now()
      return ok(req, { running: !p.exited, 'pid-alive': !p.exited, ...payload(p, args, { advance: true }) })
    }
    return ok(req, { running: true, 'pid-alive': true })
  }

  async function read(req, p) {
    const { policy, args } = req
    if (args.until) await until(() => p.exited || markerReady(p, args.until), Math.min(policy['timeout-ms'], req['deadline-ms']))
    const o = { running: !p.exited, ...payload(p, args, { advance: true }) }
    if (p.exited) o['exit-code'] = exitCode(p)
    return ok(req, o)
  }

  async function wait(req, p) {
    const { policy, args } = req
    const done = await until(() => p.exited && p.closed, Math.min(policy['timeout-ms'], req['deadline-ms']))
    if (!done) return ok(req, { running: true, 'timed-out': true, ...payload(p, args, { advance: true }) })
    const o = { 'exit-code': exitCode(p), ...payload(p, args, { advance: true }) }
    await finishProc(p)
    return ok(req, o)
  }

  async function stop(req, p) {
    await terminate(p)
    const o = { 'exit-code': exitCode(p) }
    await finishProc(p)
    return ok(req, o)
  }

  async function sweep() {
    const t = now()
    for (const p of [...procs.values()]) {
      if (t - p.lastUse > p.idleMs) {
        const k = p.hkey
        await terminate(p)
        await finishProc(p)
        if (k) { swept.delete(k); swept.add(k); if (swept.size > MAX_SWEPT) swept.delete(swept.values().next().value) }
      }
    }
    for (const [k, d] of [...dirs]) {
      const live = [...procs.values()].some((p) => p.dir === d)
      if (!live && d.users <= 0 && t - d.last > d.idleMs) {
        dirs.delete(k)
        removeDir(d.dir, d.run, d.resource)
      }
    }
    for (const [dir, run] of [...stuck]) removeDir(dir, run, undefined)
  }

  function ensureTimer() {
    if (sweepTimer) return
    sweepTimer = setInterval(() => { sweep().catch(() => {}) }, SWEEP_EVERY_MS)
    sweepTimer.unref()
  }

  // req is a validated cli.exec request. Returns a worker response.
  async function handle(req, { signal } = {}) {
    ensureTimer()
    await sweep()
    if (ended.has(req.run)) return endedResponse(req)
    const { policy, args } = req
    const dir = useDir(req.run, policy)
    try {
      if (args.mode === 'run') return await run(req, dir, signal)
      if (args.mode === 'start') {
        return await start(req, dir)
      }
      const p = procs.get(hkey(req.run, policy.resource, args.handle))
      if (!p) {
        return swept.has(hkey(req.run, policy.resource, args.handle))
          ? errorResponse('no-handle', req['op-id'], { why: 'idle' })
          : errorResponse('no-handle', req['op-id'])
      }
      p.lastUse = now()
      p.idleMs = dir.idleMs
      const r = await (args.mode === 'read' ? read(req, p) : args.mode === 'wait' ? wait(req, p) : stop(req, p))
      p.lastUse = now()
      return r
    } finally {
      dir.last = now()
    }
  }

  // Closes the handles and the work folders of the run; returns how many handles.
  async function closeRun(runId) {
    ended.add(runId)
    let n = 0
    for (const p of [...live]) {
      if (p.run !== runId) continue
      p.endedRun = true
      if (p.hkey) n++
      await terminate(p)
      await finishProc(p)
    }
    for (const k of [...swept]) if (JSON.parse(k)[0] === runId) swept.delete(k)
    for (const [k, d] of [...dirs]) {
      if (d.run === runId) { dirs.delete(k); removeDir(d.dir, d.run, d.resource) }
    }
    return n
  }

  return {
    handle,
    sweep,
    closeRun,
    size: () => procs.size,
    limit: maxTotal,
    hasDir: (run, resource) => dirs.has(dkey(run, resource)),
    dirOf: (run, resource) => dirs.get(dkey(run, resource))?.dir,
    async closeAll() {
      if (sweepTimer) clearInterval(sweepTimer)
      sweepTimer = null
      for (const p of [...procs.values()]) { await terminate(p); drop(p) }
      for (const d of dirs.values()) removeDir(d.dir, d.run, d.resource)
      dirs.clear()
    },
  }
}
