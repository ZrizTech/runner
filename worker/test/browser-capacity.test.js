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

test('capacity: 8 runs at once with limit 8 lose no context; a ninth gets at-capacity with the numbers', async () => {
  const max = 8
  const { launch, log } = fakeLaunch(20)
  const host = createBrowserHost({ launch, maxWaitMs: 100 })
  const res = await Promise.all(Array.from({ length: max + 1 }, (_, i) => host.handle(req(i, `run-${i}`, max))))
  assert.equal(res.filter((r) => r.ok).length, max)
  const refused = res.find((r) => !r.ok)
  assert.equal(refused.reason, 'at-capacity')
  assert.equal(refused['max-contexts'], max)
  assert.equal(refused.busy, max)
  assert.ok(Number.isInteger(refused['waited-ms']))
  assert.deepEqual(log.closed, [], 'a context was closed')
  assert.equal(log.opened, max, 'two ops took the same last place')
  assert.equal(host.size(), max)
  await host.closeAll()
})
