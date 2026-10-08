import { chromium } from 'playwright'
import { VERSION } from './protocol.js'
import { classify, pathOf, runCommand, CommandError } from './commands.js'
import { errorResponse } from './protocol.js'

const DEFAULT_TIMEOUT_MS = 10000
const DEFAULT_VIEWPORT = { width: 1280, height: 800 }
const DEFAULT_MAX_CONTEXTS = 3
const DEFAULT_IDLE_MS = 10 * 60 * 1000
const SWEEP_EVERY_MS = 60 * 1000
const LOCAL_SCHEMES = new Set(['data:', 'blob:', 'about:'])

const wsToHttp = (u) => u.replace(/^ws(s?):/, 'http$1:')

// One Chromium per worker, launched lazily. One context (one page) per (run, resource),
// kept across ops of the same run. Every request of the page is checked against the
// policy origins. At most `policy.max-contexts` live contexts: at the cap the least recently used idle one is closed for the new one, `at-capacity` only if all are busy; a context
// idle longer than `policy.idle-ms` is closed by a sweep on each request and by an unref'd
// timer. `now` is injectable for tests.
export function createBrowserHost({ launch = () => chromium.launch({ headless: true }), now = Date.now } = {}) {
  let browserP = null
  let creating = 0
  let sweepTimer = null
  const sessions = new Map() // key -> { ctx, page, last, queue, st }
  const key = (run, resource) => JSON.stringify([run, resource])

  const browser = () => (browserP ??= launch())

  async function session(run, policy) {
    const k = key(run, policy.resource)
    let s = sessions.get(k)
    if (s) { s.busy++; return s }
    if (sessions.size + creating >= (policy['max-contexts'] ?? DEFAULT_MAX_CONTEXTS)) {
      // At the cap: reuse the slot of the least recently used context with no op in flight.
      let lru = null
      for (const [ek, e] of sessions) if (e.busy === 0 && (lru === null || e.last < lru[1].last)) lru = [ek, e]
      if (lru === null) return null
      await closeSession(lru[0])
      if (sessions.size + creating >= (policy['max-contexts'] ?? DEFAULT_MAX_CONTEXTS)) return null
      s = sessions.get(k)
      if (s) { s.busy++; return s }
    }
    creating++
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
    s = { ctx, page: null, run, last: now(), idleMs: DEFAULT_IDLE_MS, allowed: new Set(), busy: 1, queue: Promise.resolve(), st: null }
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
      creating--
    }
    sweepTimer ??= setInterval(() => { sweep().catch(() => {}) }, SWEEP_EVERY_MS)
    sweepTimer.unref?.()
    return s
  }

  async function closeSession(k) {
    const s = sessions.get(k)
    if (!s) return
    sessions.delete(k)
    await s.ctx.close().catch(() => {})
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
      if (s.busy === 0 && at - s.last >= (idleMs ?? s.idleMs)) await closeSession(k)
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
    const s = await session(req.run, req.policy)
    if (s === null) return errorResponse('at-capacity', req['op-id'])
    s.idleMs = req.policy['idle-ms'] ?? DEFAULT_IDLE_MS
    // Ops on one context run one after the other.
    const job = s.queue.then(() => {
      s.last = now()
      return execute(s, req.policy, req.args, req['deadline-ms'])
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
    async closeRun(run) {
      for (const [k, s] of [...sessions]) if (s.run === run) await closeSession(k)
    },
    sweep,
    async closeAll() {
      if (sweepTimer) clearInterval(sweepTimer)
      sweepTimer = null
      for (const k of [...sessions.keys()]) await closeSession(k)
      const p = browserP
      browserP = null
      if (p) await (await p).close().catch(() => {})
    },
  }
}
