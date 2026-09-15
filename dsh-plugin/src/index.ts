/**
 * dsh-bridge-plugin: an in-process JSON-RPC 2.0 over WebSocket bridge for dsh.
 *
 * Binds only to 127.0.0.1 without authentication, like the Codex app-server. The same HTTP port serves
 * `GET /readyz` and `GET /healthz`; WebSocket connections use `/`.
 *
 * Uses named exports name / inject / Config / apply without a default export: the Loader's unwrapExports
 * drops named exports when it finds a default (vendor/loader/src/index.ts:191-199).
 */
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http'
import type { Context } from '@deepseek-ai/cordis'
import Schema from '@deepseek-ai/schemastery'

// Declaration merging: add the approval/request and user-questions/request waterfall events to Events.
import type {} from '@deepseek-ai/dsh-user-approval'
import type {} from '@deepseek-ai/dsh-user-questions'
import { WebSocketServer, type WebSocket } from 'ws'

import { Bridge, type BridgeConfig, type StartThreadParams } from './bridge.ts'
import { Connection, type MethodHandler } from './rpc.ts'
import { BridgeError } from './protocol.ts'

export const name = 'dsh-bridge'

export const inject = [
  'agents',
  'sessions',
  'sessionPersistence',
  'sessionQuery',
  'sessionProjections',
  'agentPresets',
  'agentDefaultModel',
  'workspaceRegistry',
  'sessionTitle',
]

export type { BridgeConfig }

export const Config: Schema<BridgeConfig> = Schema.object({
  host: Schema.string().default('127.0.0.1'),
  port: Schema.natural().max(65535).default(12898),
  agentPreset: Schema.string().default('standard'),
})

export function apply(ctx: Context, config: BridgeConfig): void {
  // Log to stderr: daemon.log captures both stdout and stderr, without competing with dsh's own stdout lines.
  const log = (message: string, error?: unknown): void => {
    if (error === undefined) console.error(`dsh-bridge: ${message}`)
    else console.error(`dsh-bridge: ${message}`, error)
  }

  // web-app owns its UI port in its webServer row; reconstruct only a token-free URL from the launch arguments here.
  const uiUrl = resolveUiUrl()
  const bridge = new Bridge(ctx, config, uiUrl, log)

  // Register all subscriptions once at the top level of apply, before any create/resume calls (as ACP does).
  // The plugin attaches to a ctx without a scope tag, receives process-wide events, and filters all of them by the thread table.
  ctx.on('session/event', (session, event) => { bridge.onSessionEvent(session, event) })
  ctx.on('agent/status', ({ agent, status }) => { bridge.onAgentStatus(agent, status) })
  ctx.on('agent/inbox/claimed', ({ agent, message, turn }) => { bridge.onInboxClaimed(agent, message.id, turn) })
  ctx.on('agent/error', ({ agent, turn, error }) => { bridge.onAgentError(agent, turn, error) })

  // Reverse requests (README: Request routing). `prepend: true` puts us before the gateway: with no browser connected,
  // the gateway never yields (packages/api/gateway/src/index.ts:421,511), so we must determine ownership first.
  // Requests not owned by this plugin immediately call next(), reach the gateway unchanged, and open a browser dialog.
  ctx.on('approval/request', (request, next) => bridge.onApprovalRequest(
    request as never,
    next as () => Promise<string>,
  ) as never, { prepend: true })

  ctx.on('user-questions/request', (request, next) => bridge.onUserQuestionRequest(
    request as never,
    next as () => Promise<unknown>,
  ) as never, { prepend: true })

  const methods = buildMethods(bridge)

  ctx.effect(() => {
    let nextConnectionId = 0
    const http: Server = createServer((req: IncomingMessage, res: ServerResponse) => {
      const path = (req.url ?? '/').split('?')[0]
      if (path === '/readyz' || path === '/healthz') {
        res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' })
        res.end('ok')
        return
      }
      res.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' })
      res.end('not found')
    })
    const wss = new WebSocketServer({ server: http })
    wss.on('connection', (socket: WebSocket) => {
      nextConnectionId += 1
      const conn = new Connection(nextConnectionId, socket, methods, log)
      bridge.addConnection(conn)
      socket.on('close', () => { bridge.removeConnection(conn) })
    })
    http.on('error', (error: Error) => { log('http server error', error) })
    http.listen(config.port, config.host, () => {
      log(`listening on ws://${config.host}:${config.port}/ (readyz on the same port)`)
    })
    return async () => {
      await bridge.dispose()
      await new Promise<void>((resolve) => { wss.close(() => { resolve() }) })
      await new Promise<void>((resolve) => { http.close(() => { resolve() }) })
    }
  }, 'dsh-bridge.server')
}

export function buildMethods(bridge: Bridge): Map<string, MethodHandler> {
  const methods = new Map<string, MethodHandler>()
  methods.set('initialize', () => bridge.initialize())
  methods.set('thread/start', (conn, params) => bridge.startThread(conn, params as unknown as StartThreadParams))
  methods.set('thread/resume', async (conn, params) => {
    const threadId = String(params['threadId'])
    const result = await bridge.resumeThread(conn, threadId)
    // Redelivery must follow this RPC response: use setTimeout rather than queueMicrotask,
    // because dispatch also writes the response in a microtask.
    setTimeout(() => { bridge.redeliverPending(conn, threadId) }, 0).unref?.()
    return result
  })
  methods.set('turn/start', (conn, params) => bridge.startTurn(conn, String(params['threadId']), String(params['text'])))
  methods.set('turn/steer', (conn, params) => bridge.steerTurn(
    conn,
    String(params['threadId']),
    String(params['text']),
    optionalNumber(params['expectedTurnId'], 'expectedTurnId'),
  ))
  methods.set('turn/interrupt', (conn, params) => bridge.interruptTurn(
    conn,
    String(params['threadId']),
    optionalNumber(params['turnId'], 'turnId'),
  ))
  methods.set('thread/read', (_conn, params) => bridge.readThread(String(params['threadId']), params['includeTurns'] === true))
  methods.set('thread/unsubscribe', (conn, params) => bridge.unsubscribe(conn, String(params['threadId'])))
  return methods
}

function optionalNumber(value: unknown, label: string): number | undefined {
  if (value === undefined || value === null) return undefined
  if (typeof value !== 'number' || !Number.isInteger(value)) {
    throw new BridgeError('invalid_params', `${label} must be an integer`)
  }
  return value
}

/** dsh's `--port` is web-app's UI port; the launcher leaves argv unchanged in process.argv. */
function resolveUiUrl(): string {
  const argv = process.argv
  let port = 3080
  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index]
    if (token === '--port' && index + 1 < argv.length) {
      const parsed = Number(argv[index + 1])
      if (Number.isInteger(parsed) && parsed > 0) port = parsed
    } else if (token !== undefined && token.startsWith('--port=')) {
      const parsed = Number(token.slice('--port='.length))
      if (Number.isInteger(parsed) && parsed > 0) port = parsed
    }
  }
  return `http://127.0.0.1:${port}/`
}
