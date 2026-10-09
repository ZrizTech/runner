import { chromium } from 'playwright'
import { VERSION } from './protocol.js'
import { classify, pathOf, runCommand, CommandError } from './commands.js'
import { errorResponse } from './protocol.js'

const DEFAULT_TIMEOUT_MS = 10000
const DEFAULT_VIEWPORT = { width: 1280, height: 800 }
const DEFAULT_MAX_CONTEXTS = 3
const DEFAULT_IDLE_MS = 10 * 60 * 1000
// Longest wait for a free context slot; the op's own deadline can make it shorter.
const MAX_WAIT_MS = 5000
const SWEEP_EVERY_MS = 60 * 1000
const LOCAL_SCHEMES = new Set(['data:', 'blob:', 'about:'])

const wsToHttp = (u) => u.replace(/^ws(s?):/, 'http$1:')

// One Chromium per worker, launched lazily. One context (one page) per (run, resource),
// kept across ops of the same run. Every request of the page is checked against the
// policy origins. Each resource has its own limit, `policy.max-contexts`: only the contexts of
// that resource count. The worker never closes a context to make room. When a new context is
// necessary and all places of the resource are in use, the op waits for a place (first come first
// served), at most min(its deadline, 5 s), then it gets `at-capacity` with the numbers. A place
// is freed by `run.close` (closeRun) or by the idle sweep. A context closed by the sweep is
// remembered (run, resource, 1000 entries at most): the next op of that run on that resource
// gets `context-lost` once, with `why` `idle`. A context idle longer than `policy.idle-ms` is
// closed by a sweep on each request and by an unref'd timer. `now` is injectable for tests.
export const MAX_LOST = 1000

export function createBrowserHost({ launch = () => chromium.launch({ headless: true }), now = Date.now, maxWaitMs = MAX_WAIT_MS, log } = {}) {
  let browserP = null
  const waiters = [] // { resource, wake }
  const lost = new Set() // key; contexts closed by the idle sweep, reported once to the next op
  const known = new Set() // resources seen in a request
  const creating = new Map() // resource -> contexts being created
  let sweepTimer = null
  const sessions = new Map() // key -> { ctx, page, last, queue, st, run, resource }
  const key = (run, resource) => JSON.stringify([run, resource])

  const browser = () => (browserP ??= launch())

  // The places of a resource in use: its contexts and those being created.
  const used = (resource) => {
    let n = creating.get(resource) ?? 0
    for (const e of sessions.values()) if (e.resource === resource) n++
    return n
  }

  // Returns a session, or { refused } with the numbers for `at-capacity`.
  async function session(run, policy, budgetMs) {
    const resource = policy.resource
    const k = key(run, resource)
    const max = policy['max-contexts'] ?? DEFAULT_MAX_CONTEXTS
    const bound = Math.min(budgetMs, maxWaitMs)
    const t0 = now()
    known.add(resource)
    let s = sessions.get(k)
    if (s) { s.busy++; return { s, waited: 0 } }
    // Take a place (creating) before the first await, so that no other op can take the same one.
    for (;;) {
      s = sessions.get(k)
      if (s) { s.busy++; return { s, waited: now() - t0 } }
      if (used(resource) < max) { creating.set(resource, (creating.get(resource) ?? 0) + 1); break }
      // All places in use: wait for one, for a bounded time.
      const left = bound - (now() - t0)
      if (left <= 0 || !(await waitPlace(resource, left))) {
        return { refused: { 'max-contexts': max, busy: used(resource), 'waited-ms': Math.max(0, Math.round(now() - t0)) } }
      }
    }
    const waited = now() - t0
    let ctx
    try {
    const b = await browser()
    ctx = await b.newContext({
      acceptDownloads: false,
      serviceWorkers: 'block',
      bypassCSP: false,
      locale: 'en-US',
      timezoneId: 'UTC',
      viewport: policy.viewport ?? DEFAULT_VIEWPORT,
    })
    s = { ctx, page: null, run, resource, last: now(), idleMs: DEFAULT_IDLE_MS, allowed: new Set(), busy: 1, queue: Promise.resolve(), st: null }
    // Every request (documents, subresources, fetch/XHR, iframes, redirects) must stay on an allowed origin.
    const refuse = (req, route) => {
      if (s.st) {
        s.st.blocked++
        let main = false
        try { main = req.isNavigationRequest() && req.frame() === s.page.mainFrame() } catch { /* not a frame request */ }
        if (main) s.st.navBlocked = true
      }
      return route.abort('blockedbyclient').catch(() => {})
    }
    await ctx.route('**/*', async (route) => {
      const req = route.request()
      let u = null
      try { u = new URL(req.url()) } catch { /* refused below */ }
      const ok = u !== null && (u.protocol === 'http:' || u.protocol === 'https:' ? s.allowed.has(u.origin) : LOCAL_SCHEMES.has(u.protocol))
      if (ok && u.protocol !== 'http:' && u.protocol !== 'https:') return route.continue().catch(() => {})
      if (ok) {
        // Chromium follows redirects without asking us again, so fetch with no redirect and hand the
        // 3xx to the page: the page then issues the next hop as a new request, which is checked here.
        let resp
        try { resp = await route.fetch({ maxRedirects: 0 }) } catch { return route.abort('failed').catch(() => {}) }
        // A 3xx to a foreign origin is refused here, before the page can follow it.
        const loc = resp.status() >= 300 && resp.status() < 400 ? resp.headers().location : undefined
        if (loc !== undefined) {
          let target = null
          try { target = new URL(loc, req.url()) } catch { /* refused below */ }
          const good = target !== null && (target.protocol === 'http:' || target.protocol === 'https:') && s.allowed.has(target.origin)
          if (!good) return refuse(req, route)
        }
        return route.fulfill({ response: resp }).catch(() => {})
      }
      return refuse(req, route)
    })
    await ctx.routeWebSocket('**/*', (ws) => {
      let ok = false
      try { ok = s.allowed.has(new URL(wsToHttp(ws.url())).origin) } catch { /* refused */ }
      if (ok) { ws.connectToServer(); return }
      if (s.st) s.st.blocked++
      ws.close()
    })
    const page = await ctx.newPage()
    s.page = page
    // One page per context: popups are closed at once.
    ctx.on('page', (p) => { if (p !== page) p.close().catch(() => {}) })
    page.on('response', (r) => {
      if (s.st && r.request().isNavigationRequest() && r.frame() === page.mainFrame()) s.st.status = r.status()
    })
    sessions.set(k, s)
    } catch (e) {
      await ctx?.close().catch(() => {})
      throw e
    } finally {
      creating.set(resource, creating.get(resource) - 1)
      wake(resource)
    }
    sweepTimer ??= setInterval(() => { sweep().catch(() => {}) }, SWEEP_EVERY_MS)
    sweepTimer.unref?.()
    return { s, waited }
  }

  async function closeSession(k, idle = false) {
    const s = sessions.get(k)
    if (!s) return
    sessions.delete(k)
    lost.delete(k)
    if (idle) {
      lost.add(k)
      if (lost.size > MAX_LOST) lost.delete(lost.values().next().value)
      log?.emit('WARN', 'worker.browser', 'context closed', [['run_id', s.run], ['resource', s.resource], ['reason', 'idle']])
    }
    await s.ctx.close().catch(() => {})
    wake(s.resource)
  }

  // Waiters for a place of one resource, in arrival order. Woken when an op ends or a context closes.
  function waitPlace(resource, ms) {
    return new Promise((resolve) => {
      const w = { resource }
      const t = setTimeout(() => { waiters.splice(waiters.indexOf(w), 1); resolve(false) }, ms)
      t.unref?.()
      w.wake = () => { clearTimeout(t); resolve(true) }
      waiters.push(w)
    })
  }
  const wake = (resource) => {
    for (const w of waiters.filter((x) => x.resource === resource)) { waiters.splice(waiters.indexOf(w), 1); w.wake() }
  }

  // A blocked main-frame navigation, or a page left on a foreign origin, fails the command.
  function checkPolicy(page, st) {
    if (st.navBlocked) throw new CommandError('host-not-allowed')
    let u
    try { u = new URL(page.url()) } catch { throw new CommandError('host-not-allowed') }
    if (u.protocol === 'http:' || u.protocol === 'https:') {
      if (!st.allowed.has(u.origin)) throw new CommandError('host-not-allowed')
    } else if (!LOCAL_SCHEMES.has(u.protocol)) {
      throw new CommandError('host-not-allowed')
    }
  }

  async function sweep(idleMs, at = now()) {
    for (const [k, s] of [...sessions]) {
      if (s.busy === 0 && at - s.last >= (idleMs ?? s.idleMs)) await closeSession(k, true)
    }
  }

  async function execute(s, policy, args, deadlineMs) {
    const base = new URL(policy['base-url'])
    const st = { reads: {}, status: null, baseUrl: base.href, baseOrigin: base.origin, blocked: 0, navBlocked: false }
    s.allowed = new Set([base.origin, ...policy.origins.map((o) => new URL(o).origin)])
    st.allowed = s.allowed
    s.st = st
    const page = s.page
    const timeout = args['command-timeout-ms'] ?? DEFAULT_TIMEOUT_MS
    page.setDefaultTimeout(timeout)
    page.setDefaultNavigationTimeout(timeout)
    let idx = 0
    let error = null
    let failedAt = null
    let timedOut = false
    const timer = new Promise((res) => {
      const t = setTimeout(() => { timedOut = true; res() }, deadlineMs)
      t.unref?.()
      s.cancel = () => clearTimeout(t)
    })
    const loop = (async () => {
      for (; idx < args.commands.length; idx++) {
        if (timedOut) return
        const cmd = args.commands[idx]
        try {
          await runCommand(page, cmd, st)
          checkPolicy(page, st)
        } catch (e) {
          if (timedOut) return
          error = st.navBlocked ? 'host-not-allowed' : await classify(e, cmd, page)
          failedAt = idx
          return
        }
      }
    })()
    await Promise.race([loop, timer])
    s.cancel?.()
    if (timedOut && error === null) { error = 'timeout'; failedAt = Math.min(idx, args.commands.length - 1) }
    let title = ''
    try { title = await page.title() } catch { /* page gone */ }
    s.st = null
    // Remove this context's own cookie values from every string that leaves (only the worker knows them).
    let vals = []
    try { vals = (await s.ctx.cookies()).map((c) => c.value).filter((v) => v.length >= 1) } catch { /* context gone */ }
    vals.sort((x, y) => y.length - x.length)
    // Values of 4+ characters go anywhere; 1-3 characters only as whole tokens, so a short value does not blank ordinary words.
    const esc = (v) => v.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
    const scrubOne = (t, c) => {
      if (c.length >= 4) return t.split(c).join('[cookie]')
      const re = new RegExp(`(^|[^A-Za-z0-9_])${esc(c)}($|[^A-Za-z0-9_])`, 'g')
      // two passes: adjacent matches share their boundary character
      return t.replace(re, '$1[cookie]$2').replace(re, '$1[cookie]$2')
    }
    const scrub = (v) => (typeof v === 'string' ? vals.reduce(scrubOne, v) : v)
    for (const k of Object.keys(st.reads)) st.reads[k] = scrub(st.reads[k])
    title = scrub(title)
    return {
      ok: error === null,
      url: scrub(pathOf(page.url())),
      title,
      status: st.status,
      reads: st.reads,
      blocked: st.blocked,
      'failed-at': failedAt,
      error,
    }
  }

  // req is a validated browser.page request. Returns a worker response.
  async function handle(req) {
    await sweep()
    const k = key(req.run, req.policy.resource)
    if (lost.delete(k) && !sessions.has(k)) {
      return errorResponse('context-lost', req['op-id'], { why: 'idle', 'idle-ms': req.policy['idle-ms'] ?? DEFAULT_IDLE_MS, 'max-contexts': req.policy['max-contexts'] ?? DEFAULT_MAX_CONTEXTS })
    }
    const got = await session(req.run, req.policy, req['deadline-ms'])
    if (got.refused) return errorResponse('at-capacity', req['op-id'], got.refused)
    const { s } = got
    // The wait counts against the op's own deadline.
    const deadlineMs = Math.max(1, req['deadline-ms'] - Math.round(got.waited))
    s.idleMs = req.policy['idle-ms'] ?? DEFAULT_IDLE_MS
    // Ops on one context run one after the other.
    const job = s.queue.then(() => {
      s.last = now()
      return execute(s, req.policy, req.args, deadlineMs)
    })
    s.queue = job.catch(() => {})
    let out
    try { out = await job } finally { s.busy--; s.last = now() }
    return { v: VERSION, 'op-id': req['op-id'], ok: true, out }
  }

  return {
    handle,
    size: () => sessions.size,
    has: (run, resource) => sessions.has(key(run, resource)),
    closeContext: (run, resource) => closeSession(key(run, resource)),
    // Closes every context of the run; returns how many.
    async closeRun(run) {
      let n = 0
      for (const [k, s] of [...sessions]) if (s.run === run) { await closeSession(k); n++ }
      for (const k of [...lost]) if (JSON.parse(k)[0] === run) lost.delete(k)
      return n
    },
    // The places in use for each resource the worker knows.
    stats: () => [...known].map((resource) => ({ resource, busy: used(resource) })),
    sweep,
    async closeAll() {
      if (sweepTimer) clearInterval(sweepTimer)
      sweepTimer = null
      for (const k of [...sessions.keys()]) await closeSession(k)
      lost.clear()
      const p = browserP
      browserP = null
      if (p) await (await p).close().catch(() => {})
    },
  }
}
