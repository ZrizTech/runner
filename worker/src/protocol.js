import { validateRequest } from './schema.js'

export const VERSION = 1
export const MAX_LINE_BYTES = 1024 * 1024
export const KINDS = ['ping', 'run.close', 'browser.page', 'cli.exec']

// Fixed reason codes with fixed messages. No free text from any other source.
export const ERRORS = {
  'bad-json': 'request is not valid JSON',
  'bad-version': 'unsupported protocol version',
  'unknown-kind': 'unknown request kind',
  'bad-request': 'request does not match the schema',
  'request-too-large': 'request line is too large',
  'not-implemented': 'kind is not implemented yet',
  'at-capacity': 'worker is at its browser context limit',
  'context-lost': 'the browser context of this run was closed after it was idle',
  'handle-busy': 'handle is already running',
  'no-handle': 'no such handle',
  'too-many-handles': 'too many live handles',
  'spawn-failed': 'command could not be started',
  'internal': 'internal worker error',
}

// Messages for a reason with a `why`: the plain message of the reason would be false for these.
const WHY_MESSAGES = {
  'context-lost/run-closed': 'the run has ended; its browser context or command was closed',
  'no-handle/idle': 'this handle was closed after it was idle',
}

const ID = /^[A-Za-z0-9_.:-]{1,64}$/

// `numbers`: optional integer fields (max-contexts, busy, waited-ms) and `why`, data and never free text.
export function errorResponse(reason, opId, numbers = {}) {
  const r = { v: VERSION }
  if (typeof opId === 'string' && ID.test(opId)) r['op-id'] = opId
  r.ok = false
  r.reason = reason
  r.message = WHY_MESSAGES[`${reason}/${numbers.why}`] ?? ERRORS[reason]
  Object.assign(r, numbers)
  return r
}

// Returns { ok: true, req } or { ok: false, reason, opId? }.
export function parseRequest(line) {
  let req
  try {
    req = JSON.parse(line)
  } catch {
    return { ok: false, reason: 'bad-json' }
  }
  if (req === null || typeof req !== 'object' || Array.isArray(req)) {
    return { ok: false, reason: 'bad-request' }
  }
  if (req.v !== VERSION) return { ok: false, reason: 'bad-version' }
  if (typeof req.kind !== 'string' || !KINDS.includes(req.kind)) {
    return { ok: false, reason: 'unknown-kind' }
  }
  if (!validateRequest(req)) return { ok: false, reason: 'bad-request', opId: req['op-id'] }
  return { ok: true, req, trace: req['trace-id'] }
}
