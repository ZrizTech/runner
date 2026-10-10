import { chromium } from 'playwright'
import { VERSION } from './protocol.js'
import { classify, pathOf, runCommand, CommandError } from './commands.js'
import { errorResponse } from './protocol.js'
import { createEnded, scrubPage, waitBound } from './decide.js'

const DEFAULT_TIMEOUT_MS = 10000
const DEFAULT_VIEWPORT = { width: 1280, height: 800 }
const DEFAULT_MAX_CONTEXTS = 3
const DEFAULT_IDLE_MS = 10 * 60 * 1000
// Longest wait for a free context slot; the op's own deadline can make it shorter.
const MAX_WAIT_MS = 5000
const SWEEP_EVERY_MS = 60 * 1000
// Longest wait for the error page of a blocked main-frame navigation to commit.
const SETTLE_MAX_MS = 5000
const LOCAL_SCHEMES = new Set(['data:', 'blob:', 'about:'])

const wsToHttp = (u) => u.replace(/^ws(s?):/, 'http$1:')

// One Chromium per worker, launched lazily. One context (one page) per (run, resource),
// kept across ops of the same run. Every request of the page is checked against the
// policy origins. Each resource has its own limit, `policy.max-contexts`: only the contexts of
// that resource count. The worker never closes a context to make room. When a new context is
// necessary and all places of the resource are in use, the op waits for a place (first come first
// served: the place goes to the oldest waiter at the moment it is freed), at most
// min(max(0, deadline - 1000), 5 s), then it gets `at-capacity` with the numbers. A place
// is freed by `run.close` (closeRun) or by the idle sweep. A context closed by the sweep is
// remembered (run, resource, 1000 entries at most): the next op of that run on that resource
// gets `context-lost` once, with `why` `idle`. A context idle longer than `policy.idle-ms` is
// closed by a sweep on each request and by an unref'd timer. `run.close` remembers the run as
// ended (1000 ids at most): its new ops, its waiters and its contexts still being made are
// refused or closed, with `context-lost` and `why` `run-closed`. `now` is injectable for tests.
export const MAX_LOST = 1000

export function createBrowserHost({ launch = () => chromium.launch({ headless: true }), now = Date.now, maxWaitMs = MAX_WAIT_MS, log } = {}) {
  let browserP = null
  const waiters = [] // { resource, run, max, wake, cancel }, oldest first
  const lost = new Map() // key -> 'idle' | 'browser'; contexts lost, reported once to the next op
  const ended = createEnded(MAX_LOST) // runs that got `run.close`
  const making = new Map() // key -> promise of the context being made
  let sweeping = null
  const known = new Set() // resources seen in a request
  const creating = new Map() // resource -> contexts being created
  let sweepTimer = null
  const sessions = new Map() // key -> { ctx, page, last, queue, st, run, resource }
  const key = (run, resource) => JSON.stringify([run, resource])

  // A failed launch is forgotten, so the next op tries again. A browser that goes away takes its
  // contexts with it: the ops of those contexts answer `internal` (the worker has no truer word).
  const browser = () => {
    if (!browserP) {
      const p = Promise.resolve(launch())
      browserP = p
      p.then((b) => {
        b.on?.('disconnected', () => {
          if (browserP !== p) return
          browserP = null
          for (const [k, s] of [...sessions]) { s.closedBy = 'browser'; sessions.delete(k); markLost(k, 'browser'); wakeFree(s.resource) }
        })
      }, () => { if (browserP === p) browserP = null })
    }
    return browserP
  }
  const markLost = (k, why) => {
    lost.delete(k)
    lost.set(k, why)
    if (lost.size > MAX_LOST) lost.delete(lost.keys().next().value)
  }

  // The places of a resource in use: its contexts and those being created.
  const used = (resource) => {
    let n = creating.get(resource) ?? 0
    for (const e of sessions.values()) if (e.resource === resource) n++
    return n
  }

  const refusedEnded = (opId) => errorResponse('context-lost', opId, { why: 'run-closed' })

  // Makes the context and its page. The session is not yet in `sessions`.
  async function create(run, policy) {
    const resource = policy.resource
    let ctx
    let s
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
    } catch (e) {
      await ctx?.close().catch(() => {})
      throw e
    }
    return s
  }

  // Returns { s, waited }, or { refused } with the numbers for `at-capacity`, or { ended: true }
  // (the run got `run.close`), or { aborted: true } (the runner closed the socket).
  async function session(run, policy, budgetMs, signal) {
    const resource = policy.resource
    const k = key(run, resource)
    const max = policy['max-contexts'] ?? DEFAULT_MAX_CONTEXTS
    const bound = waitBound(budgetMs, maxWaitMs)
    const t0 = now()
    known.add(resource)
    let placed = false // a waiter that was woken holds its place already
    for (;;) {
      if (ended.has(run)) { if (placed) freePlace(resource); return { ended: true } }
      if (signal?.aborted) { if (placed) freePlace(resource); return { aborted: true } }
      if (!placed) {
        const s = sessions.get(k)
        if (s) { s.busy++; return { s, waited: now() - t0 } }
        const m = making.get(k)
        if (m) { await m.catch(() => {}); continue }
        // Take a place (creating) before the first await, so that no other op can take the same one.
        if (used(resource) < max) { creating.set(resource, (creating.get(resource) ?? 0) + 1); placed = true }
      }
      if (placed) break
      // All places in use: wait for one, for a bounded time. Check again before a refusal.
      const left = bound - (now() - t0)
      const got = left > 0 ? await waitPlace(resource, run, max, left, signal) : 'timeout'
      if (got === 'placed') { placed = true; continue }
      if (got === 'timeout' && used(resource) < max) continue
      if (got === 'timeout') return { refused: { 'max-contexts': max, busy: used(resource), 'waited-ms': Math.max(0, Math.round(now() - t0)) } }
      // 'ended' and 'aborted' are found at the top of the loop
    }
    const waited = now() - t0
    let s
    const making1 = create(run, policy)
    making.set(k, making1)
    try {
      s = await making1
      if (ended.has(run) || signal?.aborted) {
        await s.ctx.close().catch(() => {})
        return ended.has(run) ? { ended: true } : { aborted: true }
      }
      sessions.set(k, s)
    } finally {
      making.delete(k)
      creating.set(resource, creating.get(resource) - 1)
      wakeFree(resource)
    }
    sweepTimer ??= setInterval(() => { sweep().catch(() => {}) }, SWEEP_EVERY_MS)
    sweepTimer.unref?.()
    return { s, waited }
  }

  const freePlace = (resource) => { creating.set(resource, creating.get(resource) - 1); wakeFree(resource) }

  // `idle`: the sweep closed it; `by`: who closed it ('run' for run.close). `expected`: close only this object.
  async function closeSession(k, idle = false, by = null, expected = null) {
    const s = sessions.get(k)
    if (!s || (expected && s !== expected)) return
    sessions.delete(k)
    lost.delete(k)
    s.closedBy = by
    if (idle) {
      markLost(k, 'idle')
      log?.emit('WARN', 'worker.browser', 'context closed', [['run_id', s.run], ['resource', s.resource], ['reason', 'idle']])
    }
    // The place is free from now on: the oldest waiter gets it before the context is really closed.
    wakeFree(s.resource)
    await s.ctx.close().catch(() => {})
  }

  // Waiters for a place of one resource, in arrival order. Resolves 'placed' (the place is held for the
  // waiter), 'timeout', 'ended' (its run got run.close) or 'aborted' (the runner closed the socket).
  function waitPlace(resource, run, max, ms, signal) {
    return new Promise((resolve) => {
      const w = { resource, run, max }
      const done = (v) => {
        clearTimeout(t)
        signal?.removeEventListener('abort', onAbort)
        const i = waiters.indexOf(w)
        if (i >= 0) waiters.splice(i, 1)
        resolve(v)
      }
      const onAbort = () => done('aborted')
      // Not unref'd: an op waits on this timer, so it must keep the loop alive until it answers.
      const t = setTimeout(() => done('timeout'), ms)
      signal?.addEventListener('abort', onAbort)
      w.wake = () => done('placed')
      w.cancel = (v) => done(v)
      waiters.push(w)
    })
  }
  // Hands each free place of the resource to the oldest waiter; the place is counted for it at once.
  function wakeFree(resource) {
    for (;;) {
      const w = waiters.find((x) => x.resource === resource)
      if (!w || used(resource) >= w.max) return
      creating.set(resource, (creating.get(resource) ?? 0) + 1)
      w.wake()
    }
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

  async function settleBlocked(page, timeout) {
    try {
      await page.waitForURL((u) => u.protocol === 'chrome-error:', { waitUntil: 'commit', timeout: Math.min(timeout, SETTLE_MAX_MS) })
    } catch { /* the page is gone or stayed put: the reason stands */ }
  }

  // One sweep at a time: a sweep that runs is not started again. A session is closed only if it is
  // still the one in the table and still idle when its turn comes.
  function sweep(idleMs, at = now()) {
    if (sweeping) return sweeping
    const p = (async () => {
      for (const [k, s] of [...sessions]) {
        if (sessions.get(k) !== s) continue
        if (s.busy === 0 && at - s.last >= (idleMs ?? s.idleMs)) await closeSession(k, true, null, s)
      }
    })()
    sweeping = p.finally(() => { sweeping = null })
    return sweeping
  }

  async function execute(s, policy, args, deadlineMs, signal) {
    if (s.closedBy) return { unreadable: true } // closed before this op began
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
      // Not unref'd: the op awaits this deadline; s.cancel clears it when the op ends.
      const t = setTimeout(() => { timedOut = true; res() }, deadlineMs)
      // The runner closed the socket: nobody waits for the answer, stop at once.
      const onAbort = () => { timedOut = true; res() }
      signal?.addEventListener('abort', onAbort)
      s.cancel = () => { clearTimeout(t); signal?.removeEventListener('abort', onAbort) }
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
    // A blocked main-frame navigation leaves Chromium committing its error page after the abort is
    // reported. Wait for that commit, or the next op's first navigation is interrupted by it and
    // fails as navigation-failed. The wait never changes the result: the reason is already decided.
    if (st.navBlocked && !timedOut) await settleBlocked(page, timeout)
    if (timedOut && error === null) { error = 'timeout'; failedAt = Math.min(idx, args.commands.length - 1) }
    let title = ''
    try { title = await page.title() } catch { /* page gone */ }
    s.st = null
    // Remove this context's own cookie values from every string that leaves (only the worker knows them).
    // If they cannot be read (the context is gone), nothing from the page leaves: fail closed.
    let cookies = null
    try { cookies = (await s.ctx.cookies()).map((c) => c.value) } catch { /* context gone */ }
    if (s.closedBy) cookies = null // closed under this op: what the page held may be stale
    const page1 = cookies === null ? null : scrubPage({ url: pathOf(page.url()), title, reads: st.reads }, cookies)
    if (page1 === null) return { unreadable: true }
    return {
      ok: error === null,
      url: page1.url,
      title: page1.title,
      status: st.status,
      reads: page1.reads,
      blocked: st.blocked,
      'failed-at': failedAt,
      error,
    }
  }

  // req is a validated browser.page request. Returns a worker response.
  // `opts.signal`: aborted when the runner closed the socket; the op stops and its place is freed.
  async function handle(req, { signal } = {}) {
    await sweep()
    const opId = req['op-id']
    if (ended.has(req.run)) return refusedEnded(opId)
    const k = key(req.run, req.policy.resource)
    const why = lost.get(k)
    if (why !== undefined && !sessions.has(k)) {
      lost.delete(k)
      // The browser was lost: the worker knows no truer word than `internal`.
      if (why === 'browser') return errorResponse('internal', opId)
      return errorResponse('context-lost', opId, { why, 'idle-ms': req.policy['idle-ms'] ?? DEFAULT_IDLE_MS, 'max-contexts': req.policy['max-contexts'] ?? DEFAULT_MAX_CONTEXTS })
    }
    lost.delete(k)
    const got = await session(req.run, req.policy, req['deadline-ms'], signal)
    if (got.refused) return errorResponse('at-capacity', opId, got.refused)
    if (got.ended) return refusedEnded(opId)
    if (got.aborted) return errorResponse('internal', opId) // nobody reads it
    const { s } = got
    // The wait counts against the op's own deadline.
    const deadlineMs = Math.max(1, req['deadline-ms'] - Math.round(got.waited))
    s.idleMs = req.policy['idle-ms'] ?? DEFAULT_IDLE_MS
    // Ops on one context run one after the other.
    const job = s.queue.then(() => {
      s.last = now()
      return execute(s, req.policy, req.args, deadlineMs, signal)
    })
    s.queue = job.catch(() => {})
    let out
    try { out = await job } finally { s.busy--; s.last = now() }
    if (out.unreadable) {
      // The page data cannot be scrubbed, so none leaves. run.close closed it: the run has ended.
      return s.closedBy === 'run' ? refusedEnded(opId) : errorResponse('internal', opId)
    }
    return { v: VERSION, 'op-id': opId, ok: true, out }
  }

  return {
    handle,
    size: () => sessions.size,
    has: (run, resource) => sessions.has(key(run, resource)),
    closeContext: (run, resource) => closeSession(key(run, resource)),
    // Closes every context of the run; returns how many.
    async closeRun(run) {
      ended.add(run)
      for (const w of [...waiters]) if (w.run === run) w.cancel('ended')
      let n = 0
      for (const [k, s] of [...sessions]) if (s.run === run) { s.closedBy = 'run'; await closeSession(k, false, 'run'); n++ }
      for (const k of [...lost.keys()]) if (JSON.parse(k)[0] === run) lost.delete(k)
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
      if (p) await p.then((b) => b.close()).catch(() => {})
    },
  }
}
