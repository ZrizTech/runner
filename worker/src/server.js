import net from 'node:net'
import { randomBytes } from 'node:crypto'
import { chmodSync, existsSync, unlinkSync } from 'node:fs'
import { MAX_LINE_BYTES, VERSION, errorResponse, parseRequest } from './protocol.js'
import { createBrowserHost } from './browser.js'
import { createCliHost } from './cli.js'

const READ_TIMEOUT_MS = 10000

// One id for this process, made at the start. A new id tells the runner that the worker restarted.
const BOOT_ID = `b-${randomBytes(6).toString('hex')}`

// ping, run.close, browser.page and cli.exec are answered.
const makeHandler = (host, cli) => async (req, { signal } = {}) => {
  if (req.kind === 'ping') return { v: VERSION, kind: 'ping', ok: true, browser: host.stats(), cli: { busy: cli.size(), limit: cli.limit }, 'boot-id': BOOT_ID }
  if (req.kind === 'run.close') return { v: VERSION, kind: 'run.close', ok: true, closed: (await host.closeRun(req.run)) + (await cli.closeRun(req.run)) }
  if (req.kind === 'browser.page') return host.handle(req, { signal })
  if (req.kind === 'cli.exec') return cli.handle(req, { signal })
  return errorResponse('not-implemented', req['op-id'])
}

export function startServer(socketPath, { log, host = createBrowserHost({ log }), cli = createCliHost({ log }), handler = makeHandler(host, cli) } = {}) {
  if (existsSync(socketPath)) unlinkSync(socketPath)
  const server = net.createServer((sock) => {
    const t0 = process.hrtime.bigint()
    const chunks = []
    let size = 0
    let done = false
    sock.setTimeout(READ_TIMEOUT_MS, () => sock.destroy())
    sock.on('error', () => sock.destroy())

    // Aborts when the runner closes its end before the answer: nobody waits, so waiters and ops stop.
    const gone = new AbortController()
    sock.on('close', () => { if (!done) gone.abort() })
    let traceId
    const finish = (resp, meta) => {
      if (done) return
      done = true
      log?.({ traceId, ...meta, ok: resp.ok, reason: resp.reason, closed: resp.closed, exit: resp.out?.['exit-code'], us: (process.hrtime.bigint() - t0) / 1000n })
      sock.end(JSON.stringify(resp) + '\n')
    }

    sock.on('data', async (buf) => {
      if (done) return
      size += buf.length
      if (size > MAX_LINE_BYTES) return finish(errorResponse('request-too-large'), {})
      chunks.push(buf)
      const all = Buffer.concat(chunks)
      const nl = all.indexOf(0x0a)
      if (nl < 0) return
      sock.setTimeout(0) // the request is in; the op may run up to its deadline
      const p = parseRequest(all.subarray(0, nl).toString('utf8'))
      traceId = p.trace
      if (!p.ok) return finish(errorResponse(p.reason, p.opId), { opId: p.opId, kind: undefined })
      const meta = { opId: p.req['op-id'], run: p.req.run, kind: p.req.kind }
      if (p.req.kind === 'cli.exec') { meta.mode = p.req.args.mode; meta.cmd = p.req.policy.command }
      try {
        const resp = await handler(p.req, { signal: gone.signal })
        // The numbers of a capacity refusal go to the log line.
        if (resp.reason === 'at-capacity') { meta.busy = resp.busy; meta.cap = resp['max-contexts'] }
        if (resp.reason === 'too-many-handles') { meta.busy = cli.size(); meta.cap = cli.limit }
        finish(resp, meta)
      } catch {
        finish(errorResponse('internal', p.req['op-id']), meta)
      }
    })
  })
  server.host = host
  server.cli = cli
  return new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(socketPath, () => {
      try { chmodSync(socketPath, 0o660) } catch { /* best effort */ }
      resolve(server)
    })
  })
}

export function stopServer(server, socketPath) {
  return new Promise((resolve) => {
    server.close(async () => {
      try { unlinkSync(socketPath) } catch { /* already gone */ }
      await server.host?.closeAll()
      await server.cli?.closeAll()
      resolve()
    })
    server.closeAllConnections?.()
  })
}
