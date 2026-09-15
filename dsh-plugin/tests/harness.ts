/**
 * Fake dsh services and a fake WebSocket drive Bridge and Connection without a dsh process.
 * Implement only the few members Bridge actually calls, with shapes taken from the 0.1.5-rc.2 source.
 */
import { Bridge, type BridgeConfig } from '../src/bridge.ts'
import { Connection, type MethodHandler } from '../src/rpc.ts'
import { buildMethods } from '../src/index.ts'

export interface FakeEvent {
  type: string
  seq: number
  time: number
  data: Record<string, unknown>
}

export interface FakeAgentOptions {
  provider?: string
  model?: string
  reasoningEffort?: string
}

export class FakeSession {
  readonly events: FakeEvent[] = []
  private seq = 0

  constructor(readonly id: string, readonly header: Record<string, unknown>) {}

  append(type: string, data: Record<string, unknown>): FakeEvent {
    const event: FakeEvent = { type, seq: this.seq, time: Date.now(), data }
    this.seq += 1
    this.events.push(event)
    return event
  }

  snapshotEvents(): readonly FakeEvent[] {
    return this.events
  }
}

export class FakeAgent {
  status: 'idle' | 'running' = 'idle'
  readonly session: FakeSession
  readonly followups: { id: string; text: string }[] = []
  readonly steers: { id: string; text: string }[] = []
  cancelled = 0

  constructor(readonly id: string, cwd: string, public options: FakeAgentOptions = {}) {
    this.session = new FakeSession(id, { id, cwd })
  }

  followup(message: { id: string; content: { type: string; text?: string }[] }): void {
    this.followups.push({ id: message.id, text: textOf(message) })
  }

  steer(message: { id: string; content: { type: string; text?: string }[] }): void {
    this.steers.push({ id: message.id, text: textOf(message) })
  }

  cancel(): void {
    this.cancelled += 1
  }
}

function textOf(message: { content: { type: string; text?: string }[] }): string {
  return message.content.filter(block => block.type === 'text').map(block => block.text ?? '').join('')
}

/** Fake ctx: only the services used by Bridge. */
export class FakeContext {
  readonly agents = new Map<string, FakeAgent>()
  readonly persisted = new Map<string, Record<string, unknown>>()
  readonly persistedEvents = new Map<string, FakeEvent[]>()
  readonly flushed: string[] = []
  modelSelection: FakeAgentOptions = { provider: 'mock-provider', model: 'mock-model' }
  resumeCalls = 0
  createCalls = 0
  /** Let tests hold resume open to create a concurrency window. */
  resumeGate: Promise<void> = Promise.resolve()


  readonly agentsService = {
    get: (id: string) => this.agents.get(id),
    create: async (options: {
      sessionId: string
      meta: { cwd: string }
      agentOptions?: FakeAgentOptions
      setup?: (agentCtx: unknown, agent: FakeAgent) => Promise<void> | void
    }) => {
      this.createCalls += 1
      const agent = new FakeAgent(options.sessionId, options.meta.cwd, { ...options.agentOptions })
      await options.setup?.(this.agentCtx, agent)
      this.agents.set(agent.id, agent)
      return { agent, dispose: async () => { this.agents.delete(agent.id) } }
    },
    resume: async (options: {
      resumeSessionId: string
      agentOptions?: FakeAgentOptions
      setup?: (agentCtx: unknown) => Promise<void> | void
    }) => {
      this.resumeCalls += 1
      await this.resumeGate
      const header = this.persisted.get(options.resumeSessionId)
      if (header === undefined) throw new Error('not persisted')
      const agent = new FakeAgent(options.resumeSessionId, String(header['cwd']), { ...options.agentOptions })
      await options.setup?.(this.agentCtx)
      this.agents.set(agent.id, agent)
      return { agent, dispose: async () => { this.agents.delete(agent.id) } }
    },
  }

  readonly agentCtx = {
    systemPrompt: {
      section: () => () => {},
      getSectionOrder: () => 0,
    },
  }

  readonly sessions = {
    flush: async (session: FakeSession) => { this.flushed.push(session.id); return true },
  }

  readonly sessionPersistence = {
    stat: async (id: string) => {
      const header = this.persisted.get(id)
      return header === undefined ? undefined : { header, revision: 'r1' }
    },
  }

  readonly sessionQuery = {
    observeSession: async (id: string) => {
      const header = this.persisted.get(id) ?? {}
      return {
        header,
        events: this.persistedEvents.get(id) ?? [],
        cursor: -1,
        [Symbol.dispose]: () => {},
      }
    },
  }

  readonly agentPresets = {
    resolve: async () => ({ id: 'standard' }),
    mount: async () => ({ id: 'standard' }),
  }

  readonly agentDefaultModel = {
    currentSelection: () => ({ ...this.modelSelection }),
  }

  readonly workspaceRegistry = {
    create: async (path: string) => ({
      path,
      attachSession: async () => {},
      detachSession: async () => {},
    }),
  }

  readonly sessionTitle = {
    rename: () => ({ title: 'renamed' }),
    get: () => ({ title: 'persisted title' }),
  }

  /** The "Context" instance supplied to Bridge. */
  asContext(): never {
    return {
      agents: this.agentsService,
      sessions: this.sessions,
      sessionPersistence: this.sessionPersistence,
      sessionQuery: this.sessionQuery,
      agentPresets: this.agentPresets,
      agentDefaultModel: this.agentDefaultModel,
      workspaceRegistry: this.workspaceRegistry,
      sessionTitle: this.sessionTitle,
    } as never
  }
}

type Listener = (...args: unknown[]) => void

/** Fake socket: Connection only uses on / send / close. */
export class FakeSocket {
  readonly sent: Record<string, unknown>[] = []
  private readonly listeners = new Map<string, Listener[]>()

  on(event: string, listener: Listener): this {
    const bucket = this.listeners.get(event) ?? []
    bucket.push(listener)
    this.listeners.set(event, bucket)
    return this
  }

  send(raw: string): void {
    this.sent.push(JSON.parse(raw) as Record<string, unknown>)
  }

  close(): void {
    this.emit('close')
  }

  emit(event: string, ...args: unknown[]): void {
    for (const listener of this.listeners.get(event) ?? []) listener(...args)
  }

  /** Receive a frame sent by the client. */
  receive(frame: Record<string, unknown>): void {
    this.emit('message', JSON.stringify(frame))
  }

  /** Notifications already sent (those without an id). */
  notifications(method?: string): Record<string, unknown>[] {
    return this.sent.filter(frame => frame['method'] !== undefined && frame['id'] === undefined)
      .filter(frame => method === undefined || frame['method'] === method)
  }

  /** Server requests already sent (with both id and method). */
  requests(method?: string): Record<string, unknown>[] {
    return this.sent.filter(frame => frame['method'] !== undefined && frame['id'] !== undefined)
      .filter(frame => method === undefined || frame['method'] === method)
  }

  /** RPC responses already sent. */
  responses(): Record<string, unknown>[] {
    return this.sent.filter(frame => frame['method'] === undefined && frame['id'] !== undefined)
  }
}

export interface Harness {
  ctx: FakeContext
  bridge: Bridge
  methods: Map<string, MethodHandler>
  connect: () => { conn: Connection; socket: FakeSocket }
  call: (conn: Connection, method: string, params: Record<string, unknown>) => Promise<unknown>
}

export function createHarness(config: Partial<BridgeConfig> = {}): Harness {
  const ctx = new FakeContext()
  const bridge = new Bridge(
    ctx.asContext(),
    { host: '127.0.0.1', port: 0, agentPreset: 'standard', ...config },
    'http://127.0.0.1:12899/',
    () => {},
  )
  const methods = buildMethods(bridge)
  let nextId = 0
  return {
    ctx,
    bridge,
    methods,
    connect: () => {
      nextId += 1
      const socket = new FakeSocket()
      const conn = new Connection(nextId, socket as never, methods, () => {})
      bridge.addConnection(conn)
      socket.on('close', () => { bridge.removeConnection(conn) })
      return { conn, socket }
    },
    /** Call the method table directly, skipping frame encoding and decoding (dedicated tests cover the frame path). */
    call: async (conn, method, params) => {
      const handler = methods.get(method)
      if (handler === undefined) throw new Error(`unknown method ${method}`)
      return handler(conn, params)
    },
  }
}

/** Wait one macrotask so redeliveries scheduled by setTimeout(..., 0) can finish. */
export function tick(): Promise<void> {
  return new Promise(resolve => { setTimeout(resolve, 0) })
}
