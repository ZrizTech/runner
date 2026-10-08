// "The worker holds no runner secret." Part 1: the worker reads a closed set of
// environment names and never hands its own environment on. Part 2: the compose
// file gives the worker service nothing but its socket path.
import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const ALLOWED_ENV = ['ZRIZ_LOG', 'ZRIZ_WORKER_SOCKET']

const here = (p) => fileURLToPath(new URL(p, import.meta.url))
const srcDir = here('../src/')
const sources = () => readdirSync(srcDir, { recursive: true })
  .filter((f) => f.endsWith('.js'))
  .map((f) => ({ file: f, text: readFileSync(srcDir + f, 'utf8') }))

// What the scan flags in one source text: names read, and any use of the env
// that is not a plain `process.env.NAME` read.
export function scanEnv(text) {
  const names = [...text.matchAll(/\bprocess\.env\.([A-Za-z0-9_]+)/g)].map((m) => m[1])
  const loose = []
  for (const m of text.matchAll(/\bprocess\.env\b(?!\.[A-Za-z_])/g)) loose.push(`process.env at ${m.index}`)
  for (const m of text.matchAll(/\bprocess\s*(\[|\?\.\s*env)|[=(,]\s*process\s*(?=[;,)\n]|$)/gm)) loose.push(`process alias at ${m.index}`)
  if (/import\s+[^;\n]*from\s+['"](node:)?process['"]|require\(\s*['"](node:)?process['"]\s*\)/.test(text)) loose.push('process import')
  if (/\bconst\s*\{[^}]*\benv\b[^}]*\}\s*=\s*process\b/.test(text)) loose.push('env destructured')
  return { names, loose }
}

test('the scan flags whole-env passes, dynamic reads and aliases', () => {
  const cases = [
    ['const a = process.env.ZRIZ_LOG', ['ZRIZ_LOG'], 0],
    ['spawn(c, { env: process.env })', [], 1],
    ['const e = { ...process.env }', [], 1],
    ['const k = process.env[name]', [], 1],
    ['const p = process\nuse(p)', [], 1],
    ['const { env } = process', [], 2],
    ["import process from 'node:process'", [], 1],
    ['process.stdout.write(s)', [], 0],
  ]
  for (const [text, names, loose] of cases) {
    const r = scanEnv(text)
    assert.deepEqual(r.names, names, text)
    assert.ok(loose === 0 ? r.loose.length === 0 : r.loose.length >= 1, `${text} -> ${r.loose}`)
  }
})

test('the worker reads only the closed set of environment names and never passes its env on', () => {
  const read = new Set()
  const files = sources()
  assert.ok(files.length >= 5, 'found the worker sources')
  for (const { file, text } of files) {
    const { names, loose } = scanEnv(text)
    assert.deepEqual(loose, [], `${file}: the worker env is used as a whole or dynamically`)
    names.forEach((n) => read.add(n))
  }
  assert.deepEqual([...read].sort(), ALLOWED_ENV)
})

test('child processes and the browser launch get an explicit env, not the worker env', () => {
  const spawns = sources().filter(({ text }) => /child_process/.test(text))
  assert.deepEqual(spawns.map((s) => s.file), ['cli.js'])
  // the spawn options carry `env` built from a literal, never from the worker env
  assert.match(spawns[0].text, /const env = \{ PATH:/)
  assert.match(spawns[0].text, /spawn\([\s\S]*?shell: false,[^}]*\benv,/)
  for (const { file, text } of sources()) {
    for (const m of text.matchAll(/\.launch(?:Persistent\w*)?\(([^)]*)\)/g)) {
      assert.doesNotMatch(m[1], /\benv\b/, `${file}: browser launch passes an env`)
    }
  }
})

test('compose gives the worker service no env_file and only ZRIZ_WORKER_SOCKET', () => {
  const lines = readFileSync(here('../../ops/docker-compose.yml'), 'utf8').split('\n')
  const start = lines.findIndex((l) => /^  worker:\s*$/.test(l))
  assert.ok(start >= 0, 'worker service found')
  let end = lines.findIndex((l, i) => i > start && /^ {0,2}\S/.test(l))
  if (end < 0) end = lines.length
  const block = lines.slice(start + 1, end)
  assert.ok(block.length > 5, 'worker block parsed')
  assert.ok(!block.some((l) => /^\s{4}env_file\s*:/.test(l)), 'env_file on the worker')
  const at = block.findIndex((l) => /^\s{4}environment\s*:/.test(l))
  assert.ok(at >= 0, 'environment block found')
  const vars = []
  for (const l of block.slice(at + 1)) {
    if (/^\s{4}\S/.test(l)) break
    const m = l.match(/^\s+-?\s*([A-Za-z_][A-Za-z0-9_]*)\s*[:=]/)
    if (m) vars.push(m[1])
    else assert.match(l, /^\s*(#.*)?$/, `unparsed environment line: ${l}`)
  }
  assert.deepEqual(vars, ['ZRIZ_WORKER_SOCKET'])
})
