#!/usr/bin/env node
/**
 * Manual e2e client for dsh-bridge-plugin (a real dsh process; excluded from vitest).
 *
 *   node scripts/m1-client.mjs <scenario> [--url ws://127.0.0.1:12898] [--cwd <dir>]
 *
 * scenario:
 *   turns        Three turns: no tools / tool use / continued context
 *   approve      Trigger approval/request with an out-of-sandbox write; answer allowed-once after 5 seconds
 *   reject       Same as above, but answer rejected
 *   resend       Disconnect with approval pending, reconnect with thread/resume, then answer after verifying redelivery with the same id
 *   interrupt    Send turn/interrupt with approval pending; verify request/resolved cancelled
 *   question     Ask the model to use ask_user_question; respond with answers
 *   busy         Send another turn/start while a turn is running; verify thread_busy
 *   resume <id>  Cold resume: thread/resume an existing thread, then send another turn
 *   read <id>    Read one thread only
 *
 * Node 24 provides the global WebSocket class; ws is not needed.
 */

import { argv } from 'node:process'
import { fileURLToPath } from 'node:url'

function flag(name, fallback) {
  const index = argv.indexOf(name)
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : fallback
}

const URL_ = flag('--url', 'ws://127.0.0.1:12898')
const CWD = flag('--cwd', fileURLToPath(new URL('../.m0/work', import.meta.url)))
const OUTSIDE = flag('--outside', fileURLToPath(new URL('../.m0/outside/x.txt', import.meta.url)))
const scenario = argv[2] ?? 'turns'
const positional = argv[3]

const stamp = () => new Date().toISOString().slice(11, 23)
const say = (...parts) => { console.log(`[${stamp()}]`, ...parts) }

class Client {
  #socket
  #nextId = 0
  #pending = new Map()
  /** (frame) => void, server notification. */
  onNotify = () => {}
  /** (frame) => Promise<{result}|{error}>, reverse request. */
  onRequest = async () => ({ error: { code: -32000, message: 'no handler' } })
  events = []

  async connect() {
    this.#socket = new WebSocket(URL_)
    await new Promise((resolve, reject) => {
      this.#socket.addEventListener('open', resolve, { once: true })
      this.#socket.addEventListener('error', reject, { once: true })
    })
    this.#socket.addEventListener('message', (event) => { this.#dispatch(JSON.parse(String(event.data))) })
    return this
  }

  #dispatch(frame) {
    if (typeof frame.method === 'string' && frame.id !== undefined) {
      say(`<< SERVER REQUEST ${frame.method} id=${frame.id}`, JSON.stringify(frame.params))
      void Promise.resolve(this.onRequest(frame)).then((answer) => {
        if (answer === undefined) return // Deliberately leave it unanswered
        this.#socket.send(JSON.stringify({ jsonrpc: '2.0', id: frame.id, ...answer }))
        say(`>> answered id=${frame.id}`, JSON.stringify(answer))
      })
      return
    }
    if (typeof frame.method === 'string') {
      this.events.push(frame)
      this.onNotify(frame)
      return
    }
    const pending = this.#pending.get(frame.id)
    if (pending === undefined) return
    this.#pending.delete(frame.id)
    if (frame.error !== undefined) pending.reject(new Error(JSON.stringify(frame.error)))
    else pending.resolve(frame.result)
  }

  call(method, params) {
    this.#nextId += 1
    const id = this.#nextId
    this.#socket.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise((resolve, reject) => { this.#pending.set(id, { resolve, reject }) })
  }

  close() { this.#socket.close() }
}

/** Print a single-line summary of a notification. */
function summarize(frame) {
  const p = frame.params ?? {}
  if (frame.method === 'thread/event') {
    const detail = p.type === 'tool/call'
      ? p.data?.name
      : p.type === 'assistant/message'
        ? (p.data?.message?.content ?? []).filter(b => b.type === 'text').map(b => b.text).join('').slice(0, 70).replace(/\s+/g, ' ')
        : ''
    return `thread/event ${p.type}${detail ? ` (${detail})` : ''}`
  }
  if (frame.method === 'turn/completed') {
    return `turn/completed status=${p.status} reason=${p.reason} final=${JSON.stringify((p.finalMessage ?? '').slice(0, 70))}`
  }
  if (frame.method === 'request/resolved') return `request/resolved ${p.requestId} by=${p.resolvedBy}`
  return `${frame.method} ${JSON.stringify(p)}`
}

/** Subscribe to notifications and resolve when turn/completed arrives. */
function watch(client, log = true) {
  const seen = []
  let settle
  const done = new Promise((resolve) => { settle = resolve })
  client.onNotify = (frame) => {
    seen.push(summarize(frame))
    if (log) say('  ->', summarize(frame))
    if (frame.method === 'turn/completed') settle(frame.params)
  }
  return { seen, done }
}

const sleep = (ms) => new Promise(resolve => { setTimeout(resolve, ms) })

async function newThread(client, options = {}) {
  const thread = await client.call('thread/start', {
    cwd: CWD,
    sandbox: 'workspace-write',
    approval: 'ask',
    title: `dsh-bridge ${scenario}`,
    ...options,
  })
  say('thread/start ->', JSON.stringify(thread))
  return thread
}

const ESCALATION_PROMPT = `Use the write tool to write one line containing hi at the absolute path ${OUTSIDE}. `
  + 'This path is outside the current working directory. If the sandbox rejects the first attempt, retry once with sandbox_permissions. '
  + 'When finished, reply with just "done".'

async function scenarioApproval(decision) {
  const client = await (new Client()).connect()
  say('initialize ->', JSON.stringify(await client.call('initialize', { clientInfo: { name: 'm1-client', version: '1' } })))
  const thread = await newThread(client)
  const { seen, done } = watch(client)
  client.onRequest = async (frame) => {
    if (frame.method !== 'approval/request') return { error: { code: -32000, message: 'unexpected' } }
    say(`holding the approval for 5s, then answering ${decision}`)
    await sleep(5000)
    return { result: { decision } }
  }
  say('turn/start ->', JSON.stringify(await client.call('turn/start', { threadId: thread.threadId, text: ESCALATION_PROMPT })))
  const completed = await done
  say('FINAL:', completed.finalMessage ?? '(none)')
  say('SEQUENCE:', seen.join(' | '))
  client.close()
}

async function scenarioResend() {
  const first = await (new Client()).connect()
  await first.call('initialize', {})
  const thread = await newThread(first)
  const seen = []
  let requestId
  let dropped
  const droppedOnce = new Promise((resolve) => { dropped = resolve })
  first.onNotify = (frame) => { seen.push(summarize(frame)); say('  ->', summarize(frame)) }
  first.onRequest = async (frame) => {
    requestId = frame.id
    say(`got ${frame.method} id=${requestId}; dropping the connection without answering`)
    first.close()
    dropped(requestId)
    return undefined
  }
  say('turn/start ->', JSON.stringify(await first.call('turn/start', { threadId: thread.threadId, text: ESCALATION_PROMPT })))
  await droppedOnce
  await sleep(500)

  const second = await (new Client()).connect()
  await second.call('initialize', {})
  const { seen: seen2, done } = watch(second)
  second.onRequest = async (frame) => {
    say(`resent id=${frame.id} (original ${requestId}) same=${String(frame.id === requestId)}`)
    return { result: { decision: 'allowed-once' } }
  }
  const resumed = await second.call('thread/resume', { threadId: thread.threadId })
  say('thread/resume ->', JSON.stringify(resumed))
  const completed = await done
  say('FINAL:', completed.finalMessage ?? '(none)')
  say('SEQUENCE:', [...seen, '<<disconnect>>', ...seen2].join(' | '))
  second.close()
}

async function scenarioInterrupt() {
  const client = await (new Client()).connect()
  await client.call('initialize', {})
  const thread = await newThread(client)
  const { seen, done } = watch(client)
  client.onRequest = async (frame) => {
    say(`got ${frame.method} id=${frame.id}; interrupting the turn instead of answering`)
    await client.call('turn/interrupt', { threadId: thread.threadId })
    return undefined
  }
  say('turn/start ->', JSON.stringify(await client.call('turn/start', { threadId: thread.threadId, text: ESCALATION_PROMPT })))
  const completed = await done
  say('TURN:', JSON.stringify(completed))
  say('SEQUENCE:', seen.join(' | '))
  client.close()
}

async function scenarioQuestion() {
  const client = await (new Client()).connect()
  await client.call('initialize', {})
  const thread = await newThread(client, { sandbox: 'read-only' })
  const { seen, done } = watch(client)
  client.onRequest = async (frame) => {
    if (frame.method !== 'userQuestion/request') return { error: { code: -32000, message: 'unexpected' } }
    const question = frame.params.questions[0]
    const pick = question.options?.[1]?.label ?? question.options?.[0]?.label ?? 'B'
    say(`answering ${question.id} with ${JSON.stringify(pick)}`)
    return { result: { answers: [{ id: question.id, selected: [pick] }] } }
  }
  const prompt = 'First use the ask_user_question tool to ask me: "Which M1 acceptance check should run first?",'
    + ' offering two options, A: reverse requests, B: cold resume. After receiving my answer, reply only with "You selected <option>".'
  say('turn/start ->', JSON.stringify(await client.call('turn/start', { threadId: thread.threadId, text: prompt })))
  const completed = await done
  say('FINAL:', completed.finalMessage ?? '(none)')
  say('SEQUENCE:', seen.join(' | '))
  client.close()
}

async function scenarioBusy() {
  const client = await (new Client()).connect()
  await client.call('initialize', {})
  const thread = await newThread(client)
  const { seen, done } = watch(client, false)
  await client.call('turn/start', { threadId: thread.threadId, text: 'Count to twenty, with each number on its own line.' })
  try {
    await client.call('turn/start', { threadId: thread.threadId, text: 'This message should be rejected.' })
    say('UNEXPECTED: the second turn/start was accepted')
  } catch (error) {
    say('second turn/start rejected ->', error.message)
  }
  const completed = await done
  say('first turn:', completed.status)
  say('SEQUENCE:', seen.join(' | '))
  client.close()
}

async function scenarioBrowser() {
  const { execFile } = await import('node:child_process')
  const client = await (new Client()).connect()
  await client.call('initialize', {})
  const thread = await newThread(client)
  // Run a short turn first so the thread enters the plugin's thread table and is persisted to disk.
  {
    const { done } = watch(client, false)
    await client.call('turn/start', { threadId: thread.threadId, text: 'Reply only with ok.' })
    say('warm-up turn:', (await done).status)
  }

  const seen = []
  let settle
  const idleAgain = new Promise((resolve) => { settle = resolve })
  let sawRunning = false
  client.onNotify = (frame) => {
    seen.push(summarize(frame))
    say('  ->', summarize(frame))
    if (frame.method === 'thread/status' && frame.params.status === 'running') sawRunning = true
    if (frame.method === 'thread/status' && frame.params.status === 'idle' && sawRunning) settle()
  }

  say('sending a prompt through the dsh gateway as if from the browser')
  await new Promise((resolve, reject) => {
    execFile(
      process.execPath,
      [fileURLToPath(new URL('browser-prompt.mjs', import.meta.url)), thread.threadId, 'Reply with just this sentence: I was sent from the browser.'],
      (error, stdout, stderr) => {
        say('browser-prompt stdout:', stdout.trim().split('\n').at(-1) ?? '')
        if (error !== null) { console.error(stderr); reject(error); return }
        resolve()
      },
    )
  })

  // Our turn/start must be rejected while the browser's turn is running.
  await sleep(1500)
  try {
    await client.call('turn/start', { threadId: thread.threadId, text: 'This message should be rejected.' })
    say('UNEXPECTED: turn/start was accepted during the browser turn')
  } catch (error) {
    say('turn/start during the browser turn ->', error.message)
  }

  await idleAgain
  await sleep(500)
  say('SEQUENCE:', seen.join(' | '))
  say('turn/started count:', seen.filter(line => line.startsWith('turn/started')).length)
  say('turn/completed count:', seen.filter(line => line.startsWith('turn/completed')).length)
  say('thread/event count:', seen.filter(line => line.startsWith('thread/event')).length)
  client.close()
}

async function scenarioTurns() {
  const client = await (new Client()).connect()
  say('initialize ->', JSON.stringify(await client.call('initialize', {})))
  const thread = await newThread(client)
  for (const [label, text] of [
    ['no tools', 'Reply with just one sentence: state the absolute path of the current working directory, without calling any tools.'],
    ['tool use', 'Create hello.txt in the current directory containing hi, then reply that it is done.'],
    ['context', 'What is the name of the file you just created? Reply only with the filename, without calling any more tools.'],
  ]) {
    const { seen, done } = watch(client)
    say(`===== ${label} =====`)
    await client.call('turn/start', { threadId: thread.threadId, text })
    const completed = await done
    say('FINAL:', completed.finalMessage ?? '(none)')
    say('SEQUENCE:', seen.join(' | '))
  }
  say('threadId for a later cold resume:', thread.threadId)
  client.close()
}

async function scenarioResume(threadId) {
  const client = await (new Client()).connect()
  await client.call('initialize', {})
  const resumed = await client.call('thread/resume', { threadId })
  say('thread/resume ->', JSON.stringify(resumed))
  const { seen, done } = watch(client)
  await client.call('turn/start', { threadId, text: 'We created a file earlier in this session. What was its name? Reply only with the filename, without calling any tools.' })
  const completed = await done
  say('FINAL:', completed.finalMessage ?? '(none)')
  say('SEQUENCE:', seen.join(' | '))
  client.close()
}

async function scenarioRead(threadId) {
  const client = await (new Client()).connect()
  await client.call('initialize', {})
  console.log(JSON.stringify(await client.call('thread/read', { threadId, includeTurns: true }), null, 2))
  client.close()
}

const table = {
  turns: () => scenarioTurns(),
  approve: () => scenarioApproval('allowed-once'),
  reject: () => scenarioApproval('rejected'),
  resend: () => scenarioResend(),
  interrupt: () => scenarioInterrupt(),
  question: () => scenarioQuestion(),
  busy: () => scenarioBusy(),
  browser: () => scenarioBrowser(),
  resume: () => scenarioResume(positional),
  read: () => scenarioRead(positional),
}

const run = table[scenario]
if (run === undefined) {
  console.error(`unknown scenario ${JSON.stringify(scenario)}; one of ${Object.keys(table).join(', ')}`)
  process.exit(2)
}
await run()
process.exit(0)
