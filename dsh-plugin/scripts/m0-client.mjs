#!/usr/bin/env node
/**
 * Manual debugging client for dsh-bridge-plugin.
 *
 * Usage:
 *   node scripts/m0-client.mjs                        # Run the three M0 turns
 *   node scripts/m0-client.mjs --url ws://127.0.0.1:12898
 *   node scripts/m0-client.mjs --cwd <directory>
 *   node scripts/m0-client.mjs --read <threadId>      # Read one thread only (including its event sequence)
 *
 * Node requirement: Node 24 provides the global WebSocket class; ws is not needed.
 */

import { argv } from 'node:process'
import { fileURLToPath } from 'node:url'

function arg(name, fallback) {
  const index = argv.indexOf(name)
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : fallback
}

const url = arg('--url', 'ws://127.0.0.1:12898')
const cwd = arg('--cwd', fileURLToPath(new URL('../.m0/work', import.meta.url)))
const readOnly = arg('--read', undefined)

class Client {
  #socket
  #nextId = 0
  #pending = new Map()
  /** Record every notification so the event sequence can be summarized. */
  notifications = []

  async connect() {
    this.#socket = new WebSocket(url)
    await new Promise((resolve, reject) => {
      this.#socket.addEventListener('open', resolve, { once: true })
      this.#socket.addEventListener('error', reject, { once: true })
    })
    this.#socket.addEventListener('message', (event) => { this.#onMessage(String(event.data)) })
  }

  #onMessage(raw) {
    const frame = JSON.parse(raw)
    if (typeof frame.method === 'string') {
      if (frame.id === undefined) {
        this.notifications.push(frame)
        this.#onNotification?.(frame)
        return
      }
      // Reverse request (not produced in M0; retained for manual M1 debugging).
      console.log('<< server request', JSON.stringify(frame))
      return
    }
    const pending = this.#pending.get(frame.id)
    if (pending === undefined) return
    this.#pending.delete(frame.id)
    if (frame.error !== undefined) pending.reject(new Error(JSON.stringify(frame.error)))
    else pending.resolve(frame.result)
  }

  #onNotification
  onNotification(handler) { this.#onNotification = handler }

  call(method, params) {
    this.#nextId += 1
    const id = this.#nextId
    this.#socket.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise((resolve, reject) => { this.#pending.set(id, { resolve, reject }) })
  }

  close() { this.#socket.close() }
}

/** Run one turn, print every notification, and return the turn/completed params. */
async function runTurn(client, threadId, text, label) {
  console.log(`\n===== ${label} =====`)
  console.log(`prompt: ${text}`)
  const seen = []
  const completed = new Promise((resolve) => {
    client.onNotification((frame) => {
      const params = frame.params ?? {}
      if (frame.method === 'thread/event') {
        const detail = params.type === 'tool/call'
          ? `${params.data?.name ?? ''}`
          : params.type === 'assistant/message'
            ? `${(params.data?.message?.content ?? []).filter(b => b.type === 'text').map(b => b.text).join('').slice(0, 80).replace(/\s+/g, ' ')}`
            : ''
        seen.push(`thread/event ${params.type}${detail === '' ? '' : ` (${detail})`}`)
        console.log(`  -> thread/event seq=${params.seq} type=${params.type}${detail === '' ? '' : ` ${JSON.stringify(detail)}`}`)
        return
      }
      seen.push(frame.method)
      console.log(`  -> ${frame.method} ${JSON.stringify(params)}`)
      if (frame.method === 'turn/completed') resolve(params)
    })
  })
  const started = await client.call('turn/start', { threadId, text })
  console.log(`turn/start -> ${JSON.stringify(started)}`)
  const result = await completed
  console.log(`\n--- finalMessage ---\n${result.finalMessage ?? '(none)'}\n--- end ---`)
  console.log(`event sequence: ${seen.join(' | ')}`)
  return result
}

const client = new Client()
await client.connect()

const info = await client.call('initialize', { clientInfo: { name: 'm0-client', version: '0.1.0' } })
console.log('initialize ->', JSON.stringify(info))

if (readOnly !== undefined) {
  const read = await client.call('thread/read', { threadId: readOnly, includeTurns: true })
  console.log(JSON.stringify(read, null, 2))
  client.close()
  process.exit(0)
}

const thread = await client.call('thread/start', {
  cwd,
  sandbox: 'workspace-write',
  approval: 'ask',
  title: 'dsh-bridge M0',
})
console.log('thread/start ->', JSON.stringify(thread))

await runTurn(client, thread.threadId, 'Reply with just one sentence: state the absolute path of the current working directory, without calling any tools.', 'turn 1 (no tools)')
await runTurn(client, thread.threadId, 'Create hello.txt in the current directory containing hi, then reply that it is done.', 'turn 2 (tool use)')
await runTurn(client, thread.threadId, 'What is the name of the file you just created? Reply only with the filename, without calling any more tools.', 'turn 3 (context retained)')

const read = await client.call('thread/read', { threadId: thread.threadId, includeTurns: true })
console.log('\n===== thread/read =====')
console.log(JSON.stringify(read, null, 2))

client.close()
