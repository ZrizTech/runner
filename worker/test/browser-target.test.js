import test from 'node:test'
import assert from 'node:assert/strict'
import { createBrowserHost } from '../src/browser.js'
import { validateResponse } from '../src/schema.js'
import { req, sleep } from './fake-browser.js'

// Time budget rules (spec 8.1). The clock is injected: the fake browser moves it, nothing sleeps for
// long. D = 200 ms of waiting for the target, S = 1000 ms of get-ready or later work.
const D = 200
const S = 1000
const passes = (t) => D <= t && t < D + S / 2

// A goto to `/slow-N` takes N ms on the clock; `/fail-N` takes N ms, then fails. A new context
// costs S ms and `title()` costs S ms, on the same clock.
function setup() {
  const clock = { t: 5000.5 }
  const page = {
    setDefaultTimeout() {}, setDefaultNavigationTimeout() {}, on() {},
    async goto(href) {
      await sleep(5) // lets a second op queue behind this one
      const m = /(slow|fail)-(\d+)/.exec(href)
      clock.t += Number(m[2])
      if (m[1] === 'fail') throw new Error('net::ERR_FAILED')
      return null
    },
    waitForURL: () => new Promise(() => {}),
    mainFrame: () => ({}), url: () => 'about:blank', title: async () => { clock.t += S; return 'x' },
    async close() {},
  }
  const browser = {
    on() {},
    async newContext() {
      clock.t += S
      return { route: async () => {}, routeWebSocket: async () => {}, on() {}, newPage: async () => page, cookies: async () => [], async close() {} }
    },
    async close() {},
  }
  const host = createBrowserHost({ launch: async () => browser, mono: () => clock.t })
  let n = 0
  const go = async (commands) => {
    const r = await host.handle(req(++n, 'run-1', { commands }))
    assert.equal(validateResponse(r), true, JSON.stringify(validateResponse.errors))
    return r
  }
  return { host, go, clock }
}
const goto = (path) => ({ do: 'goto', path })

test('R1: queue wait, new context and later work are out of target-ms', async () => {
  const { host, go } = setup()
  const [a, b] = await Promise.all([go([goto(`/slow-${S}`)]), go([goto('/slow-100'), goto('/slow-100')])])
  assert.equal(a['target-ms'], S) // the new context (S) and the title (S) are out
  assert.ok(passes(b['target-ms']), `got ${b['target-ms']}`) // B waited S behind A
  assert.equal(b['target-ms'], D)
  await host.closeAll()
})

test('R2: the same action two times in a row gives the same number', async () => {
  const { host, go } = setup()
  const t1 = (await go([goto(`/slow-${D}`)]))['target-ms']
  const t2 = (await go([goto(`/slow-${D}`)]))['target-ms']
  assert.ok(passes(t1) && passes(t2), `got ${t1}, ${t2}`)
  await host.closeAll()
})

test('a failed command: ok false, and target-ms runs to the end of the failed command', async () => {
  const { host, go } = setup()
  const r = await go([goto('/slow-50'), goto('/fail-150'), goto('/slow-999')])
  assert.equal(r.ok, true)
  assert.equal(r.out.ok, false)
  assert.equal(r.out['failed-at'], 1)
  assert.equal(r['target-ms'], 200)
  await host.closeAll()
})

test('work after the command loop is not in the number; the number is a whole, cut integer', async () => {
  const { host, go, clock } = setup()
  const r = await go([goto('/slow-7')])
  assert.equal(r['target-ms'], 7)
  assert.ok(clock.t - 5000.5 >= 2 * S + 7) // the context and the title did move the clock
  assert.ok(Number.isInteger(r['target-ms']))
  await host.closeAll()
})

test('an op with no command has target-ms 0', async () => {
  const { host, go } = setup()
  assert.equal((await go([]))['target-ms'], 0)
  await host.closeAll()
})
