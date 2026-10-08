import { makeLogger } from './log.js'
import { startServer, stopServer } from './server.js'

const log = makeLogger()

// One ERROR panic line, then exit. Only the error's class name and the top frame's file:line: no message text.
const panic = (err) => {
  const top = /([^/\s()]+:\d+):\d+\)?\s*$/m.exec(String(err?.stack ?? '').split('\n').filter((l) => /^\s+at /.test(l))[0] ?? '')
  log.emit('ERROR', 'worker.main', 'panic', [['location', top?.[1]], ['error', err?.name ?? 'Error']])
  process.exit(1)
}
process.on('uncaughtException', panic)
process.on('unhandledRejection', panic)

const socketPath = process.env.ZRIZ_WORKER_SOCKET
if (!socketPath) {
  log.emit('ERROR', 'worker.main', 'stopped', [['listener', 'unix'], ['error', 'ZRIZ_WORKER_SOCKET not set']])
  process.exit(2)
}

const server = await startServer(socketPath, { log })

let stopping = false
const stop = async () => {
  if (stopping) return
  stopping = true
  await stopServer(server, socketPath)
  process.exit(0)
}
// Handlers go in before the started line: whoever waits for that line may signal at once.
process.on('SIGTERM', stop)
process.on('SIGINT', stop)
log.emit('INFO', 'worker.main', 'started', [['listener', 'unix']])
