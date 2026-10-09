import test from 'node:test'
import assert from 'node:assert/strict'
import { createBrowserHost } from '../src/browser.js'

// Fake Chromium: async newContext / close with a delay, so the race window is open.
function fakeLaunch(delayMs) {
  const sleep = () => new Promise((r) => setTimeout(r, delayMs))
  const log = { closed: [], opened: 0 }
  const browser = {
    async newContext() {
      await sleep()
      log.opened++
      const page = {
        setDefaultTimeout() {}, setDefaultNavigationTimeout() {}, on() {},
        mainFrame: () => ({}), url: () => 'about:blank', title: async () => '',
        async close() {},
      }
      const ctx = {
        id: log.opened,
        route: async () => {}, routeWebSocket: async () => {}, on() {},
        newPage: async () => page, cookies: async () => [],
        async close() { await sleep(); log.closed.push(ctx.id) },
      }
      return ctx
    },
    async close() {},
  }
  return { launch: async () => browser, log }
}

const req = (n, run, max) => ({
  v: 1, 'op-id': `op-${n}`, run, kind: 'browser.page',
  policy: { resource: 'web', 'base-url': 'http://x.test', origins: ['http://x.test'], 'max-contexts': max },
  args: { commands: [], 'command-timeout-ms': 1000 }, 'deadline-ms': 5000,
})

test('capacity: N concurrent first ops of new runs at a full host of idle contexts are not refused', async () => {
  const max = 8
  let t = 1000
  const { launch, log } = fakeLaunch(20)
  const host = createBrowserHost({ launch, now: () => ++t })
  // Fill with idle contexts of runs that ended.
  for (let i = 0; i < max; i++) assert.equal((await host.handle(req(i, `old-${i}`, max))).ok, true)
  assert.equal(host.size(), max)
  // 8 new runs start in the same moment.
  const res = await Promise.all(Array.from({ length: max }, (_, i) => host.handle(req(100 + i, `new-${i}`, max))))
  const refused = res.map((r, i) => (r.ok ? null : `new-${i}:${r.error?.code ?? r.error ?? JSON.stringify(r)}`)).filter(Boolean)
  console.log('refused:', refused, 'closed ctx ids:', log.closed, 'size:', host.size())
  assert.deepEqual(refused, [], 'refused with idle capacity available')
  assert.equal(new Set(log.closed).size, log.closed.length, 'a context was closed twice')
  await host.closeAll()
})
