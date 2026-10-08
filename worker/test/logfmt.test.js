import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dropped, enabled, formatLine, parseFilter } from '../src/logfmt.js'

const dir = new URL('../../contract/log/', import.meta.url)
const cases = JSON.parse(readFileSync(new URL('cases.json', dir), 'utf8'))
const lists = JSON.parse(readFileSync(new URL('lists.json', dir), 'utf8'))

// The worker has no library map: cases for cloud/runner library targets are not its business.
const own = cases.cases.filter((c) => !c.input.product)

test('every own case matches byte for byte', () => {
  assert.ok(own.length > 100)
  for (const c of own) {
    dropped.count = 0
    assert.equal(formatLine(c.input), c.expect_line, c.name)
    assert.equal(dropped.count, c.expect_dropped, `${c.name}: dropped`)
  }
})

test('every filter case', () => {
  assert.ok(cases.filter_cases.length >= 40)
  for (const f of cases.filter_cases) {
    if (f.target.includes('::')) continue // library targets: no library lines in the worker, still parsed
    assert.equal(enabled(parseFilter(f.spec), f.target, f.level), f.shown, JSON.stringify(f))
  }
  for (const f of cases.filter_cases.filter((x) => x.target.includes('::'))) {
    assert.equal(enabled(parseFilter(f.spec), f.target, f.level), f.shown, JSON.stringify(f))
  }
})

test('key order in the formatter is the list order', async () => {
  const src = readFileSync(new URL('../src/logfmt.js', import.meta.url), 'utf8')
  const m = /const KEY_ORDER = (\[.*\])/.exec(src)
  assert.deepEqual(JSON.parse(m[1].replaceAll("'", '"')), lists.keys.order)
})
