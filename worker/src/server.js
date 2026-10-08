import net from 'node:net'
import { chmodSync, existsSync, unlinkSync } from 'node:fs'
import { MAX_LINE_BYTES, VERSION, errorResponse, parseRequest } from './protocol.js'
import { createBrowserHost } from './browser.js'
import { createCliHost } from './cli.js'

const READ_TIMEOUT_MS = 10000

// ping, browser.page and cli.exec are answered.
const makeHandler = (host, cli) => async (req) => {
  if (req.kind === 'ping') return { v: VERSION, kind: 'ping', ok: true }
  if (req.kind === 'browser.page') return host.handle(req)
  if (req.kind === 'cli.exec') return cli.handle(req)
  return errorResponse('not-implemented', req['op-id'])
}

export function startServer(socketPath, { log, host = createBrowserHost(), cli = createCliHost(), handler = makeHandler(host, cli) } = {}) {
  if (existsSync(socketPath)) unlinkSync(socketPath)
  const server = net.createServer((sock) => {
    const t0 = process.hrtime.bigint()
    const chunks = []
    let size = 0
    let done = false
    sock.setTimeout(READ_TIMEOUT_MS, () => sock.destroy())
    sock.on('error', () => sock.destroy())

    let traceId
    const finish = (resp, meta) => {
      if (done) return
      done = true
      log?.({ traceId, ...meta, ok: resp.ok, reason: resp.reason, exit: resp.out?.['exit-code'], us: (process.hrtime.bigint() - t0) / 1000n })
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
        finish(await handler(p.req), meta)
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
