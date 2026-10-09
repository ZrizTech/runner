import test from 'node:test'
import assert from 'node:assert/strict'
import { createBrowserHost } from '../src/browser.js'
import { validateResponse } from '../src/schema.js'

// Fake Chromium. A page hangs in `goto` while `gate` is set, so a context stays busy.
function fakeLaunch({ failNew = false } = {}) {
  const log = { closed: [], opened: 0 }
  const gates = new Map()
  const browser = {
    async newContext() {
      if (failNew) throw new Error('boom')
      log.opened++
      const id = log.opened
      const page = {
        setDefaultTimeout() {}, setDefaultNavigationTimeout() {}, on() {},
        async goto(href) {
          const m = /slow-(\d+)/.exec(href)
          if (m) await new Promise((r) => setTimeout(r, Number(m[1])))
          return null
        },
        mainFrame: () => ({}), url: () => 'about:blank', title: async () => '',
        async close() {},
      }
      const ctx = {
        id, route: async () => {}, routeWebSocket: async () => {}, on() {},
        newPage: async () => page, cookies: async () => [],
        async close() { log.closed.push(id) },
      }
      return ctx
    },
    async close() {},
  }
  return { launch: async () => browser, log, gates }
}

const req = (n, run, max, deadline = 5000) => ({
  v: 1, 'op-id': `op-${n}`, run, kind: 'browser.page',
  policy: { resource: 'web', 'base-url': 'http://x.test', origins: ['http://x.test'], 'max-contexts': max },
  args: { commands: [], 'command-timeout-ms': 1000 }, 'deadline-ms': deadline,
})

test('all busy: the op waits and succeeds when one ends', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 2000 })
  // Two runs fill the host; their contexts stay busy while `busy` is set by an in-flight op.
  const busyOps = ['a', 'b'].map((run, i) => {
    const r = req(i, run, 2, 300)
    r.args.commands = [{ do: 'goto', path: '/slow-150' }]
    return host.handle(r)
  })
  await new Promise((r) => setTimeout(r, 30))
  const t0 = Date.now()
  const res = await host.handle(req(9, 'c', 2, 3000))
  assert.equal(res.ok, true, JSON.stringify(res))
  assert.ok(Date.now() - t0 < 1500)
  await Promise.all(busyOps)
  assert.equal(host.size(), 2)
  await host.closeAll()
})

test('all busy past the bound: at-capacity with the three numbers', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 80 })
  const busyOps = ['a', 'b'].map((run, i) => {
    const r = req(i, run, 2, 600)
    r.args.commands = [{ do: 'goto', path: '/slow-400' }]
    return host.handle(r)
  })
  await new Promise((r) => setTimeout(r, 30))
  const res = await host.handle(req(9, 'c', 2, 3000))
  assert.equal(res.ok, false)
  assert.equal(res.reason, 'at-capacity')
  assert.equal(res['max-contexts'], 2)
  assert.equal(res.busy, 2)
  assert.ok(Number.isInteger(res['waited-ms']) && res['waited-ms'] >= 50, JSON.stringify(res))
  assert.equal(validateResponse(res), true, JSON.stringify(validateResponse.errors))
  await Promise.all(busyOps)
  await host.closeAll()
})

test('the wait is cut short by the op deadline', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 5000 })
  const busyOps = ['a'].map((run, i) => {
    const r = req(i, run, 1, 800)
    r.args.commands = [{ do: 'goto', path: '/slow-500' }]
    return host.handle(r)
  })
  await new Promise((r) => setTimeout(r, 30))
  const t0 = Date.now()
  const res = await host.handle(req(9, 'c', 1, 100))
  assert.equal(res.reason, 'at-capacity')
  assert.ok(Date.now() - t0 < 400)
  await Promise.all(busyOps)
  await host.closeAll()
})

test('an evicted live run gets context-lost once, then a fresh context; no context closes twice', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch })
  assert.equal((await host.handle(req(0, 'a', 1))).ok, true)
  assert.equal((await host.handle(req(1, 'b', 1))).ok, true) // evicts a
  assert.equal(host.has('a', 'web'), false)
  const lost = await host.handle(req(2, 'a', 1))
  assert.equal(lost.ok, false)
  assert.equal(lost.reason, 'context-lost')
  assert.equal(lost['max-contexts'], 1)
  assert.equal(validateResponse(lost), true, JSON.stringify(validateResponse.errors))
  assert.equal((await host.handle(req(3, 'a', 1))).ok, true) // evicts b
  assert.equal((await host.handle(req(4, 'a', 1))).ok, true)
  await host.closeAll()
  assert.equal(new Set(f.log.closed).size, f.log.closed.length)
})

test('closeRun forgets a lost context; a failed creation frees its slot', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch })
  await host.handle(req(0, 'a', 1))
  await host.handle(req(1, 'b', 1))
  await host.closeRun('a')
  assert.equal((await host.handle(req(2, 'a', 1))).ok, true)
  await host.closeAll()

  const bad = fakeLaunch({ failNew: true })
  const h2 = createBrowserHost({ launch: bad.launch, maxWaitMs: 50 })
  await assert.rejects(h2.handle(req(3, 'x', 1)))
  await assert.rejects(h2.handle(req(4, 'y', 1))) // not at-capacity: the slot is back
  await h2.closeAll()
})
