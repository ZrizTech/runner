import test, { before, after } from 'node:test'
import assert from 'node:assert/strict'
import { createBrowserHost } from '../src/browser.js'
import { validateRequest, validateResponse } from '../src/schema.js'
import { startSite } from './fixture-site.js'

let site
let host
let n = 0

before(async () => {
  site = await startSite()
  host = createBrowserHost()
})
after(async () => {
  await host.closeAll()
  await site.close()
})

// Builds a schema-valid request, runs it, checks the response against the schema, returns `out`.
async function run(commands, { run: runId = 'r-1', resource = 'web', args = {}, deadline = 30000 } = {}) {
  const req = {
    v: 1,
    'op-id': `op-${++n}`,
    run: runId,
    kind: 'browser.page',
    policy: { resource, 'base-url': site.origin, origins: [site.origin], 'max-contexts': 16 },
    args: { commands, 'command-timeout-ms': 1500, ...args },
    'deadline-ms': deadline,
  }
  assert.equal(validateRequest(req), true, JSON.stringify(validateRequest.errors))
  const resp = await host.handle(req)
  assert.equal(validateResponse(resp), true, JSON.stringify(validateResponse.errors))
  assert.equal(resp.ok, true)
  return resp.out
}

const goto = (path) => ({ do: 'goto', path })
const css = (c) => ({ css: c })
const read = (as, what, target) => ({ do: 'read', as, what, ...(target ? { target } : {}) })

test('goto + read url, title, status, text, count, visible, attr', async () => {
  const o = await run([
    goto('/'),
    read('url', 'url'),
    read('title', 'title'),
    read('h', 'text', css('h1')),
    read('n', 'count', css('li')),
    read('v', 'visible', css('h1')),
    read('nov', 'visible', css('#nothing')),
    read('x', 'attr:data-x', css('#lnk')),
  ])
  assert.deepEqual(o, {
    ok: true, url: '/', title: 'Home', status: 200, blocked: 0, 'failed-at': null, error: null,
    reads: { url: '/', title: 'Home', h: 'Home', n: 3, v: true, nov: false, x: '42' },
  })
})

test('click follows a link; status is the last navigation; url path only', async () => {
  const o = await run([goto('/'), { do: 'click', target: { role: 'link', name: 'Next page' } }, { do: 'wait-for', url: '/next' }])
  assert.equal(o.ok, true)
  assert.equal(o.url, '/next')
  assert.equal(o.title, 'Next')
  assert.equal(o.status, 200)
  const o2 = await run([goto('/missing')])
  assert.equal(o2.status, 404)
  assert.equal(o2.ok, true)
})

test('fill, click, read value/text; label, text and test-id locators', async () => {
  const o = await run([
    goto('/form'),
    { do: 'fill', target: { label: 'Email' }, value: 'a@b.c' },
    read('val', 'value', { label: 'Email' }),
    { do: 'click', target: { role: 'button', name: 'Save', exact: true } },
    read('out', 'text', css('#out')),
    read('tid', 'text', { 'test-id': 'tid' }),
    read('txt', 'visible', { text: 'by test id' }),
  ])
  assert.equal(o.ok, true, JSON.stringify(o))
  assert.deepEqual(o.reads, { val: 'a@b.c', out: 'saved:a@b.c', tid: 'by test id', txt: true })
})

test('select, check, uncheck, press, press-seq', async () => {
  const o = await run([
    goto('/form'),
    { do: 'select', target: { label: 'Color' }, value: 'b' },
    read('color', 'value', { label: 'Color' }),
    { do: 'check', target: { label: 'Agree' } },
    read('chk', 'attr:id', { label: 'Agree' }),
    { do: 'uncheck', target: { label: 'Agree' } },
    { do: 'press', target: { label: 'Keys' }, key: 'Enter' },
    read('keyed', 'text', css('#keyed')),
    { do: 'press-seq', target: { label: 'Seq' }, value: 'hello' },
    read('seq', 'value', { label: 'Seq' }),
  ])
  assert.equal(o.ok, true, JSON.stringify(o))
  assert.deepEqual(o.reads, { color: 'b', chk: 'c', keyed: 'enter', seq: 'hello' })
})

test('wait-for visible, hidden, attached and url', async () => {
  const o = await run([
    goto('/form'),
    { do: 'wait-for', target: css('#late'), state: 'visible' },
    { do: 'wait-for', target: css('#gone'), state: 'hidden' },
    { do: 'wait-for', target: css('#late'), state: 'attached' },
    { do: 'wait-for', url: '/form' },
  ])
  assert.equal(o.ok, true, JSON.stringify(o))
})

test('viewport and overflow-x', async () => {
  const o = await run([
    goto('/wide'), read('wide', 'overflow-x'),
    goto('/narrow'), { do: 'viewport', width: 390, height: 844 }, read('narrow', 'overflow-x'),
  ])
  assert.deepEqual(o.reads, { wide: true, narrow: false })
  // the viewport stays for the next op of this run
  const o2 = await run([goto('/narrow'), read('w', 'overflow-x')])
  assert.equal(o2.reads.w, false)
})

test('nth picks the match; text is capped at 8 KiB', async () => {
  const o = await run([goto('/'), read('second', 'text', { css: 'li', nth: 1 }), goto('/big'), read('big', 'text', css('#big'))])
  assert.equal(o.reads.second, 'b')
  assert.equal(o.reads.big.length, 8192)
})

test('failing locator: index and fixed reason, not an error', async () => {
  const o = await run([goto('/'), read('h', 'text', css('h1')), { do: 'click', target: css('#nothing') }, read('never', 'url')])
  assert.equal(o.ok, false)
  assert.equal(o['failed-at'], 2)
  assert.equal(o.error, 'not-found')
  assert.deepEqual(Object.keys(o.reads), ['h'])
})

test('hidden element -> not-visible; wait-for never true -> timeout', async () => {
  const o = await run([goto('/hidden'), { do: 'click', target: css('#hb') }])
  assert.equal(o['failed-at'], 1)
  assert.equal(o.error, 'not-visible')
  const o2 = await run([goto('/hidden'), { do: 'wait-for', target: css('#hb'), state: 'visible' }])
  assert.equal(o2['failed-at'], 1)
  assert.equal(o2.error, 'timeout')
  const o3 = await run([goto('/'), { do: 'wait-for', url: '/never' }])
  assert.equal(o3.error, 'timeout')
})

test('navigation failure and off-host goto', async () => {
  const o = await run([goto('/'), goto('//evil.example/x')])
  assert.equal(o['failed-at'], 1)
  assert.equal(o.error, 'host-not-allowed')
  const dead = await run([goto('/')], { resource: 'dead' })
  assert.equal(dead.ok, true)
  const req = {
    v: 1, 'op-id': 'op-dead', run: 'r-dead', kind: 'browser.page',
    policy: { resource: 'w', 'base-url': 'http://127.0.0.1:1', origins: ['http://127.0.0.1:1'] },
    args: { commands: [goto('/')], 'command-timeout-ms': 1500 }, 'deadline-ms': 10000,
  }
  const r = await host.handle(req)
  assert.equal(r.out.error, 'navigation-failed')
  assert.equal(r.out['failed-at'], 0)
})

test('deadline bounds the op', async () => {
  const o = await run([goto('/hidden'), { do: 'wait-for', target: css('#hb'), state: 'visible' }], {
    args: { 'command-timeout-ms': 20000 }, deadline: 800,
  })
  assert.equal(o.ok, false)
  assert.equal(o.error, 'timeout')
  assert.equal(o['failed-at'], 1)
})

test('same run + resource shares cookies across ops; another run does not', async () => {
  await run([goto('/cookie-set')], { run: 'r-cookie' })
  const same = await run([goto('/cookie-get'), read('c', 'text', css('#c'))], { run: 'r-cookie' })
  assert.equal(same.reads.c, 'cookie:sid=[cookie]')
  const other = await run([goto('/cookie-get'), read('c', 'text', css('#c'))], { run: 'r-other' })
  assert.equal(other.reads.c, 'cookie:none')
  const otherRes = await run([goto('/cookie-get'), read('c', 'text', css('#c'))], { run: 'r-cookie', resource: 'web2' })
  assert.equal(otherRes.reads.c, 'cookie:none')
})

test('close API: closeContext, closeRun, sweep', async () => {
  await run([goto('/')], { run: 'r-close', resource: 'a' })
  await run([goto('/')], { run: 'r-close', resource: 'b' })
  assert.equal(host.has('r-close', 'a'), true)
  await host.closeContext('r-close', 'a')
  assert.equal(host.has('r-close', 'a'), false)
  await host.closeRun('r-close')
  assert.equal(host.has('r-close', 'b'), false)
  await run([goto('/cookie-get'), read('c', 'text', css('#c'))], { run: 'r-close2', resource: 'a' })
  const before = host.size()
  await host.sweep(60000)
  assert.equal(host.size(), before)
  await host.sweep(0, Date.now() + 1)
  assert.equal(host.size(), 0)
})
