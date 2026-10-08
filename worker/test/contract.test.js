import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { validateRequest, validateResponse, contractDir } from '../src/schema.js'

const fx = (kind) => {
  const dir = new URL(`fixtures/${kind}/`, contractDir)
  return readdirSync(dir).map((f) => [f, JSON.parse(readFileSync(new URL(f, dir), 'utf8'))])
}
const pick = (f) => (f.startsWith('request-') ? validateRequest : validateResponse)

for (const [f, doc] of fx('valid')) {
  test(`valid fixture passes: ${f}`, () => assert.equal(pick(f)(doc), true))
}
for (const [f, doc] of fx('invalid')) {
  test(`invalid fixture fails: ${f}`, () => assert.equal(pick(f)(doc), false))
}
