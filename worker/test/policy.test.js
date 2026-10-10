import test, { before, after } from 'node:test'
import assert from 'node:assert/strict'
import { createBrowserHost } from '../src/browser.js'
import { validateRequest, validateResponse } from '../src/schema.js'
import { startSite } from './fixture-site.js'

let a
let b
let host
let clock = 1_000_000
let n = 0

before(async () => {
  a = await startSite()
  b = await startSite()
  host = createBrowserHost({ now: () => clock, maxWaitMs: 100 })
})
after(async () => {
  await host.closeAll()
  await a.close()
  await b.close()
})

const req = (commands, { run = 'p-1', resource = 'web', origins = [a.origin], policy = {} } = {}) => ({
  v: 1,
  'op-id': `pop-${++n}`,
  run,
  kind: 'browser.page',
  policy: { resource, 'base-url': a.origin, origins, ...policy },
  args: { commands, 'command-timeout-ms': 3000 },
  'deadline-ms': 30000,
})

async function exec(commands, opts) {
  const r = req(commands, opts)
  assert.equal(validateRequest(r), true, JSON.stringify(validateRequest.errors))
  const resp = await host.handle(r)
  assert.equal(validateResponse(resp), true, JSON.stringify(validateResponse.errors))
  return resp
}
async function out(commands, opts) {
  const resp = await exec(commands, opts)
  assert.equal(resp.ok, true, JSON.stringify(resp))
  return resp.out
}

const goto = (path) => ({ do: 'goto', path })
const css = (c) => ({ css: c })
const settled = { do: 'wait-for', target: css('#r:not(:empty)'), state: 'attached' }
const readR = { do: 'read', as: 'r', what: 'text', target: css('#r') }
const probe = (kind, o) => [goto(`/${kind}?o=${encodeURIComponent(o)}`), settled, readR]

test('off-origin fetch is blocked and counted', async () => {
  const o = await out(probe('x-fetch', b.origin))
  assert.equal(o.ok, true, JSON.stringify(o))
  assert.equal(o.reads.r, 'err')
  assert.equal(o.blocked, 1)
})

test('off-origin img and img redirect are blocked and counted', async () => {
  const o = await out(probe('x-img', b.origin))
  assert.equal(o.reads.r, 'err')
  assert.equal(o.blocked, 1)
  const o2 = await out(probe('x-imgredir', b.origin))
  assert.equal(o2.reads.r, 'err')
  assert.equal(o2.blocked, 1)
})

test('off-origin iframe is blocked and counted', async () => {
  const o = await out([goto(`/x-iframe?o=${encodeURIComponent(b.origin)}`), { do: 'wait-for', target: css('iframe'), state: 'attached' }])
  assert.equal(o.ok, true, JSON.stringify(o))
  assert.equal(o.blocked, 1)
})

test('same-origin traffic is not counted', async () => {
  const o = await out(probe('x-fetch', a.origin))
  assert.equal(o.reads.r, 'ok')
  assert.equal(o.blocked, 0)
})

test('an extra origin in policy.origins is allowed', async () => {
  const o = await out(probe('x-fetch', b.origin), { origins: [a.origin, b.origin], run: 'p-extra' })
  assert.equal(o.reads.r, 'ok')
  assert.equal(o.blocked, 0)
  const ws = await out(probe('x-ws', b.origin), { origins: [a.origin, b.origin], run: 'p-extra' })
  assert.equal(ws.blocked, 0)
  // a redirect to an allowed origin is followed
  const rd = await out([goto(`/redir?to=${encodeURIComponent(b.origin + '/ok')}`), { do: 'read', as: 'u', what: 'title' }], { origins: [a.origin, b.origin], run: 'p-extra' })
  assert.equal(rd.ok, true, JSON.stringify(rd))
  assert.equal(rd.status, 200)
  assert.equal(rd.blocked, 0)
})

test('main-frame redirect off-origin fails with host-not-allowed', async () => {
  const o = await out([goto(`/redir?to=${encodeURIComponent(b.origin + '/ok')}`)])
  assert.equal(o.ok, false)
  assert.equal(o.error, 'host-not-allowed')
  assert.equal(o['failed-at'], 0)
  assert.equal(o.blocked, 1)
  // the op ends only when the page has settled on the browser's error page, not on a stale page
  assert.equal(o.url, '')
  // the context still works
  const o2 = await out([goto('/')])
  assert.equal(o2.ok, true)
})

test('click on an off-origin link fails with host-not-allowed', async () => {
  const o = await out([goto(`/x-link?o=${encodeURIComponent(b.origin)}`), { do: 'click', target: { role: 'link', name: 'off' } }])
  assert.equal(o.ok, false)
  assert.equal(o.error, 'host-not-allowed')
  assert.equal(o['failed-at'], 1)
  assert.equal(o.blocked, 1)
  assert.equal(o.url, '')
  // the next op's first navigation is not interrupted by the error page of the blocked one
  const o2 = await out([goto('/')])
  assert.equal(o2.ok, true, JSON.stringify(o2))
})

test('WebSocket to another origin is blocked and counted', async () => {
  const o = await out(probe('x-ws', b.origin))
  assert.equal(o.reads.r, 'closed')
  assert.equal(o.blocked, 1)
})

test('a cookie value a page prints into its DOM is replaced by [cookie]', async () => {
  const o = await out([goto('/x-cookie-dom'), readR])
  assert.equal(o.reads.r, 'sid=[cookie]')
  assert.equal(JSON.stringify(o).includes('abc123'), false)
  // no header value is ever part of `out`
  assert.deepEqual(Object.keys(o).sort(), ['blocked', 'error', 'failed-at', 'ok', 'reads', 'status', 'title', 'url'])
})

test('a short cookie value is scrubbed as a whole token only, never inside a word', async () => {
  const o = await out([goto('/x-cookie-short'), readR])
  assert.equal(o.reads.r, '[cookie] cab [cookie], [cookie]')
})

test('a full resource refuses a 4th run; no context is closed to make room', async () => {
  await host.closeAll()
  const policy = { 'max-contexts': 3 }
  await out([goto('/cookie-set')], { run: 'e1', policy })
  for (const r of ['e2', 'e3']) await out([goto('/')], { run: r, policy })
  const resp = await exec([goto('/')], { run: 'e4', policy })
  assert.equal(resp.reason, 'at-capacity')
  assert.equal(resp.busy, 3)
  assert.equal(host.size(), 3)
  // the run in work keeps its context and its cookie
  const seen = await out([goto('/cookie-get'), { do: 'read', as: 'c', what: 'text', target: css('#c') }], { run: 'e1', policy })
  assert.equal(seen.reads.c, 'cookie:sid=[cookie]')
  // run.close frees a place for the 4th run
  assert.equal(await host.closeRun('e2'), 1)
  assert.equal((await exec([goto('/')], { run: 'e4', policy })).ok, true)
  assert.equal(host.size(), 3)
})

test('idle contexts are evicted by the sweep on the next request (fake clock)', async () => {
  await host.closeAll()
  const policy = { 'idle-ms': 1000 }
  await out([goto('/')], { run: 'i1', policy })
  clock += 500
  await out([goto('/')], { run: 'i2', policy })
  assert.equal(host.has('i1', 'web'), true)
  clock += 700 // i1 idle 1200, i2 idle 700
  await out([goto('/')], { run: 'i3', policy })
  assert.equal(host.has('i1', 'web'), false)
  assert.equal(host.has('i2', 'web'), true)
  clock += 5000
  await host.sweep()
  assert.equal(host.size(), 0)
})
