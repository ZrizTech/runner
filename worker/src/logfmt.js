// The zriz log line (log format v3, see contract/log/). No shared code: the shared cases prove this matches.
// Values are never trusted: unknown keys are dropped and counted.
const KEY_ORDER = ['org_id', 'account_id', 'run_id', 'notice_id', 'delivery_id', 'step', 'op_id', 'pipeline_id', 'token_id', 'runner', 'listener', 'port', 'method', 'route', 'path', 'http_status', 'env', 'step_type', 'kind', 'channel', 'resource', 'cmd', 'mode', 'exit_code', 'frame', 'status', 'reason', 'auth_method', 'provider', 'browser', 'ip', 'op', 'table', 'rows', 'sent', 'skipped', 'received', 'count', 'dropped', 'refused', 'attempts', 'writes', 'timeout_ms', 'location', 'client', 'client_trace_id', 'cloud', 'build', 'requests', 'runs', 'avg_ms', 'window_s', 'error', 'elapsed_ms']
const RANK = new Map(KEY_ORDER.map((k, i) => [k, i]))
const MICROS = new Set(['avg_ms', 'elapsed_ms'])
const LEVELS = ['error', 'warn', 'info', 'debug', 'trace']
const NEEDS_QUOTE = /[\p{White_Space}"=\\\p{Cc}]/u
const ESCAPE = /["\\\p{Cc}\u2028\u2029]/gu
const SIMPLE = { '"': '\\"', '\\': '\\\\', '\n': '\\n', '\r': '\\r', '\t': '\\t' }
const MAX_SCALARS = 256

export const dropped = { count: 0 }

export function cut(s) {
  const a = Array.from(s)
  return a.length > MAX_SCALARS ? a.slice(0, MAX_SCALARS - 1).join('') + '…' : s
}

export function value(s) {
  s = cut(s)
  if (s !== '' && !NEEDS_QUOTE.test(s)) return s
  return '"' + s.replace(ESCAPE, (c) => SIMPLE[c] ?? `\\u{${c.codePointAt(0).toString(16).padStart(2, '0')}}`) + '"'
}

export function micros(us) {
  const n = BigInt(us)
  return `${n / 1000n}.${String(n % 1000n).padStart(3, '0')}`
}

function render(key, v) {
  if (v === null || v === undefined) return undefined
  if (MICROS.has(key)) {
    if (typeof v === 'object' && (Number.isInteger(v.micros) || typeof v.micros === 'bigint')) return micros(v.micros)
    return null
  }
  if (typeof v === 'string') return value(v)
  if (typeof v === 'boolean') return String(v)
  if (typeof v === 'bigint' || Number.isInteger(v)) return String(v)
  return null
}

// input: { time_ns (string|bigint), level, target, trace, event, fields: [[key, value]] }
export function formatLine(input) {
  const seen = new Set()
  const out = []
  for (const [key, v] of input.fields) {
    if (!RANK.has(key) || seen.has(key) || (key === 'ip' && input.target !== 'cloud.auth')) { dropped.count++; continue }
    const r = render(key, v)
    if (r === undefined) continue
    if (r === null) { dropped.count++; continue }
    seen.add(key)
    out.push([RANK.get(key), `${key}=${r}`])
  }
  out.sort((a, b) => a[0] - b[0])
  const time = new Date(Number(BigInt(input.time_ns) / 1000000n)).toISOString()
  const trace = `trace_id=${input.trace ?? '-'}`.padEnd(45)
  const head = `${time} ${input.level.padEnd(5)} ${trace} ${input.target.padEnd(15)} ${input.event}`
  return out.length ? `${head} ${out.map((e) => e[1]).join(' ')}` : head
}

// "<level>" or "<level>,<target>=<level>,..."; anything unusable falls back to info.
export const DEFAULT_FILTER = 'warn,worker=info'

export function parseFilter(spec) {
  const f = { def: 'info', over: new Map() }
  for (const part of String(spec ?? '').split(',').map((s) => s.trim()).filter(Boolean)) {
    const eq = part.lastIndexOf('=')
    const level = (eq < 0 ? part : part.slice(eq + 1)).toLowerCase()
    if (!LEVELS.includes(level)) continue
    if (eq < 0) f.def = level
    else f.over.set(part.slice(0, eq), level)
  }
  return f
}

// Like EnvFilter: an override matches by target prefix (`worker` matches `worker.main`); the longest wins.
export function enabled(filter, target, level) {
  let max = filter.def
  let best = -1
  for (const [k, v] of filter.over) {
    if (k.length > best && target.startsWith(k)) { best = k.length; max = v }
  }
  return LEVELS.indexOf(level.toLowerCase()) <= LEVELS.indexOf(max)
}

// Wall clock anchored once; later times move with the monotonic clock.
export function makeClock() {
  const wall = BigInt(Date.now()) * 1000000n
  const mono = process.hrtime.bigint()
  return () => wall + (process.hrtime.bigint() - mono)
}
