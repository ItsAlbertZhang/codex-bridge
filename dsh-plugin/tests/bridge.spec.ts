import { mkdir, rm } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { afterAll, beforeAll, describe, expect, it } from 'vitest'

import { BRIDGE_ERROR_CODE } from '../src/protocol.ts'
import { createHarness, FakeAgent, FakeSession, tick, type Harness } from './harness.ts'

const WORK = fileURLToPath(new URL('../.m0/vitest-work', import.meta.url))

beforeAll(async () => { await mkdir(WORK, { recursive: true }) })
afterAll(async () => { await rm(WORK, { recursive: true, force: true }) })

/** Create a thread and return {threadId, agent}. */
async function startThread(h: Harness, conn: Parameters<Harness['call']>[0]): Promise<{
  threadId: string
  agent: FakeAgent
}> {
  const result = await h.call(conn, 'thread/start', {
    cwd: WORK,
    sandbox: 'workspace-write',
    approval: 'ask',
    title: 'spec',
  }) as { threadId: string }
  const agent = h.ctx.agents.get(result.threadId)
  if (agent === undefined) throw new Error('agent missing')
  return { threadId: result.threadId, agent }
}

/** Submit a turn and have it claimed, returning turnId. */
async function claimTurn(h: Harness, conn: Parameters<Harness['call']>[0], threadId: string, agent: FakeAgent, turn = 1): Promise<number> {
  const started = h.call(conn, 'turn/start', { threadId, text: 'hello' })
  await tick()
  const messageId = agent.followups.at(-1)?.id
  if (messageId === undefined) throw new Error('followup missing')
  agent.status = 'running'
  h.bridge.onInboxClaimed(agent as never, messageId, turn)
  const value = await started as { turnId: number }
  return value.turnId
}

/** Trigger approval/request and return the promise received by the dsh side. */
function askApproval(h: Harness, agent: FakeAgent, signal?: AbortSignal): Promise<string> {
  return h.bridge.onApprovalRequest(
    { agent: agent as never, toolName: 'write', callId: 'call-1', reason: 'outside the workspace', ...signal === undefined ? {} : { signal } },
    async () => 'unavailable',
  )
}

function errorKind(error: unknown): string {
  const data = (error as { toBody?: () => { data?: { kind?: string } } }).toBody?.()
  return data?.data?.kind ?? 'none'
}

describe('thread lifecycle', () => {
  it('starts a thread, appends the policy events before the preset mount, and flushes', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    expect(threadId.startsWith('bridge-')).toBe(true)
    expect(agent.session.events.map(event => event.type)).toEqual(['sandbox/mode', 'approval/policy'])
    expect(h.ctx.flushed).toEqual([threadId])
  })

  it('rejects a relative cwd with invalid_cwd', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    await expect(h.call(conn, 'thread/start', { cwd: 'relative/path' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'invalid_cwd',
    )
  })

  it('rejects an unknown thread with thread_not_found', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    await expect(h.call(conn, 'turn/start', { threadId: 'nope', text: 'hi' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'thread_not_found',
    )
  })

  it('rejects empty turn text with invalid_params', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId } = await startThread(h, conn)
    await expect(h.call(conn, 'turn/start', { threadId, text: '   ' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'invalid_params',
    )
  })
})

describe('model routing', () => {
  it('uses the default selection and reports all routing fields in start, read, and resume', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const selected = { provider: 'mock-provider', model: 'mock-model', reasoningEffort: 'medium' }
    h.ctx.modelSelection = selected

    const started = await h.call(conn, 'thread/start', { cwd: WORK }) as Record<string, unknown>
    expect(started).toMatchObject(selected)
    expect(h.ctx.agents.get(String(started['threadId']))?.options).toEqual(selected)
    for (const method of ['thread/read', 'thread/resume']) {
      expect(await h.call(conn, method, { threadId: started['threadId'] })).toMatchObject(selected)
    }
  })

  it('passes explicit provider, model, and effort overrides to the agent', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    h.ctx.modelSelection.reasoningEffort = 'low'
    const selected = { provider: 'mock-provider-override', model: 'mock-model-override', reasoningEffort: 'high' }

    const started = await h.call(conn, 'thread/start', { cwd: WORK, ...selected }) as Record<string, unknown>
    expect(h.ctx.agents.get(String(started['threadId']))?.options).toEqual(selected)
    expect(started).toMatchObject(selected)
  })

  it('overrides the provider while preserving the default model and effort', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    h.ctx.modelSelection.reasoningEffort = 'medium'

    const started = await h.call(conn, 'thread/start', { cwd: WORK, provider: 'mock-provider-override' })
    expect(started).toMatchObject({ provider: 'mock-provider-override', model: 'mock-model', reasoningEffort: 'medium' })
  })

  it.each(['', 123])('uses the default provider for a non-usable override %s', async (provider) => {
    const h = createHarness()
    const { conn } = h.connect()
    const started = await h.call(conn, 'thread/start', { cwd: WORK, provider })
    expect(started).toMatchObject({ provider: 'mock-provider', model: 'mock-model' })
  })

  it('reports actual agent options after creation and subsequent live changes', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const create = h.ctx.agentsService.create
    const actual = { provider: 'mock-provider-actual', model: 'mock-model-actual', reasoningEffort: 'high' }
    h.ctx.agentsService.create = async (options) => {
      const handle = await create(options)
      handle.agent.options = actual
      return handle
    }

    const started = await h.call(conn, 'thread/start', {
      cwd: WORK, provider: 'mock-provider-requested', model: 'mock-model-requested', reasoningEffort: 'low',
    }) as Record<string, unknown>
    expect(started).toMatchObject(actual)
    const agent = h.ctx.agents.get(String(started['threadId']))
    if (agent === undefined) throw new Error('agent missing')
    agent.options = { provider: 'mock-provider-current', model: 'mock-model-current' }
    for (const method of ['thread/read', 'thread/resume']) {
      const response = await h.call(conn, method, { threadId: started['threadId'] })
      expect(response).toMatchObject(agent.options)
      expect(response).not.toHaveProperty('reasoningEffort')
    }
  })

  it('reads an unregistered live agent from its options instead of historical configuration', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const selected = { provider: 'mock-provider-live', model: 'mock-model-live', reasoningEffort: 'high' }
    const agent = new FakeAgent('external-thread', WORK, selected)
    h.ctx.agents.set(agent.id, agent)
    h.ctx.persisted.set(agent.id, agent.session.header)
    agent.session.append('request/header', { header: { config: { provider: 'mock-provider-old', model: 'mock-model-old' } } })
    h.ctx.persistedEvents.set(agent.id, agent.session.events)

    expect(await h.call(conn, 'thread/read', { threadId: agent.id })).toMatchObject(selected)
    expect(h.ctx.createCalls).toBe(0)
    expect(h.ctx.resumeCalls).toBe(0)
  })

  it('reports the restored agent options after a cold resume', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const selected = { provider: 'mock-provider', model: 'mock-model', reasoningEffort: 'medium' }
    h.ctx.modelSelection = selected
    h.ctx.persisted.set('cold-thread', { id: 'cold-thread', cwd: WORK })

    const resumed = await h.call(conn, 'thread/resume', { threadId: 'cold-thread' })
    expect(resumed).toMatchObject(selected)
    expect(h.ctx.agents.get('cold-thread')?.options).toEqual(selected)
    expect(h.ctx.resumeCalls).toBe(1)
  })

  it('reads the latest persisted request configuration without activating a cold agent', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const session = new FakeSession('cold-thread', { id: 'cold-thread', cwd: WORK })
    const selected = { provider: 'mock-provider-history', model: 'mock-model-history', reasoningEffort: 'high' }
    session.append('request/header', { header: { config: { provider: 'mock-provider-old', model: 'mock-model-old' } }, reason: 'initial' })
    session.append('request/header', { header: { config: selected }, reason: 'change' })
    session.append('turn/end', { turn: 1, reason: { kind: 'completed' } })
    h.ctx.persisted.set(session.id, session.header)
    h.ctx.persistedEvents.set(session.id, session.events)

    for (const includeTurns of [false, true]) {
      expect(await h.call(conn, 'thread/read', { threadId: session.id, includeTurns })).toMatchObject(selected)
    }
    expect(h.ctx.createCalls).toBe(0)
    expect(h.ctx.resumeCalls).toBe(0)
    expect(h.ctx.agents.size).toBe(0)
  })

  it('does not restore effort omitted from the latest full request header', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const session = new FakeSession('cold-thread', { id: 'cold-thread', cwd: WORK })
    session.append('request/header', { header: { config: { provider: 'mock-provider-old', model: 'mock-model-old', reasoningEffort: 'high' } } })
    session.append('request/header', { header: { config: { provider: 'mock-provider-current', model: 'mock-model-current' } } })
    h.ctx.persisted.set(session.id, session.header)
    h.ctx.persistedEvents.set(session.id, session.events)

    const read = await h.call(conn, 'thread/read', { threadId: session.id })
    expect(read).toMatchObject({ provider: 'mock-provider-current', model: 'mock-model-current' })
    expect(read).not.toHaveProperty('reasoningEffort')
    expect(h.ctx.agents.size).toBe(0)
    expect(h.ctx.resumeCalls).toBe(0)
  })

  it('omits unavailable cold routing fields instead of substituting deployment defaults', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    h.ctx.modelSelection.reasoningEffort = 'high'
    h.ctx.persisted.set('cold-thread', { id: 'cold-thread', cwd: WORK })

    const read = await h.call(conn, 'thread/read', { threadId: 'cold-thread' })
    for (const field of ['provider', 'model', 'reasoningEffort']) expect(read).not.toHaveProperty(field)
    expect(h.ctx.createCalls).toBe(0)
    expect(h.ctx.resumeCalls).toBe(0)
    expect(h.ctx.agents.size).toBe(0)
  })

  it('omits unavailable routing from partial persisted headers without falling back or activating', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const session = new FakeSession('cold-thread', { id: 'cold-thread', cwd: WORK })
    session.append('request/header', { header: { config: { provider: 'mock-provider-old', model: 'mock-model-old', reasoningEffort: 'high' } } })
    h.ctx.persisted.set(session.id, session.header)
    h.ctx.persistedEvents.set(session.id, session.events)

    const partials = [
      {},
      { header: null },
      { header: {} },
      { header: { config: null } },
    ]
    for (const data of partials) {
      session.append('request/header', data)
      const read = await h.call(conn, 'thread/read', { threadId: session.id })
      for (const field of ['provider', 'model', 'reasoningEffort']) expect(read).not.toHaveProperty(field)
    }
    session.append('request/header', { header: { config: { model: 'mock-model-partial' } } })
    const partial = await h.call(conn, 'thread/read', { threadId: session.id })
    expect(partial).toMatchObject({ model: 'mock-model-partial' })
    expect(partial).not.toHaveProperty('provider')
    expect(partial).not.toHaveProperty('reasoningEffort')
    expect(h.ctx.createCalls).toBe(0)
    expect(h.ctx.resumeCalls).toBe(0)
    expect(h.ctx.agents.size).toBe(0)
  })
})

describe('busy detection', () => {
  it('rejects a second turn while one is in flight', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent)
    await expect(h.call(conn, 'turn/start', { threadId, text: 'again' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'thread_busy',
    )
  })

  it('rejects a turn while the agent runs a browser-originated turn', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    agent.status = 'running' // A human submitted a message in the browser.
    await expect(h.call(conn, 'turn/start', { threadId, text: 'mine' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'thread_busy',
    )
  })
})

describe('browser-originated turns', () => {
  it('forwards thread/event but emits no turn/started or turn/completed', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)

    // A message submitted by a human in the browser: its messageId is not one we submitted.
    h.bridge.onInboxClaimed(agent as never, 'someone-elses-message', 7)
    h.bridge.onSessionEvent(agent.session as never, agent.session.append('turn/start', { turn: 7 }) as never)
    h.bridge.onSessionEvent(
      agent.session as never,
      agent.session.append('turn/end', { turn: 7, reason: { kind: 'completed' } }) as never,
    )
    h.bridge.onAgentStatus(agent as never, 'idle')

    expect(socket.notifications('turn/started')).toHaveLength(0)
    expect(socket.notifications('turn/completed')).toHaveLength(0)
    expect(socket.notifications('thread/event')).toHaveLength(2)
    expect(socket.notifications('thread/status')).toHaveLength(1)
    expect(threadId).toBeTruthy()
  })
})

describe('subscription broadcast', () => {
  it('sends notifications to every subscribed connection and stops after unsubscribe', async () => {
    const h = createHarness()
    const a = h.connect()
    const b = h.connect()
    const { threadId, agent } = await startThread(h, a.conn)
    h.ctx.persisted.set(threadId, { id: threadId, cwd: WORK })
    await h.call(b.conn, 'thread/resume', { threadId })

    h.bridge.onAgentStatus(agent as never, 'running')
    expect(a.socket.notifications('thread/status')).toHaveLength(1)
    expect(b.socket.notifications('thread/status')).toHaveLength(1)

    await h.call(b.conn, 'thread/unsubscribe', { threadId })
    h.bridge.onAgentStatus(agent as never, 'idle')
    expect(a.socket.notifications('thread/status')).toHaveLength(2)
    expect(b.socket.notifications('thread/status')).toHaveLength(1)
  })

  it('reports turn/completed with the last non-empty assistant message', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    const turnId = await claimTurn(h, conn, threadId, agent)

    h.bridge.onSessionEvent(agent.session as never, agent.session.append('assistant/message', {
      turn: turnId,
      message: { content: [{ type: 'text', text: 'first' }] },
    }) as never)
    // Empty messages do not overwrite previous output.
    h.bridge.onSessionEvent(agent.session as never, agent.session.append('assistant/message', {
      turn: turnId,
      message: { content: [] },
    }) as never)
    h.bridge.onSessionEvent(agent.session as never, agent.session.append('turn/end', {
      turn: turnId,
      reason: { kind: 'completed' },
    }) as never)

    const completed = socket.notifications('turn/completed').at(0)?.['params'] as Record<string, unknown>
    expect(completed).toMatchObject({ threadId, turnId, status: 'completed', finalMessage: 'first' })
  })

  it('maps an aborted turn to interrupted', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    const turnId = await claimTurn(h, conn, threadId, agent)
    h.bridge.onSessionEvent(agent.session as never, agent.session.append('turn/end', {
      turn: turnId,
      reason: { kind: 'aborted', reason: { kind: 'user' } },
    }) as never)
    const completed = socket.notifications('turn/completed').at(0)?.['params'] as Record<string, unknown>
    expect(completed['status']).toBe('interrupted')
  })
})

describe('steer and interrupt', () => {
  it('rejects steering an idle thread with no_active_turn', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId } = await startThread(h, conn)
    await expect(h.call(conn, 'turn/steer', { threadId, text: 'more' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'no_active_turn',
    )
  })

  it('rejects a steer whose expectedTurnId does not match', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent, 3)
    await expect(h.call(conn, 'turn/steer', { threadId, text: 'more', expectedTurnId: 9 })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'turn_mismatch',
    )
    await h.call(conn, 'turn/steer', { threadId, text: 'more', expectedTurnId: 3 })
    expect(agent.steers).toHaveLength(1)
  })

  it('cancels the agent on turn/interrupt and refuses when idle', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await expect(h.call(conn, 'turn/interrupt', { threadId })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'no_active_turn',
    )
    await claimTurn(h, conn, threadId, agent, 2)
    await h.call(conn, 'turn/interrupt', { threadId, turnId: 2 })
    expect(agent.cancelled).toBe(1)
  })
})

describe('thread/resume', () => {
  it('deduplicates concurrent resumes of the same cold thread', async () => {
    const h = createHarness()
    const a = h.connect()
    const b = h.connect()
    h.ctx.persisted.set('cold-1', { id: 'cold-1', cwd: WORK })
    let open: () => void = () => {}
    h.ctx.resumeGate = new Promise<void>((resolve) => { open = resolve })

    const first = h.call(a.conn, 'thread/resume', { threadId: 'cold-1' })
    const second = h.call(b.conn, 'thread/resume', { threadId: 'cold-1' })
    open()
    const [left, right] = await Promise.all([first, second]) as Record<string, unknown>[]

    expect(h.ctx.resumeCalls).toBe(1)
    expect(left?.['threadId']).toBe('cold-1')
    expect(right?.['cwd']).toBe(WORK)
  })

  it('binds a live agent instead of resuming it again', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    const { threadId } = await startThread(h, conn)
    const other = h.connect()
    h.ctx.persisted.set(threadId, { id: threadId, cwd: WORK })
    await h.call(other.conn, 'thread/resume', { threadId })
    expect(h.ctx.resumeCalls).toBe(0)
  })

  it('refuses a subagent session with not_resumable', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    h.ctx.persisted.set('child', { id: 'child', cwd: WORK, origin: 'subagent' })
    await expect(h.call(conn, 'thread/resume', { threadId: 'child' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'not_resumable',
    )
  })

  it('refuses an unknown session with thread_not_found', async () => {
    const h = createHarness()
    const { conn } = h.connect()
    await expect(h.call(conn, 'thread/resume', { threadId: 'ghost' })).rejects.toSatisfy(
      (error: unknown) => errorKind(error) === 'thread_not_found',
    )
  })
})

describe('reverse requests', () => {
  it('delegates to the next listener when the thread has no in-flight bridge turn', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { agent } = await startThread(h, conn)
    const outcome = await askApproval(h, agent)
    expect(outcome).toBe('unavailable')
    expect(socket.requests()).toHaveLength(0)
  })

  it('delivers to every subscriber, lets the first answer win, and tells the rest', async () => {
    const h = createHarness()
    const a = h.connect()
    const b = h.connect()
    const { threadId, agent } = await startThread(h, a.conn)
    h.ctx.persisted.set(threadId, { id: threadId, cwd: WORK })
    await h.call(b.conn, 'thread/resume', { threadId })
    await claimTurn(h, a.conn, threadId, agent)

    const decision = askApproval(h, agent)
    await tick()
    const sentToA = a.socket.requests('approval/request').at(0)
    const sentToB = b.socket.requests('approval/request').at(0)
    expect(sentToA?.['id']).toBe(sentToB?.['id'])
    expect((sentToA?.['params'] as Record<string, unknown>)['toolName']).toBe('write')

    b.socket.receive({ jsonrpc: '2.0', id: sentToB?.['id'] as string, result: { decision: 'allowed-once' } })
    expect(await decision).toBe('allowed-once')

    const resolved = a.socket.notifications('request/resolved').at(0)?.['params'] as Record<string, unknown>
    expect(resolved).toMatchObject({ requestId: sentToA?.['id'], resolvedBy: 'client' })
    expect(b.socket.notifications('request/resolved')).toHaveLength(0)
  })

  it('ignores a late answer from the loser', async () => {
    const h = createHarness()
    const a = h.connect()
    const b = h.connect()
    const { threadId, agent } = await startThread(h, a.conn)
    h.ctx.persisted.set(threadId, { id: threadId, cwd: WORK })
    await h.call(b.conn, 'thread/resume', { threadId })
    await claimTurn(h, a.conn, threadId, agent)

    const decision = askApproval(h, agent)
    await tick()
    const id = a.socket.requests('approval/request').at(0)?.['id'] as string
    a.socket.receive({ jsonrpc: '2.0', id, result: { decision: 'rejected' } })
    b.socket.receive({ jsonrpc: '2.0', id, result: { decision: 'allowed-once' } })
    expect(await decision).toBe('rejected')
  })

  it('treats a JSON-RPC error response as a rejection', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent)

    const decision = askApproval(h, agent)
    await tick()
    const id = socket.requests('approval/request').at(0)?.['id'] as string
    socket.receive({ jsonrpc: '2.0', id, error: { code: BRIDGE_ERROR_CODE, message: 'auto-declined' } })
    expect(await decision).toBe('rejected')
  })

  it('normalizes an out-of-vocabulary decision to rejected', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent)

    const decision = askApproval(h, agent)
    await tick()
    const id = socket.requests('approval/request').at(0)?.['id'] as string
    socket.receive({ jsonrpc: '2.0', id, result: { decision: 'accept-for-session' } })
    expect(await decision).toBe('rejected')
  })

  it('keeps the pending request when the client disconnects and resends it on resume', async () => {
    const h = createHarness()
    const first = h.connect()
    const { threadId, agent } = await startThread(h, first.conn)
    h.ctx.persisted.set(threadId, { id: threadId, cwd: WORK })
    await claimTurn(h, first.conn, threadId, agent)

    const decision = askApproval(h, agent)
    await tick()
    const id = first.socket.requests('approval/request').at(0)?.['id'] as string
    first.socket.close()
    await tick()

    // Disconnecting does not resolve the request: it remains pending.
    const read = await h.call(first.conn, 'thread/read', { threadId }) as Record<string, unknown>
    expect(read['pendingRequests']).toEqual([id])

    const second = h.connect()
    const resumed = await h.call(second.conn, 'thread/resume', { threadId }) as Record<string, unknown>
    expect(resumed['pendingRequests']).toEqual([id])
    await tick()
    const resent = second.socket.requests('approval/request').at(0)
    expect(resent?.['id']).toBe(id)

    second.socket.receive({ jsonrpc: '2.0', id, result: { decision: 'allowed-once' } })
    expect(await decision).toBe('allowed-once')
  })

  it('cancels the pending request when the tool signal aborts', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent)

    const controller = new AbortController()
    const decision = askApproval(h, agent, controller.signal)
    await tick()
    const id = socket.requests('approval/request').at(0)?.['id'] as string
    controller.abort()
    expect(await decision).toBe('cancelled')

    const resolved = socket.notifications('request/resolved').at(0)?.['params'] as Record<string, unknown>
    expect(resolved).toMatchObject({ requestId: id, resolvedBy: 'cancelled' })
    const read = await h.call(conn, 'thread/read', { threadId }) as Record<string, unknown>
    expect(read['pendingRequests']).toEqual([])
  })

  it('returns cancelled without dispatching when the signal is already aborted', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent)
    expect(await askApproval(h, agent, AbortSignal.abort())).toBe('cancelled')
    expect(socket.requests('approval/request')).toHaveLength(0)
  })

  it('passes a user-question answer through and rejects a malformed one', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent)

    const questions = [{ id: 'q1', question: 'pick one', options: [{ label: 'a' }, { label: 'b' }] }]
    const good = h.bridge.onUserQuestionRequest({ agent: agent as never, questions }, async () => ({}))
    await tick()
    const id = socket.requests('userQuestion/request').at(0)?.['id'] as string
    socket.receive({ jsonrpc: '2.0', id, result: { answers: [{ id: 'q1', selected: ['a'] }] } })
    expect(await good).toEqual({ answers: [{ id: 'q1', selected: ['a'] }] })

    const bad = h.bridge.onUserQuestionRequest({ agent: agent as never, questions }, async () => ({}))
    await tick()
    const badId = socket.requests('userQuestion/request').at(1)?.['id'] as string
    socket.receive({ jsonrpc: '2.0', id: badId, result: { nope: true } })
    await expect(bad).rejects.toThrow(/answers/)
  })

  it('turns a JSON-RPC error on a user question into a thrown rejection', async () => {
    const h = createHarness()
    const { conn, socket } = h.connect()
    const { threadId, agent } = await startThread(h, conn)
    await claimTurn(h, conn, threadId, agent)
    const asked = h.bridge.onUserQuestionRequest(
      { agent: agent as never, questions: [{ id: 'q1', question: 'x' }] },
      async () => ({}),
    )
    await tick()
    const id = socket.requests('userQuestion/request').at(0)?.['id'] as string
    socket.receive({ jsonrpc: '2.0', id, error: { code: BRIDGE_ERROR_CODE, message: 'auto-declined' } })
    await expect(asked).rejects.toThrow(/auto-declined/)
  })
})

describe('json-rpc framing', () => {
  it('answers initialize over the wire', async () => {
    const h = createHarness()
    const { socket } = h.connect()
    socket.receive({ jsonrpc: '2.0', id: 1, method: 'initialize', params: {} })
    await tick()
    const response = socket.responses().at(0)
    expect((response?.['result'] as Record<string, unknown>)['uiUrl']).toBe('http://127.0.0.1:12899/')
  })

  it('reports an unknown method with -32601', async () => {
    const h = createHarness()
    const { socket } = h.connect()
    socket.receive({ jsonrpc: '2.0', id: 2, method: 'nope/at-all', params: {} })
    await tick()
    expect((socket.responses().at(0)?.['error'] as Record<string, unknown>)['code']).toBe(-32601)
  })

  it('reports a business error with -32000 and a kind', async () => {
    const h = createHarness()
    const { socket } = h.connect()
    socket.receive({ jsonrpc: '2.0', id: 3, method: 'thread/read', params: { threadId: 'ghost' } })
    await tick()
    const error = socket.responses().at(0)?.['error'] as Record<string, unknown>
    expect(error['code']).toBe(BRIDGE_ERROR_CODE)
    expect((error['data'] as Record<string, unknown>)['kind']).toBe('thread_not_found')
  })

  it('reports malformed JSON with -32700', async () => {
    const h = createHarness()
    const { socket } = h.connect()
    socket.emit('message', '{ not json')
    expect((socket.sent.at(0)?.['error'] as Record<string, unknown>)['code']).toBe(-32700)
  })
})
