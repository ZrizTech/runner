// One line per request, log format v3 (contract/log/). Only ids, kind, reason and time. Never args, values or output.
import { DEFAULT_FILTER, enabled, formatLine, makeClock, parseFilter } from './logfmt.js'

const ID = /^[A-Za-z0-9_.:-]{1,64}$/
const TRACE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/
const KINDS = new Set(['browser.page', 'cli.exec'])
const REASONS = new Set(['bad-json', 'bad-version', 'unknown-kind', 'bad-request', 'request-too-large', 'not-implemented', 'at-capacity', 'handle-busy', 'no-handle', 'too-many-handles', 'spawn-failed', 'internal', 'timeout'])
const MODES = new Set(['run', 'start', 'read', 'wait', 'stop'])
const okId = (v) => (typeof v === 'string' && ID.test(v) ? v : undefined)
const COMPONENT = { 'browser.page': 'worker.browser', 'cli.exec': 'worker.cli' }

export function makeLogger({ write = (s) => process.stdout.write(s), filter = parseFilter(process.env.ZRIZ_LOG || DEFAULT_FILTER), now = makeClock() } = {}) {
  const emit = (level, target, event, fields, trace) => {
    if (!enabled(filter, target, level)) return
    write(formatLine({ time_ns: now(), level, target, trace, event, fields }) + '\n')
  }
  const log = ({ opId, run, kind, mode, cmd, exit, ok, reason, us, traceId }) => {
    const trace = typeof traceId === 'string' && TRACE.test(traceId) ? traceId : undefined
    if (kind === 'ping') return emit('DEBUG', 'worker.main', 'ping done', [['elapsed_ms', { micros: us }]], trace)
    if (!KINDS.has(kind)) {
      // No known kind: worker.main lists only reason and elapsed_ms for op failed.
      return emit('WARN', 'worker.main', 'op failed', [['reason', REASONS.has(reason) ? reason : undefined], ['elapsed_ms', { micros: us }]], trace)
    }
    const fields = [
      ['run_id', okId(run)], ['op_id', okId(opId)], ['kind', KINDS.has(kind) ? kind : undefined],
      ['cmd', okId(cmd)], ['mode', MODES.has(mode) ? mode : undefined],
      ['exit_code', Number.isInteger(exit) ? exit : undefined],
      ['status', ok ? 'pass' : 'error'], ['reason', REASONS.has(reason) ? reason : undefined],
      ['elapsed_ms', { micros: us }],
    ]
    const target = COMPONENT[kind]
    if (ok) emit('INFO', target, 'op done', fields, trace)
    else emit('WARN', target, 'op failed', fields, trace)
  }
  log.emit = emit
  return log
}
