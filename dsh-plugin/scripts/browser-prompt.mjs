#!/usr/bin/env node
/**
 * Send a message to a session as a browser would, using the dsh gateway's `session/prompt` remote.
 * Verify bridge plugin behavior for a human-initiated browser turn without opening a browser.
 *
 *   node scripts/browser-prompt.mjs <threadId> "<text>" [--ui http://127.0.0.1:12899] [--token <t>]
 *
 * Wire flow: exchange `?token=` for a `dsh-auth-*` cookie, then POST /api/session/prompt,
 * with a Connection RPC envelope as the body (packages/client/connection/src/rpc-host.ts:216-247).
 * If no token is given, read it from the `dsh web:` line in daemon.log. Uses only Node built-in modules.
 */

import { readFileSync } from 'node:fs'
import { randomUUID } from 'node:crypto'
import process, { argv } from 'node:process'
import { fileURLToPath } from 'node:url'

function flag(name, fallback) {
  const index = argv.indexOf(name)
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : fallback
}

const threadId = argv[2]
const text = argv[3]
if (threadId === undefined || text === undefined) {
  console.error('usage: node scripts/browser-prompt.mjs <threadId> "<text>"')
  process.exitCode = 2
  throw new Error('missing arguments')
}

const ui = flag('--ui', 'http://127.0.0.1:12899')
const logPath = flag('--log', fileURLToPath(new URL('../.m0/daemon.log', import.meta.url)))
const mode = flag('--mode', 'queue') // queue | steer

function tokenFromLog() {
  const lines = readFileSync(logPath, 'utf8').split(/\r?\n/).filter(line => line.startsWith('dsh web:'))
  const last = lines.at(-1)
  if (last === undefined) throw new Error(`no "dsh web:" line in ${logPath}`)
  return new URL(last.slice('dsh web:'.length).trim()).searchParams.get('token')
}

const token = flag('--token', tokenFromLog())

// 1. Exchange the token for a cookie.
const handshake = await fetch(`${ui}/?token=${token}`, { redirect: 'manual' })
const setCookie = handshake.headers.get('set-cookie')
if (setCookie === null) throw new Error(`no Set-Cookie from ${ui} (status ${handshake.status})`)
const cookie = setCookie.split(';')[0]
console.log(`cookie ok (${cookie.split('=')[0]})`)

// 2. POST a Connection RPC envelope to /api/session/prompt.
const rpcId = randomUUID()
const response = await fetch(`${ui}/api/session/prompt`, {
  method: 'POST',
  headers: { 'content-type': 'application/json', cookie },
  body: JSON.stringify({
    type: 'client-request',
    rpcId,
    method: 'session/prompt',
    payload: {
      args: {
        request: {
          requestId: `browser-${randomUUID()}`,
          sessionId: threadId,
          mode,
          content: [{ type: 'text', text }],
        },
      },
    },
  }),
})

const body = await response.text()
console.log(`POST /api/session/prompt -> ${response.status} ${body}`)
// exit() can crash libuv while undici closes its handles; use exitCode to let the process exit naturally.
process.exitCode = response.ok && !body.includes('"ok":false') ? 0 : 1
