import type { WebSocket } from 'ws'
import {
  BRIDGE_ERROR_CODE,
  BridgeError,
  type RpcErrorBody,
  type RpcId,
  type RpcResponse,
} from './protocol.ts'

/** A method handler. Its return value becomes the JSON-RPC result directly. */
export type MethodHandler = (conn: Connection, params: Record<string, unknown>) => Promise<unknown> | unknown

/**
 * The client answered a reverse request with a JSON-RPC error.
 * This is an **explicit rejection**, not a transport failure: the Rust CLI's `--auto-decline` sends this response.
 */
export class RpcRemoteError extends Error {
  constructor(readonly body: RpcErrorBody) {
    super(body.message)
    this.name = 'RpcRemoteError'
  }
}

/** A reverse request was abandoned unanswered (the connection closed or it was resolved elsewhere). This is not a rejection; callers should ignore it. */
export class RpcAbandonedError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'RpcAbandonedError'
  }
}

/**
 * A client connection. Parses and sends JSON-RPC frames and matches responses to server-initiated reverse requests.
 * The thread subscription set lives here; the thread table only stores Connection references.
 */
export class Connection {
  /** Thread ids subscribed to by this connection. */
  readonly subscriptions = new Set<string>()
  /** In-flight server -> client requests on this connection. */
  private readonly outbound = new Map<string, { resolve: (value: unknown) => void; reject: (error: Error) => void }>()
  private closed = false

  constructor(
    readonly id: number,
    private readonly socket: WebSocket,
    private readonly methods: Map<string, MethodHandler>,
    private readonly log: (message: string, error?: unknown) => void,
  ) {
    socket.on('message', (raw: unknown) => { this.onMessage(String(raw)) })
    socket.on('close', () => { this.onClose() })
    socket.on('error', (error: Error) => { this.log(`connection ${this.id} socket error`, error) })
  }

  get isClosed(): boolean {
    return this.closed
  }

  /** A one-way notification. */
  notify(method: string, params: unknown): void {
    this.write({ jsonrpc: '2.0', method, params })
  }

  /**
   * A server -> client request. The caller supplies the id: redelivering a reverse request must reuse the same id.
   * Settlement: client result -> resolve; client error -> reject(RpcRemoteError);
   * connection closed or withdrawn by `cancelRequest` -> reject(RpcAbandonedError).
   */
  request(id: string, method: string, params: unknown): Promise<unknown> {
    if (this.closed) return Promise.reject(new RpcAbandonedError('connection closed'))
    return new Promise<unknown>((resolve, reject) => {
      this.outbound.set(id, { resolve, reject })
      this.write({ jsonrpc: '2.0', id, method, params })
    })
  }

  /** Whether this reverse request is still pending on this connection. */
  hasRequest(id: string): boolean {
    return this.outbound.has(id)
  }

  /** Withdraw an in-flight reverse request (the turn was interrupted or it was resolved elsewhere). */
  cancelRequest(id: string, reason: string): void {
    const pending = this.outbound.get(id)
    if (pending === undefined) return
    this.outbound.delete(id)
    pending.reject(new RpcAbandonedError(reason))
  }

  close(): void {
    try {
      this.socket.close()
    } catch (error: unknown) {
      this.log(`connection ${this.id} close failed`, error)
    }
  }

  private onClose(): void {
    this.closed = true
    for (const [id, pending] of this.outbound) {
      this.outbound.delete(id)
      pending.reject(new RpcAbandonedError('connection closed'))
    }
    this.subscriptions.clear()
  }

  private write(frame: unknown): void {
    if (this.closed) return
    try {
      this.socket.send(JSON.stringify(frame))
    } catch (error: unknown) {
      this.log(`connection ${this.id} send failed`, error)
    }
  }

  private onMessage(raw: string): void {
    let frame: unknown
    try {
      frame = JSON.parse(raw)
    } catch {
      this.write({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } satisfies RpcErrorBody })
      return
    }
    if (typeof frame !== 'object' || frame === null) {
      this.write({ jsonrpc: '2.0', id: null, error: { code: -32600, message: 'invalid request' } })
      return
    }
    const message = frame as Partial<RpcResponse> & { method?: unknown; params?: unknown }
    if (typeof message.method === 'string') {
      void this.dispatch(message.id ?? null, message.method, message.params)
      return
    }
    // Without a method, this is a response to a reverse request we sent.
    if (message.id === undefined || message.id === null) return
    const key = String(message.id)
    const pending = this.outbound.get(key)
    if (pending === undefined) return // Late response: already resolved elsewhere, so ignore it.
    this.outbound.delete(key)
    if (message.error !== undefined) {
      pending.reject(new RpcRemoteError(message.error))
      return
    }
    pending.resolve(message.result)
  }

  private async dispatch(id: RpcId | null, method: string, params: unknown): Promise<void> {
    const handler = this.methods.get(method)
    if (handler === undefined) {
      if (id !== null) {
        this.write({ jsonrpc: '2.0', id, error: { code: -32601, message: `unknown method "${method}"` } })
      }
      return
    }
    const args = typeof params === 'object' && params !== null ? params as Record<string, unknown> : {}
    try {
      const result = await handler(this, args)
      if (id !== null) this.write({ jsonrpc: '2.0', id, result: result ?? {} })
    } catch (error: unknown) {
      const body = error instanceof BridgeError
        ? error.toBody()
        : { code: BRIDGE_ERROR_CODE, message: error instanceof Error ? error.message : String(error), data: { kind: 'internal' } }
      this.log(`method "${method}" failed`, error)
      if (id !== null) this.write({ jsonrpc: '2.0', id, error: body })
    }
  }
}
