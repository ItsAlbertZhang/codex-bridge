import { mkdir } from 'node:fs/promises'
import { isAbsolute } from 'node:path'
import { randomUUID } from 'node:crypto'
import type { Context } from '@deepseek-ai/cordis'
import type { Agent } from '@deepseek-ai/dsh-agent'
import { brandString } from '@deepseek-ai/dsh-brand'
import { createUserMessage } from '@deepseek-ai/dsh-llm'
import { setSandboxMode } from '@deepseek-ai/dsh-sandbox-policy'
import { setApprovalPolicy } from '@deepseek-ai/dsh-user-approval'
import type { Session, SessionEvent, SessionId } from '@deepseek-ai/dsh-session'
// Declaration merging: make ctx.<service> visible to the type system.
import type {} from '@deepseek-ai/dsh-agent-default-model'
import type {} from '@deepseek-ai/dsh-agent-presets'
import type {} from '@deepseek-ai/dsh-session-persistence'
import type {} from '@deepseek-ai/dsh-session-projection'
import type {} from '@deepseek-ai/dsh-session-query'
import type {} from '@deepseek-ai/dsh-session-title'
import type {} from '@deepseek-ai/dsh-workspace'

import {
  APPROVAL_DECISIONS,
  APPROVAL_POLICY_VALUES,
  APPROVAL_REQUEST_METHOD,
  BridgeError,
  FORWARDED_EVENT_TYPES,
  SANDBOX_MODE_VALUES,
  USER_QUESTION_REQUEST_METHOD,
  type ResolvedBy,
  type ThreadStatus,
  type TurnStatus,
} from './protocol.ts'
import { type Connection, RpcRemoteError } from './rpc.ts'

export interface BridgeConfig {
  host: string
  port: number
  agentPreset: string
}

/** One turn in flight for this plugin. */
interface Inflight {
  /** Id of the message sent by followup; `agent/inbox/claimed` uses it to claim the turn number. */
  messageId: string
  /** Filled in when `agent/inbox/claimed` arrives. */
  turnId?: number | undefined
  /** Text of the last non-empty assistant/message within the turn. */
  finalMessage?: string | undefined
  /** Resolve the turn/start RPC response once the turn number is available. */
  onClaimed?: ((turnId: number) => void) | undefined
}

/** A reverse request's outcome, translated into dsh's vocabulary by the dsh-side listeners. */
export type ReverseOutcome =
  | { kind: 'value'; value: unknown }
  /** A client response with a JSON-RPC error means explicit rejection. */
  | { kind: 'rejected'; message: string }
  /** `req.signal` aborted: the turn was interrupted or the tool call timed out. */
  | { kind: 'cancelled' }

/** A pending reverse request. Lives in plugin memory and remains unresolved if the client disconnects. */
export interface PendingRequest {
  readonly requestId: string
  readonly threadId: string
  readonly kind: 'approval' | 'userQuestion'
  /** JSON-RPC method sent to the client. */
  readonly method: string
  /** Params sent to the client, reused unchanged on redelivery. */
  readonly params: Record<string, unknown>
  /** Connections already reached, to avoid sending the same id twice. */
  readonly delivered: Set<Connection>
  settled: boolean
  settle: (outcome: ReverseOutcome) => void
  detachAbort?: (() => void) | undefined
}

interface Thread {
  threadId: string
  agent: Agent
  /** Only a create handle has a real disposer; resume binding to an agent created elsewhere uses a no-op. */
  dispose: () => Promise<void>
  cwd: string
  title: string
  inflight?: Inflight | undefined
  pending: Map<string, PendingRequest>
  subscribers: Set<Connection>
}

export interface StartThreadParams {
  cwd: string
  provider?: string
  model?: string
  reasoningEffort?: string
  sandbox?: string
  approval?: string
  developerInstructions?: string
  title?: string
}

/**
 * The thread table, reverse request table, and all operations. All dsh calls live here; rpc.ts only handles frames.
 * Listeners register once at the top level of apply, receive process-wide events, and filter each by whether its thread is registered with this plugin.
 */
export class Bridge {
  private readonly threads = new Map<string, Thread>()
  private readonly connections = new Set<Connection>()
  /** Deduplication gate for concurrent `thread/resume` calls with the same id. */
  private readonly resumes = new Map<string, Promise<Thread>>()
  private requestSeq = 0

  constructor(
    private readonly ctx: Context,
    private readonly config: BridgeConfig,
    /** web-app's UI URL (without a token), returned by initialize. */
    readonly uiUrl: string,
    private readonly log: (message: string, error?: unknown) => void,
  ) {}

  // ── Connection lifecycle ──────────────────────────────────────

  addConnection(conn: Connection): void {
    this.connections.add(conn)
  }

  /** Connection closed: only unsubscribe. Pending reverse requests **remain unresolved**, awaiting `thread/resume` redelivery. */
  removeConnection(conn: Connection): void {
    this.connections.delete(conn)
    for (const thread of this.threads.values()) thread.subscribers.delete(conn)
  }

  // ── Operations ────────────────────────────────────────────────

  initialize(): Record<string, unknown> {
    return {
      serverVersion: '0.2.0',
      dshVersion: '0.1.5-rc.2',
      uiUrl: this.uiUrl,
      profile: process.env['DSH_PROFILE'] ?? null,
      agentPreset: this.config.agentPreset,
    }
  }

  /**
   * The thread/start sequence:
   * validate cwd -> workspaceRegistry.create -> agentPresets.resolve -> agents.create
   * (append sandbox/approval, mount the preset, and install developerInstructions in setup)
   * -> workspace.attachSession -> sessionTitle.rename -> sessions.flush.
   */
  async startThread(conn: Connection, params: StartThreadParams): Promise<Record<string, unknown>> {
    const cwd = params.cwd
    if (typeof cwd !== 'string' || cwd.length === 0 || !isAbsolute(cwd)) {
      throw new BridgeError('invalid_cwd', `cwd must be an absolute path, got ${JSON.stringify(cwd)}`)
    }
    const sandbox = optionalEnum(params.sandbox, SANDBOX_MODE_VALUES, 'sandbox')
    const approval = optionalEnum(params.approval, APPROVAL_POLICY_VALUES, 'approval')
    try {
      await mkdir(cwd, { recursive: true })
    } catch (error: unknown) {
      throw new BridgeError('invalid_cwd', `cannot create cwd "${cwd}": ${String(error)}`)
    }

    const preset = await this.resolvePreset()
    const workspace = await this.ctx.workspaceRegistry.create(cwd)
    const sessionId = brandString<SessionId>(`bridge-${randomUUID()}`)
    const title = normalizeTitle(params.title) ?? `dsh-${sessionId.slice(7, 15)}`
    const instructions = typeof params.developerInstructions === 'string' && params.developerInstructions.trim() !== ''
      ? params.developerInstructions
      : undefined

    const handle = await this.ctx.agents.create({
      sessionId,
      meta: { cwd: workspace.path, agentPreset: preset.id },
      agentOptions: this.agentOptions(params.provider, params.model, params.reasoningEffort) as never,
      setup: async (agentCtx, agent) => {
        // Policy events must be appended before session/created to take precedence over permission-presets' default pinning
        // (packages/interaction/permission-presets/src/index.ts:402-428 only fills in missing facts).
        if (sandbox !== undefined) setSandboxMode(agent.session, sandbox as never)
        if (approval !== undefined) setApprovalPolicy(agent.session, approval as never)
        await this.ctx.agentPresets.mount(agentCtx, preset.id)
        if (instructions !== undefined) {
          // Use our own section name, not `deployment:persona-prefix`: a matching name would shadow the preset's own persona.
          agentCtx.systemPrompt.section({
            name: 'dsh-bridge:developer-instructions',
            order: agentCtx.systemPrompt.getSectionOrder('DEPLOYMENT_PERSONA_PREFIX') + 1,
            text: instructions,
          })
        }
      },
    })

    let attached = false
    try {
      await workspace.attachSession(sessionId)
      attached = true
      this.ctx.sessionTitle.rename(handle.agent.session, title)
      await this.ctx.sessions.flush(handle.agent.session)
    } catch (error: unknown) {
      if (attached) {
        try {
          await workspace.detachSession(sessionId)
        } catch (rollback: unknown) {
          this.log('workspace detach failed during rollback', rollback)
        }
      }
      try {
        await handle.dispose()
      } catch (rollback: unknown) {
        this.log('agent dispose failed during rollback', rollback)
      }
      throw error
    }

    this.register(conn, {
      threadId: sessionId,
      agent: handle.agent,
      dispose: () => handle.dispose(),
      cwd: workspace.path,
      title,
      pending: new Map(),
      subscribers: new Set(),
    })
    return { threadId: sessionId, title, cwd: workspace.path, ...modelFields(handle.agent.options) }
  }

  /**
   * Subscribe to an existing thread. Bind directly if live; restore from persistence if cold (deduplicate concurrent calls for the same id).
   * The result includes `pendingRequests`; the caller invokes `redeliverPending` **after sending the response** to redeliver them.
   */
  async resumeThread(conn: Connection, threadId: string): Promise<Record<string, unknown>> {
    if (typeof threadId !== 'string' || threadId === '') {
      throw new BridgeError('invalid_params', 'thread/resume requires a threadId')
    }
    const known = this.threads.get(threadId)
    const thread = known ?? await this.resumeUnknown(threadId)
    this.subscribe(conn, thread)
    return {
      threadId,
      status: thread.agent.status satisfies ThreadStatus,
      cwd: thread.cwd,
      title: thread.title,
      ...modelFields(thread.agent.options),
      pendingRequests: [...thread.pending.keys()],
    }
  }

  /** Redeliver all pending reverse requests for this thread to this connection, reusing their ids. Call after the thread/resume response. */
  redeliverPending(conn: Connection, threadId: string): void {
    const thread = this.threads.get(threadId)
    if (thread === undefined) return
    for (const pending of thread.pending.values()) this.deliver(thread, pending, conn)
  }

  /** Check whether busy, call followup, then await `agent/inbox/claimed` for the turn number. */
  async startTurn(conn: Connection, threadId: string, text: string): Promise<Record<string, unknown>> {
    const thread = this.mustGet(threadId)
    this.subscribe(conn, thread)
    const message = this.userMessage(text, 'turn/start')
    // agent.status is a synchronous getter and followup a synchronous method: reading status and submitting within one tick is atomic.
    // A browser-originated turn sets agent.status to running, so this also prevents a race with human input.
    if (thread.inflight !== undefined || thread.agent.status === 'running') {
      throw new BridgeError('thread_busy', `thread "${threadId}" is busy`)
    }
    if (this.ctx.agents.get(brandString<SessionId>(threadId)) !== thread.agent) {
      throw new BridgeError('thread_not_found', `thread "${threadId}" is no longer live`)
    }
    const inflight: Inflight = { messageId: message.id }
    thread.inflight = inflight
    const claimed = new Promise<number>((resolve, reject) => {
      inflight.onClaimed = resolve
      const timer = setTimeout(() => { reject(new Error('timed out waiting for the turn to be claimed')) }, 30_000)
      timer.unref?.()
    })
    thread.agent.followup(message)
    try {
      return { turnId: await claimed }
    } catch (error: unknown) {
      if (thread.inflight === inflight) thread.inflight = undefined
      throw new BridgeError('internal', error instanceof Error ? error.message : String(error))
    }
  }

  /** Incorporate the text at the next step boundary. Reject if the thread is not running. */
  steerTurn(conn: Connection, threadId: string, text: string, expectedTurnId?: number): Record<string, unknown> {
    const thread = this.mustGet(threadId)
    this.subscribe(conn, thread)
    const message = this.userMessage(text, 'turn/steer')
    if (thread.agent.status !== 'running') {
      throw new BridgeError('no_active_turn', `thread "${threadId}" has no running turn to steer`)
    }
    const current = thread.inflight?.turnId ?? lastTurnOf(thread.agent.session)
    if (expectedTurnId !== undefined && current !== expectedTurnId) {
      throw new BridgeError('turn_mismatch', `thread "${threadId}" is on turn ${String(current)}, not ${expectedTurnId}`)
    }
    thread.agent.steer(message)
    return { turnId: current ?? null }
  }

  /** Interrupt the in-flight turn. Aborting `req.signal` also cleans up pending reverse requests. */
  interruptTurn(conn: Connection, threadId: string, turnId?: number): Record<string, unknown> {
    const thread = this.mustGet(threadId)
    this.subscribe(conn, thread)
    if (thread.inflight === undefined && thread.agent.status !== 'running') {
      throw new BridgeError('no_active_turn', `thread "${threadId}" has no active turn`)
    }
    const current = thread.inflight?.turnId ?? lastTurnOf(thread.agent.session)
    if (turnId !== undefined && current !== undefined && current !== turnId) {
      throw new BridgeError('turn_mismatch', `thread "${threadId}" is on turn ${String(current)}, not ${turnId}`)
    }
    thread.agent.cancel({ kind: 'user' }, { keepInbox: true })
    return {}
  }

  /** Live threads use agent.status and folded logs; cold sessions use sessionQuery without activating the agent. */
  async readThread(threadId: string, includeTurns: boolean): Promise<Record<string, unknown>> {
    const thread = this.threads.get(threadId)
    if (thread !== undefined) {
      const events = thread.agent.session.snapshotEvents()
      return {
        threadId,
        status: thread.agent.status satisfies ThreadStatus,
        cwd: thread.cwd,
        title: thread.title,
        ...modelFields(thread.agent.options),
        pendingRequests: [...thread.pending.keys()],
        ...includeTurns ? { turns: foldTurns(events), events: summarizeEvents(events) } : {},
      }
    }
    const sessionId = brandString<SessionId>(threadId)
    const live = this.ctx.agents.get(sessionId)
    const observation = await this.ctx.sessionQuery.observeSession(sessionId, { projectionMode: 'none' })
    try {
      if (observation.header.cwd === undefined) {
        throw new BridgeError('thread_not_found', `thread "${threadId}" does not exist`)
      }
      return {
        threadId,
        status: (live?.status ?? 'idle') satisfies ThreadStatus,
        cwd: observation.header.cwd,
        title: null,
        ...live !== undefined ? modelFields(live.options) : persistedModelFields(observation.events),
        pendingRequests: [],
        ...includeTurns
          ? { turns: foldTurns(observation.events), events: summarizeEvents(observation.events) }
          : {},
      }
    } finally {
      observation[Symbol.dispose]()
    }
  }

  unsubscribe(conn: Connection, threadId: string): Record<string, unknown> {
    conn.subscriptions.delete(threadId)
    this.threads.get(threadId)?.subscribers.delete(conn)
    return {}
  }

  // ── dsh events -> client notifications ─────────────────────────

  onSessionEvent(session: Session, event: SessionEvent): void {
    const thread = this.threads.get(session.id)
    if (thread === undefined) return
    if (event.type === 'assistant/message') {
      const data = event.data as { turn: number; message: { content?: unknown } }
      const text = joinTextBlocks(data.message?.content)
      // Empty messages do not overwrite previous output (the rule in subagent/assistant-output.ts:1-11).
      if (text !== '' && thread.inflight !== undefined && thread.inflight.turnId === data.turn) {
        thread.inflight.finalMessage = text
      }
    }
    if (FORWARDED_EVENT_TYPES.includes(event.type)) {
      this.broadcast(thread, 'thread/event', {
        threadId: thread.threadId,
        seq: event.seq,
        type: event.type,
        data: event.data,
      })
    }
    if (event.type !== 'turn/end') return
    const data = event.data as { turn: number; reason: { kind: string; reason?: unknown; error?: unknown } }
    const inflight = thread.inflight
    // Turns submitted by a human in the browser also reach here, but have no corresponding inflight,
    // so they only produce thread/event, not turn/completed.
    if (inflight === undefined || inflight.turnId !== data.turn) return
    thread.inflight = undefined
    const { status, error } = mapTurnEnd(data.reason)
    this.broadcast(thread, 'turn/completed', {
      threadId: thread.threadId,
      turnId: data.turn,
      status,
      reason: data.reason.kind,
      ...error !== undefined ? { error } : {},
      ...inflight.finalMessage !== undefined ? { finalMessage: inflight.finalMessage } : {},
    })
    void this.ctx.sessions.flush(thread.agent.session).catch((flushError: unknown) => {
      this.log('flush after turn failed', flushError)
    })
  }

  onAgentStatus(agent: Agent, status: ThreadStatus): void {
    const thread = this.threads.get(agent.id)
    if (thread === undefined) return
    this.broadcast(thread, 'thread/status', { threadId: thread.threadId, status })
  }

  onInboxClaimed(agent: Agent, messageId: string, turn: number): void {
    const thread = this.threads.get(agent.id)
    const inflight = thread?.inflight
    // A messageId we did not submit belongs to a human's browser turn; do not claim it.
    if (thread === undefined || inflight === undefined || inflight.messageId !== messageId) return
    inflight.turnId = turn
    this.broadcast(thread, 'turn/started', { threadId: thread.threadId, turnId: turn })
    inflight.onClaimed?.(turn)
    inflight.onClaimed = undefined
  }

  onAgentError(agent: Agent, turn: number, error: unknown): void {
    const thread = this.threads.get(agent.id)
    if (thread === undefined) return
    this.broadcast(thread, 'thread/event', {
      threadId: thread.threadId,
      seq: null,
      type: 'agent/error',
      data: { turn, error: error instanceof Error ? error.message : String(error) },
    })
  }

  // ── Reverse requests (README: Request routing) ─────────────────

  /**
   * Ownership check: the agent is a thread registered with this plugin and currently has an in-flight turn initiated by the bridge.
   * Otherwise call `next()`; the request reaches the gateway unchanged and opens a browser dialog.
   */
  private ownedThread(agent: Agent | undefined): Thread | undefined {
    if (agent === undefined) return undefined
    const thread = this.threads.get(agent.id)
    if (thread === undefined || thread.inflight === undefined) return undefined
    return thread
  }

  /**
   * The `approval/request` listener. Its return value must be in dsh's approval vocabulary
   * (`'allowed-once' | 'rejected' | 'cancelled'`); invalid values are normalized to `'unavailable'`.
   */
  async onApprovalRequest(
    request: { agent?: Agent; toolName?: string; callId?: string; reason?: string; signal?: AbortSignal },
    next: () => Promise<string>,
  ): Promise<string> {
    const thread = this.ownedThread(request.agent)
    if (thread === undefined) return next()
    const params: Record<string, unknown> = {
      threadId: thread.threadId,
      toolName: request.toolName ?? 'unknown',
      ...request.callId === undefined ? {} : { callId: request.callId },
      ...request.reason === undefined ? {} : { reason: request.reason },
    }
    const outcome = await this.awaitReverse(thread, 'approval', APPROVAL_REQUEST_METHOD, params, request.signal)
    if (outcome.kind === 'cancelled') return 'cancelled'
    if (outcome.kind === 'rejected') return 'rejected'
    const decision = (outcome.value as { decision?: unknown } | null | undefined)?.decision
    if (typeof decision === 'string' && APPROVAL_DECISIONS.includes(decision)) return decision
    this.log(`approval answer ${JSON.stringify(decision)} is not in the vocabulary; treating it as rejected`)
    return 'rejected'
  }

  /**
   * The `user-questions/request` listener. Return `AskUserQuestionAnswer` on success;
   * rejection, cancellation, or an invalid shape all throw: the waterfall rejection gives the tool an error result.
   */
  async onUserQuestionRequest(
    request: { agent?: Agent; questions?: unknown; signal?: AbortSignal },
    next: () => Promise<unknown>,
  ): Promise<unknown> {
    const thread = this.ownedThread(request.agent)
    if (thread === undefined) return next()
    const params: Record<string, unknown> = {
      threadId: thread.threadId,
      questions: request.questions ?? [],
    }
    const outcome = await this.awaitReverse(thread, 'userQuestion', USER_QUESTION_REQUEST_METHOD, params, request.signal)
    if (outcome.kind === 'cancelled') throw new Error('dsh-bridge: the user-question request was cancelled')
    if (outcome.kind === 'rejected') throw new Error(`dsh-bridge: ${outcome.message}`)
    const answers = (outcome.value as { answers?: unknown } | null | undefined)?.answers
    if (!Array.isArray(answers)) {
      throw new Error('dsh-bridge: the client answer must be {answers: [{id, selected, custom?}]}')
    }
    return { answers }
  }

  /** Register a pending request, fan out to all subscribed connections, and wait for an answer or signal abort. */
  private awaitReverse(
    thread: Thread,
    kind: PendingRequest['kind'],
    method: string,
    payload: Record<string, unknown>,
    signal: AbortSignal | undefined,
  ): Promise<ReverseOutcome> {
    this.requestSeq += 1
    const requestId = `req-${this.requestSeq}`
    return new Promise<ReverseOutcome>((resolve) => {
      const pending: PendingRequest = {
        requestId,
        threadId: thread.threadId,
        kind,
        method,
        params: { ...payload, requestId },
        delivered: new Set(),
        settled: false,
        settle: (outcome) => {
          if (pending.settled) return
          pending.settled = true
          thread.pending.delete(requestId)
          pending.detachAbort?.()
          resolve(outcome)
        },
      }
      thread.pending.set(requestId, pending)

      if (signal !== undefined) {
        if (signal.aborted) {
          this.announceResolved(thread, pending, 'cancelled')
          pending.settle({ kind: 'cancelled' })
          return
        }
        const onAbort = (): void => {
          this.announceResolved(thread, pending, 'cancelled')
          pending.settle({ kind: 'cancelled' })
        }
        signal.addEventListener('abort', onAbort, { once: true })
        pending.detachAbort = () => { signal.removeEventListener('abort', onAbort) }
      }

      for (const conn of thread.subscribers) this.deliver(thread, pending, conn)
    })
  }

  /** Deliver a pending request to one connection. Skip connections already reached (redelivery reuses the same id). */
  private deliver(thread: Thread, pending: PendingRequest, conn: Connection): void {
    if (pending.settled || conn.isClosed || pending.delivered.has(conn)) return
    pending.delivered.add(conn)
    conn.request(pending.requestId, pending.method, pending.params).then(
      (value: unknown) => {
        if (pending.settled) return // Late response: already resolved elsewhere, so ignore it.
        this.announceResolved(thread, pending, 'client', conn)
        pending.settle({ kind: 'value', value })
      },
      (error: unknown) => {
        // A JSON-RPC error response means explicit rejection (the Rust CLI's --auto-decline sends this).
        // Other failures (connection closed, request withdrawn) leave it unresolved: retain the pending request for thread/resume redelivery.
        if (!(error instanceof RpcRemoteError) || pending.settled) return
        this.announceResolved(thread, pending, 'client', conn)
        pending.settle({ kind: 'rejected', message: error.message })
      },
    )
  }

  /** Notify the other subscribers that this request was resolved elsewhere, and withdraw their in-flight requests. */
  private announceResolved(
    thread: Thread,
    pending: PendingRequest,
    resolvedBy: ResolvedBy,
    winner?: Connection,
  ): void {
    for (const conn of thread.subscribers) {
      if (conn === winner || conn.isClosed) continue
      if (conn.hasRequest(pending.requestId)) {
        conn.cancelRequest(pending.requestId, `request ${pending.requestId} resolved elsewhere`)
      }
      conn.notify('request/resolved', {
        threadId: thread.threadId,
        requestId: pending.requestId,
        resolvedBy,
      })
    }
  }

  async dispose(): Promise<void> {
    for (const conn of this.connections) conn.close()
    this.connections.clear()
  }

  // ── Internals ─────────────────────────────────────────────────

  private mustGet(threadId: string): Thread {
    const thread = this.threads.get(threadId)
    if (thread === undefined) throw new BridgeError('thread_not_found', `thread "${threadId}" is not registered`)
    return thread
  }

  private register(conn: Connection, thread: Thread): Thread {
    this.threads.set(thread.threadId, thread)
    this.subscribe(conn, thread)
    return thread
  }

  private subscribe(conn: Connection, thread: Thread): void {
    conn.subscriptions.add(thread.threadId)
    thread.subscribers.add(conn)
  }

  private userMessage(text: string, method: string): ReturnType<typeof createUserMessage> {
    if (typeof text !== 'string' || text.trim() === '') {
      throw new BridgeError('invalid_params', `${method} requires non-empty text`)
    }
    return createUserMessage({ content: [{ type: 'text', text }], source: { kind: 'user' } })
  }

  private async resolvePreset(): Promise<{ id: string; broken?: string }> {
    const preset = await this.ctx.agentPresets.resolve(this.config.agentPreset)
    if (preset.broken !== undefined) {
      throw new BridgeError('internal', `agent preset "${preset.id}" is unusable: ${preset.broken}`)
    }
    return preset
  }

  /**
   * provider/model must be supplied explicitly: the preset's persona contains a `{{model}}` variable,
   * so prompt assembly fails outright without routing. When omitted, use the deployment's default selection
   * (matching packages/webhook/webhook/src/session.ts:62-66).
   */
  private agentOptions(provider: string | undefined, model: string | undefined, effort: string | undefined): Record<string, unknown> {
    const selected = this.ctx.agentDefaultModel.currentSelection()
    const options: Record<string, unknown> = {
      provider: typeof provider === 'string' && provider !== '' ? provider : selected.provider,
      model: typeof model === 'string' && model !== '' ? model : selected.model,
    }
    const reasoningEffort = typeof effort === 'string' && effort !== '' ? effort : selected.reasoningEffort
    if (reasoningEffort !== undefined) options['reasoningEffort'] = reasoningEffort
    return options
  }

  /** Thread absent from the table: either a live agent created elsewhere or one requiring cold restoration from persistence. Deduplicate concurrent calls for the same id. */
  private async resumeUnknown(threadId: string): Promise<Thread> {
    const inFlight = this.resumes.get(threadId)
    if (inFlight !== undefined) return inFlight
    const task = this.doResumeUnknown(threadId).finally(() => { this.resumes.delete(threadId) })
    this.resumes.set(threadId, task)
    return task
  }

  private async doResumeUnknown(threadId: string): Promise<Thread> {
    const sessionId = brandString<SessionId>(threadId)
    const snapshot = await this.ctx.sessionPersistence.stat(sessionId)
    const header = snapshot?.header
    if (header === undefined) {
      throw new BridgeError('thread_not_found', `thread "${threadId}" does not exist`)
    }
    if (header.origin === 'subagent' || header.parentSession !== undefined) {
      throw new BridgeError('not_resumable', `thread "${threadId}" is a subagent session`)
    }
    if (header.cwd === undefined) {
      throw new BridgeError('not_resumable', `thread "${threadId}" has no cwd`)
    }

    // Already live (opened in the browser or resumed elsewhere in this process): bind directly; do not resume again.
    // The persistence write lock is exclusive, so a second resume throws SessionAlreadyOwnedError.
    const live = this.ctx.agents.get(sessionId)
    if (live !== undefined) {
      return this.adopt(live, header.cwd, threadId, async () => {})
    }

    const preset = await this.resolvePreset()
    let handle
    try {
      handle = await this.ctx.agents.resume({
        resumeSessionId: sessionId,
        agentOptions: this.agentOptions(undefined, undefined, undefined) as never,
        // Policy events are replayed from the log, so do not append them again; just remount the preset.
        setup: async (agentCtx) => { await this.ctx.agentPresets.mount(agentCtx, preset.id) },
      })
    } catch (error: unknown) {
      throw new BridgeError('not_resumable', `cannot resume "${threadId}": ${error instanceof Error ? error.message : String(error)}`)
    }
    return this.adopt(handle.agent, header.cwd, threadId, () => handle.dispose())
  }

  private adopt(agent: Agent, cwd: string, threadId: string, dispose: () => Promise<void>): Thread {
    const existing = this.threads.get(threadId)
    if (existing !== undefined) return existing
    const thread: Thread = {
      threadId,
      agent,
      dispose,
      cwd,
      title: this.ctx.sessionTitle.get(agent.session)?.title ?? `dsh-${threadId.slice(7, 15)}`,
      pending: new Map(),
      subscribers: new Set(),
    }
    this.threads.set(threadId, thread)
    return thread
  }

  private broadcast(thread: Thread, method: string, params: unknown): void {
    for (const conn of thread.subscribers) {
      if (conn.isClosed) {
        thread.subscribers.delete(conn)
        continue
      }
      conn.notify(method, params)
    }
  }
}

// ── Pure functions ──────────────────────────────────────────────

/** Report only available routing fields, without inferring missing values. */
function modelFields(options: Agent['options'] | undefined): Record<string, unknown> {
  const fields: Record<string, unknown> = {}
  for (const key of ['provider', 'model', 'reasoningEffort'] as const) {
    const value = options?.[key]
    if (typeof value === 'string') fields[key] = value
  }
  return fields
}

/** The latest full request header records a cold session's last known route. */
function persistedModelFields(events: readonly SessionEvent[]): Record<string, unknown> {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]
    if (event?.type === 'request/header') return modelFields(event.data?.header?.config)
  }
  return {}
}

function optionalEnum(value: unknown, allowed: readonly string[], label: string): string | undefined {
  if (value === undefined || value === null) return undefined
  if (typeof value !== 'string' || !allowed.includes(value)) {
    throw new BridgeError('invalid_params', `${label} must be one of ${allowed.join(' / ')}`)
  }
  return value
}

function normalizeTitle(title: unknown): string | undefined {
  if (typeof title !== 'string') return undefined
  const trimmed = title.trim()
  return trimmed === '' ? undefined : trimmed
}

function joinTextBlocks(content: unknown): string {
  if (!Array.isArray(content)) return ''
  const parts: string[] = []
  for (const block of content) {
    if (typeof block === 'object' && block !== null && (block as { type?: unknown }).type === 'text') {
      const text = (block as { text?: unknown }).text
      if (typeof text === 'string') parts.push(text)
    }
  }
  return parts.join('')
}

/** The turn number of the log's last `turn/start`; also recognizes browser-originated turns. */
function lastTurnOf(session: Session): number | undefined {
  const events = session.snapshotEvents()
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]
    if (event?.type === 'turn/start') return (event.data as { turn: number }).turn
  }
  return undefined
}

function mapTurnEnd(reason: { kind: string; error?: unknown }): { status: TurnStatus; error?: string } {
  switch (reason.kind) {
    case 'completed':
      return { status: 'completed' }
    case 'aborted':
    case 'interrupted':
      return { status: 'interrupted' }
    default: {
      const error = reason.error === undefined ? reason.kind : JSON.stringify(reason.error)
      return { status: 'failed', error }
    }
  }
}

/** Fold the log into turns, each carrying status / reason / finalMessage. */
function foldTurns(events: readonly SessionEvent[]): unknown[] {
  const turns = new Map<number, { turnId: number; status: string; reason?: string; finalMessage?: string }>()
  for (const event of events) {
    if (event.type === 'turn/start') {
      const turn = (event.data as { turn: number }).turn
      turns.set(turn, { turnId: turn, status: 'running' })
      continue
    }
    if (event.type === 'assistant/message') {
      const data = event.data as { turn: number; message: { content?: unknown } }
      const text = joinTextBlocks(data.message?.content)
      const record = turns.get(data.turn)
      if (record !== undefined && text !== '') record.finalMessage = text
      continue
    }
    if (event.type === 'turn/end') {
      const data = event.data as { turn: number; reason: { kind: string } }
      const record = turns.get(data.turn)
      if (record === undefined) continue
      record.status = mapTurnEnd(data.reason).status
      record.reason = data.reason.kind
    }
  }
  return [...turns.values()]
}

/** A compact event view for verifying policy event order (debugging interface). */
function summarizeEvents(events: readonly SessionEvent[]): unknown[] {
  return events.map(event => ({ seq: event.seq, type: event.type, data: compactData(event) }))
}

function compactData(event: SessionEvent): unknown {
  switch (event.type as string) {
    case 'sandbox/mode':
    case 'approval/policy':
    case 'permission/preset':
    case 'approval/asked':
    case 'approval/decided':
    case 'session/title':
    case 'turn/start':
    case 'turn/end':
    case 'step/start':
    case 'step/end':
      return event.data
    case 'tool/call':
      return { name: (event.data as { name: string }).name, callId: (event.data as { callId: string }).callId }
    default:
      return undefined
  }
}
