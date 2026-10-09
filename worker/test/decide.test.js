import test from 'node:test'
import assert from 'node:assert/strict'
import { waitBound, createEnded, scrubber, scrubPage } from '../src/decide.js'

test('waitBound', () => {
  assert.equal(waitBound(900, 5000), 0)
  assert.equal(waitBound(1000, 5000), 0)
  assert.equal(waitBound(1300, 5000), 300)
  assert.equal(waitBound(30000, 5000), 5000)
})

test('createEnded: bounded, oldest out', () => {
  const e = createEnded(3)
  for (const r of ['a', 'b', 'c', 'd']) e.add(r)
  assert.equal(e.has('a'), false)
  assert.equal(e.has('d'), true)
  assert.equal(e.size(), 3)
})

test('scrubber: long values anywhere, short ones as whole tokens', () => {
  const s = scrubber(['abcd', 'ab'])
  assert.equal(s('xabcdx ab cab'), 'x[cookie]x [cookie] cab')
})

test('scrubPage: no cookie list, no page data', () => {
  assert.equal(scrubPage({ url: '/u', title: 't', reads: { a: 'x' } }, null), null)
  assert.deepEqual(scrubPage({ url: '/u', title: 't zz12', reads: { a: 'zz12', b: 3 } }, ['zz12']), { url: '/u', title: 't [cookie]', reads: { a: '[cookie]', b: 3 } })
})
