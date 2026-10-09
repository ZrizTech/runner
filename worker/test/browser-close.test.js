import test from 'node:test'
import assert from 'node:assert/strict'
import { createBrowserHost } from '../src/browser.js'
import { validateResponse } from '../src/schema.js'
import { fakeBrowser, req, sleep } from './fake-browser.js'

const hang = [{ do: 'read', what: 'title', as: 't' }, { do: 'wait-for', url: '/never' }]

test('J1: run.close under a running op: no page data leaves, the answer is an error', async () => {
  const f = fakeBrowser()
  const host = createBrowserHost({ launch: f.launch })
  const op = host.handle(req(1, 'a', { commands: hang }))
  await sleep(30)
  await host.closeRun('a')
  const r = await op
  assert.equal(JSON.stringify(r).includes('SESSIONVALUE99'), false, JSON.stringify(r))
  assert.equal(r.ok, false)
  assert.equal(r.reason, 'context-lost')
  assert.equal(r.why, 'run-closed')
  assert.equal(validateResponse(r), true, JSON.stringify(validateResponse.errors))
  await host.closeAll()
})

test('J1: cookies unreadable and not closed by run.close: internal, no page data', async () => {
  const f = fakeBrowser()
  const host = createBrowserHost({ launch: f.launch })
  const op = host.handle(req(1, 'a', { commands: hang }))
  await sleep(30)
  await f.ctxs[0].close() // the context dies by itself
  const r = await op
  assert.equal(JSON.stringify(r).includes('SESSIONVALUE99'), false)
  assert.equal(r.reason, 'internal')
  await host.closeAll()
})

test('J3: a new op of an ended run is refused at once and makes nothing', async () => {
  const f = fakeBrowser()
  const host = createBrowserHost({ launch: f.launch })
  await host.closeRun('a')
  const r = await host.handle(req(1, 'a'))
  assert.equal(r.reason, 'context-lost')
  assert.equal(r.why, 'run-closed')
  assert.equal(f.log.opened, 0)
  assert.equal(host.size(), 0)
  await host.closeAll()
})

test('J3: a context that is being made when the run ends is closed and its place freed', async () => {
  const f = fakeBrowser({ newDelay: 60 })
  const host = createBrowserHost({ launch: f.launch })
  const op = host.handle(req(1, 'a', { max: 1 }))
  await sleep(20)
  await host.closeRun('a')
  const r = await op
  assert.equal(r.why, 'run-closed', JSON.stringify(r))
  assert.equal(host.size(), 0)
  assert.deepEqual(host.stats(), [{ resource: 'web', busy: 0 }])
  assert.deepEqual(f.log.closed, [1])
  await host.closeAll()
})

test('J3: a waiter of an ended run is removed and refused', async () => {
  const f = fakeBrowser()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 3000 })
  await host.handle(req(0, 'x', { max: 1 }))
  const t0 = Date.now()
  const w = host.handle(req(1, 'a', { max: 1, deadline: 9000 }))
  await sleep(20)
  await host.closeRun('a')
  const r = await w
  assert.equal(r.why, 'run-closed', JSON.stringify(r))
  assert.ok(Date.now() - t0 < 1000)
  assert.equal(host.has('a', 'web'), false)
  await host.closeAll()
})

test('J4: the oldest waiter gets the freed place at once; a new op does not jump the line', async () => {
  const f = fakeBrowser({ closeDelay: 80 })
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 3000 })
  await host.handle(req(0, 'x', { max: 1 }))
  const order = []
  const w1 = host.handle(req(1, 'b', { max: 1 })).then((r) => { order.push('b'); return r })
  await sleep(10)
  const closing = host.closeRun('x')
  await sleep(10) // closeSession awaits ctx.close(); the place is already free
  const late = host.handle(req(2, 'c', { max: 1, deadline: 400 })).then((r) => { order.push('c'); return r })
  await closing
  const [rb, rc] = await Promise.all([w1, late])
  assert.equal(rb.ok, true, JSON.stringify(rb))
  assert.equal(rc.reason, 'at-capacity')
  assert.equal(rc.busy, 1)
  assert.equal(order[0], 'b')
  await host.closeAll()
})

test('J5: two overlapping sweeps close each session once and never a new session of the same key', async () => {
  const f = fakeBrowser({ closeDelay: 40 })
  let t = 1000
  const host = createBrowserHost({ launch: f.launch, now: () => t })
  await host.handle(req(0, 'x', { idle: 100 }))
  await host.handle(req(1, 'y', { idle: 100 }))
  t += 500
  const s1 = host.sweep()
  const s2 = host.sweep()
  assert.equal(s1, s2, 'a second sweep started while one runs')
  await sleep(10)
  const first = await host.handle(req(2, 'y', { idle: 100 })) // waits for the sweep; y was lost as idle
  assert.equal(first.reason, 'context-lost')
  const busy = host.handle(req(3, 'y', { idle: 100, commands: [{ do: 'goto', path: '/slow-150' }] }))
  await Promise.all([s1, s2])
  await sleep(10)
  assert.equal(host.has('y', 'web'), true, 'the new session of y was closed by a stale sweep')
  assert.equal((await busy).ok, true)
  assert.deepEqual(f.log.closed.slice().sort(), [1, 2])
  await host.closeAll()
})

test('J6: a failed launch is tried again by the next op', async () => {
  const f = fakeBrowser({ launchFails: 1 })
  const host = createBrowserHost({ launch: f.launch })
  await assert.rejects(host.handle(req(1, 'a')))
  assert.equal((await host.handle(req(2, 'a'))).ok, true)
  await host.closeAll()
})

test('J6: a browser that disconnected is launched again; the lost sessions answer internal', async () => {
  const f = fakeBrowser()
  const host = createBrowserHost({ launch: f.launch })
  assert.equal((await host.handle(req(1, 'a'))).ok, true)
  f.disconnect()
  assert.equal(host.size(), 0)
  const r = await host.handle(req(2, 'a'))
  assert.equal(r.reason, 'internal')
  assert.equal((await host.handle(req(3, 'a'))).ok, true)
  assert.equal(f.log.launches, 2)
  await host.closeAll()
})

test('J6: two ops of one new (run, resource) at once make one context', async () => {
  const f = fakeBrowser({ newDelay: 30 })
  const host = createBrowserHost({ launch: f.launch })
  const rs = await Promise.all([host.handle(req(1, 'a')), host.handle(req(2, 'a'))])
  assert.ok(rs.every((r) => r.ok), JSON.stringify(rs))
  assert.equal(f.log.opened, 1)
  assert.equal(host.size(), 1)
  await host.closeAll()
})

test('J2: the wait bound is min(max(0, deadline - 1000), 5000); a bound of 0 answers at once', async () => {
  const f = fakeBrowser()
  const host = createBrowserHost({ launch: f.launch })
  await host.handle(req(0, 'x', { max: 1 }))
  const t0 = Date.now()
  const r = await host.handle(req(1, 'b', { max: 1, deadline: 900 }))
  assert.equal(r.reason, 'at-capacity')
  assert.ok(Date.now() - t0 < 100, `waited ${Date.now() - t0} ms`)
  const t1 = Date.now()
  const r2 = await host.handle(req(2, 'c', { max: 1, deadline: 1300 }))
  assert.equal(r2.reason, 'at-capacity')
  assert.ok(Date.now() - t1 >= 250 && Date.now() - t1 < 700, `waited ${Date.now() - t1} ms`)
  await host.closeAll()
})

test('J2: a closed socket removes the waiter', async () => {
  const f = fakeBrowser()
  const host = createBrowserHost({ launch: f.launch, maxWaitMs: 3000 })
  await host.handle(req(0, 'x', { max: 1 }))
  const ac = new AbortController()
  const w = host.handle(req(1, 'b', { max: 1, deadline: 9000 }), { signal: ac.signal })
  await sleep(20)
  ac.abort()
  await w
  await host.closeRun('x')
  await sleep(20)
  assert.equal(host.has('b', 'web'), false, 'a dead waiter took a place')
  assert.deepEqual(host.stats(), [{ resource: 'web', busy: 0 }])
  await host.closeAll()
})
