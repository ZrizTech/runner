// Fake Chromium for host tests. The page title holds the cookie value; `wait-for` url hangs until
// the context closes, then rejects like Playwright does.
export function fakeBrowser({ newDelay = 0, closeDelay = 0, cookie = 'SESSIONVALUE99', launchFails = 0 } = {}) {
  const log = { closed: [], opened: 0, launches: 0 }
  const ctxs = []
  const listeners = {}
  const browser = {
    on(ev, fn) { listeners[ev] = fn },
    async newContext() {
      if (newDelay) await new Promise((r) => setTimeout(r, newDelay))
      log.opened++
      const id = log.opened
      let closed = false
      const waits = []
      const page = {
        setDefaultTimeout() {}, setDefaultNavigationTimeout() {}, on() {},
        async goto(href) {
          const m = /slow-(\d+)/.exec(href)
          if (m) await new Promise((r) => setTimeout(r, Number(m[1])))
          if (closed) throw new Error('Target closed')
          return null
        },
        waitForURL() { return new Promise((_, rej) => waits.push(rej)) },
        mainFrame: () => ({}), url: () => 'about:blank', title: async () => `page ${cookie}`,
        async close() {},
      }
      const ctx = {
        id, route: async () => {}, routeWebSocket: async () => {}, on() {},
        newPage: async () => page,
        cookies: async () => { if (closed) throw new Error('Target closed'); return [{ value: cookie }] },
        async close() {
          closed = true
          for (const w of waits) w(new Error('Target closed'))
          if (closeDelay) await new Promise((r) => setTimeout(r, closeDelay))
          log.closed.push(id)
        },
      }
      ctxs.push(ctx)
      return ctx
    },
    async close() {},
  }
  return {
    launch: async () => { log.launches++; if (log.launches <= launchFails) throw new Error('no browser'); return browser },
    log, ctxs, disconnect: () => listeners.disconnected?.(),
  }
}

export const req = (n, run, { max = 3, deadline = 5000, resource = 'web', commands = [], idle } = {}) => ({
  v: 1, 'op-id': `op-${n}`, run, kind: 'browser.page',
  policy: { resource, 'base-url': 'http://x.test', origins: ['http://x.test'], 'max-contexts': max, ...(idle ? { 'idle-ms': idle } : {}) },
  args: { commands, 'command-timeout-ms': 1000 }, 'deadline-ms': deadline,
})
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
