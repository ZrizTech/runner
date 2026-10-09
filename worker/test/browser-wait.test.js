import test from 'node:test'
import assert from 'node:assert/strict'
import { createBrowserHost, MAX_LOST } from '../src/browser.js'
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

const reqR = (n, run, resource, max, deadline = 5000) => {
  const r = req(n, run, max, deadline)
  r.policy.resource = resource
  return r
}
const slow = (r, ms) => { r.args.commands = [{ do: 'goto', path: `/slow-${ms}` }]; return r }

test('the order: a free place, then a wait, then at-capacity; nothing is closed to make room', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 80 })
  assert.equal((await host.handle(req(0, 'a', 1))).ok, true) // free place
  const res = await host.handle(req(1, 'b', 1)) // the place of a is idle and still in use
  assert.equal(res.reason, 'at-capacity')
  assert.equal(res.busy, 1)
  assert.ok(res['waited-ms'] >= 50, JSON.stringify(res)) // it waited first
  assert.deepEqual(f.log.closed, [])
  assert.equal(host.has('a', 'web'), true)
  assert.equal((await host.handle(req(2, 'a', 1))).ok, true) // a keeps its context
  await host.closeAll()
})

test('a waiter gets the place when run.close arrives inside the wait; first come first served', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 2000 })
  await host.handle(req(0, 'a', 1))
  const order = []
  const w1 = host.handle(req(1, 'b', 1)).then((r) => { order.push('b'); return r })
  await new Promise((r) => setTimeout(r, 20))
  const w2 = host.handle(req(2, 'c', 1, 150)).then((r) => { order.push('c'); return r })
  await new Promise((r) => setTimeout(r, 20))
  assert.equal(await host.closeRun('a'), 1)
  const [r1, r2] = await Promise.all([w1, w2])
  assert.equal(r1.ok, true, JSON.stringify(r1))
  assert.equal(r2.reason, 'at-capacity') // b took the only place
  assert.deepEqual(order[0], 'b')
  await host.closeAll()
})

test('one limit for each resource: a full resource does not refuse the other', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 50 })
  for (const run of ['a', 'b']) assert.equal((await host.handle(reqR(run, run, 'first', 2))).ok, true)
  assert.equal((await host.handle(reqR('c', 'c', 'first', 2))).reason, 'at-capacity')
  for (const run of ['a', 'b', 'c']) assert.equal((await host.handle(reqR(`s${run}`, run, 'second', 3))).ok, true)
  const d = await host.handle(reqR('sd', 'd', 'second', 3))
  assert.equal(d.reason, 'at-capacity')
  assert.equal(d['max-contexts'], 3)
  assert.equal(d.busy, 3)
  assert.deepEqual(host.stats(), [{ resource: 'first', busy: 2 }, { resource: 'second', busy: 3 }])
  await host.closeAll()
})

test('context-lost with why idle exactly one time after the sweep; run.close forgets it', async () => {
  const f = fakeLaunch()
  let t = 1000
  const lines = []
  const host = createBrowserHost({ launch: f.launch, now: () => t, log: { emit: (...a) => lines.push(a) } })
  const idle = (n, run) => { const r = req(n, run, 3); r.policy['idle-ms'] = 1000; return r }
  await host.handle(idle(0, 'a'))
  await host.handle(idle(1, 'b'))
  t += 2000
  await host.sweep()
  assert.equal(host.size(), 0)
  assert.deepEqual(lines[0], ['WARN', 'worker.browser', 'context closed', [['run_id', 'a'], ['resource', 'web'], ['reason', 'idle']]])
  const lost = await host.handle(idle(2, 'a'))
  assert.equal(lost.reason, 'context-lost')
  assert.equal(lost.why, 'idle')
  assert.equal(validateResponse(lost), true, JSON.stringify(validateResponse.errors))
  assert.equal((await host.handle(idle(3, 'a'))).ok, true) // only one time
  await host.closeRun('b') // forgets the entry
  assert.equal((await host.handle(idle(4, 'b'))).ok, true)
  await host.closeAll()
})

test('the memory of lost contexts has a bound', async () => {
  const f = fakeLaunch()
  let t = 1000
  const host = createBrowserHost({ launch: f.launch, now: () => t })
  const idle = (n, run) => { const r = req(n, run, MAX_LOST + 5); r.policy['idle-ms'] = 1000; return r }
  for (let i = 0; i < MAX_LOST + 2; i++) await host.handle(idle(i, `r${i}`))
  t += 2000
  await host.sweep()
  assert.equal((await host.handle(idle(9000, 'r0'))).ok, true) // the oldest entry was forgotten
  assert.equal((await host.handle(idle(9001, `r${MAX_LOST + 1}`))).reason, 'context-lost')
  await host.closeAll()
})

test('closeRun closes the contexts of one run only; a failed creation frees its place', async () => {
  const f = fakeLaunch()
  const host = createBrowserHost({ launch: f.launch })
  await host.handle(req(0, 'a', 2))
  await host.handle(req(1, 'b', 2))
  assert.equal(await host.closeRun('a'), 1)
  assert.equal(await host.closeRun('nobody'), 0)
  assert.equal(host.has('b', 'web'), true)
  assert.equal((await host.handle(req(2, 'a', 2))).ok, true) // run.close forgot nothing to report
  await host.closeAll()

  const bad = fakeLaunch({ failNew: true })
  const h2 = createBrowserHost({ launch: bad.launch, maxWaitMs: 50 })
  await assert.rejects(h2.handle(req(3, 'x', 1)))
  await assert.rejects(h2.handle(req(4, 'y', 1))) // not at-capacity: the place is back
  await h2.closeAll()
})
