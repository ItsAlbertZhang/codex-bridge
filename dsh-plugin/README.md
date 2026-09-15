# dsh-bridge-plugin

A Cordis plugin for dsh (DeepSeek Harness). It opens a WebSocket port bound to
loopback inside the dsh process and exposes the dsh agent kernel over JSON-RPC
2.0 as threads, turns, and server-initiated requests (approvals and user
questions). It runs in the same process as dsh's web UI, so the browser shows
the very sessions a client drives through this port.

Its client is the `dsh` backend of [agent-bridge](../README.md); the protocol
below is what that backend speaks. Any JSON-RPC client can use it.

## Install

Build and pack, starting in this directory:

```sh
pnpm install
pnpm run build
pnpm pack
```

`pnpm-workspace.yaml` holds the build-script decision pnpm needs for a
transitive dependency; run `pnpm install` without `--ignore-workspace`. The
result is `dsh-bridge-plugin-<version>.tgz`.

Create a dsh profile from the shipped web template (base plus web UI), then
install the tarball into it:

```sh
dsh --profile <name> --from-default-profile web --dump-config
dsh plugin --profile <name> add <path to tarball>
```

The package is a dsh bundle: `package.json` points at `cordis.patch.yml`, and
installing it appends the plugin row to the profile's bundle list. Verify with
`dsh --profile <name> --dump-config`, which should show a `dsh-bridge` row.

To install a new version, remove the old one first:

```sh
dsh plugin --profile <name> remove dsh-bridge-plugin
dsh plugin --profile <name> add <path to new tarball>
```

The profile lockfile records the previous tarball by absolute path; once that
file is gone, `add` fails while resolving the stale entry. The bundle set is
fixed at startup, so restart dsh after reinstalling.

Start dsh with that profile; `--port` is the browser UI port, not the plugin's:

```sh
dsh --profile <name> --port <ui port>
```

Configuration lives in the plugin row of `cordis.patch.yml`:

| Key | Default | Meaning |
|---|---|---|
| `host` | `127.0.0.1` | listen address |
| `port` | `12898` | WebSocket and readiness port; the shipped patch reads `DSH_BRIDGE_PORT` from the dsh process environment when set |
| `agentPreset` | `standard` | agent preset mounted on every thread |

agent-bridge's `dsh` backend expects profile `bridge` and UI port `12899`
unless told otherwise through its own environment variables.

## Protocol

JSON-RPC 2.0 over WebSocket at `ws://<host>:<port>/`. The same port answers
`GET /readyz` and `GET /healthz` with `200 ok` once the plugin is listening,
which happens only after every injected dsh service is available. There is no
authentication.

One connection can work on several threads and several connections can
subscribe to one thread. A thread is subscribed on the connection that started
it, resumed it, or sent it a turn. Client request ids may be strings or
numbers; ids of server-initiated requests are strings. `threadId` is a string
(`bridge-<uuid>` for threads created here), `turnId` is dsh's integer turn
number within the session, and `requestId` is `req-<n>` with a process-wide
counter.

### Methods

| Method | params | result |
|---|---|---|
| `initialize` | `{clientInfo?}` | `{serverVersion, dshVersion, uiUrl, profile, agentPreset}`. Optional. `uiUrl` is the token-free browser URL derived from dsh's `--port`; `profile` is `DSH_PROFILE` from the environment or `null` |
| `thread/start` | `{cwd, provider?, model?, reasoningEffort?, sandbox?, approval?, developerInstructions?, title?}` | `{threadId, title, cwd, provider?, model?, reasoningEffort?}` |
| `thread/resume` | `{threadId}` | `{threadId, status, cwd, title, provider?, model?, reasoningEffort?, pendingRequests: [requestId, ...]}` |
| `thread/read` | `{threadId, includeTurns?}` | `{threadId, status, cwd, title, provider?, model?, reasoningEffort?, pendingRequests, turns?, events?}` |
| `turn/start` | `{threadId, text}` | `{turnId}` |
| `turn/steer` | `{threadId, text, expectedTurnId?}` | `{turnId}` |
| `turn/interrupt` | `{threadId, turnId?}` | `{}` |
| `thread/unsubscribe` | `{threadId}` | `{}` |

`thread/start` creates a session and subscribes the connection. `cwd` must be
an absolute path and is created if missing. `sandbox` is `read-only`,
`workspace-write`, or `danger-full-access`; `approval` is `ask` or `never`;
omitted policies use dsh's process defaults. `provider`, `model`, and
`reasoningEffort` override dsh's current default selection field by field; the
result reports the created agent's actual options. `developerInstructions` is
added as a system-prompt section after the preset's persona. `title` defaults
to `dsh-` plus the first eight characters of the UUID part of the thread id.

`thread/resume` subscribes the connection to an existing thread. A thread this
plugin already knows is bound directly; a session that is live elsewhere in
the process (for example opened in the browser) is adopted without resuming;
a cold session is restored from persistence with the configured preset
remounted and dsh's current default model selection. Subagent sessions are
refused with `not_resumable`. Right after the response, every pending request
of the thread is re-delivered to this connection with its original id.

`thread/read` does not activate a cold session. Its `status` is `idle` or
`running`; `title` is `null` for a thread this plugin has not registered, and
the routing fields of a cold session come from its last persisted request
header, omitted when unknown. With `includeTurns`, `turns` is
`[{turnId, status, reason?, finalMessage?}]` folded from the session log
(`status` is `running` until the turn ends) and `events` is a compact event
list for debugging.

`turn/start` fails with `thread_busy` when the plugin already has a turn in
flight on the thread or the agent is running (including a turn started from
the browser); it does not queue. It returns once dsh has claimed the message
and assigned a turn number (30 seconds at most, then `internal`).
`turn/steer` requires a running turn (`no_active_turn`) and, when
`expectedTurnId` is given, that it matches the current turn (`turn_mismatch`).
`turn/interrupt` cancels the running turn; the outcome arrives as
`turn/completed` with `status: "interrupted"`. `expectedTurnId` and `turnId`
must be integers when present (`invalid_params`).

### Notifications

All carry `threadId` and go only to connections subscribed to the thread.

| Method | params | Source |
|---|---|---|
| `thread/status` | `{threadId, status}` | dsh `agent/status`; `status` is `idle` or `running` |
| `turn/started` | `{threadId, turnId}` | dsh claimed a message this plugin submitted |
| `turn/completed` | `{threadId, turnId, status, reason, error?, finalMessage?}` | `turn/end` of a turn this plugin started |
| `thread/event` | `{threadId, seq, type, data}` | session events passed through unchanged |
| `request/resolved` | `{threadId, requestId, resolvedBy}` | a server request was settled elsewhere |

`turn/completed.status` maps dsh's `turn/end` reason: `completed` to
`completed`; `aborted` and `interrupted` to `interrupted`; anything else to
`failed`, with `error` holding the reason's error (JSON) or its kind. `reason`
is the dsh reason kind unchanged. `finalMessage` is the text of the last
nonempty `assistant/message` of the turn and is omitted when there was none.

`thread/event` forwards these session event types: `user/message`,
`assistant/message`, `tool/call`, `tool/result`, `step/start`, `step/end`,
`turn/start`, `turn/end`, `session/title`. dsh's `agent/error` is also sent as
a `thread/event` with `type: "agent/error"`, `seq: null`, and
`data: {turn, error}`. Streaming deltas are not forwarded: a step produces
events only when a message or tool call is recorded.

Turns started by a human in the browser on a thread this plugin knows produce
`thread/status` and `thread/event` but no `turn/started` or `turn/completed`.

`request/resolved.resolvedBy` is `client` (another connection answered),
`browser`, or `cancelled` (the turn was interrupted or the request's signal
aborted).

### Server-initiated requests

| Method | params | Expected result |
|---|---|---|
| `approval/request` | `{threadId, requestId, toolName, callId?, reason?}` | `{decision}` with `allowed-once` or `rejected`; any other value is logged and treated as `rejected` |
| `userQuestion/request` | `{threadId, requestId, questions: [{id, question, ...}]}` | `{answers: [{id, selected: [...], custom?}]}`; any other shape fails the tool call |

A JSON-RPC error response to either request is an explicit rejection: an
approval becomes `rejected`, and a question tool call fails with the error's
message. Both settle the request. Approvals are one-time; there is no
session-wide grant.

### Errors

Business errors use code `-32000` with `data.kind` set to one of
`thread_not_found`, `thread_busy`, `no_active_turn`, `turn_mismatch`,
`invalid_cwd`, `not_resumable`, `invalid_params`, `internal`, and a readable
`message`. Framing errors use the standard codes: `-32700` for unparseable
JSON, `-32600` for a non-object frame, `-32601` for an unknown method.

## Request routing

`approval/request` and `user-questions/request` are dsh waterfall events. The
plugin registers its listeners in front of dsh's own web gateway and decides
ownership per request:

- The request belongs to the plugin only when its agent is a thread the plugin
  registered and that thread currently has a turn in flight that a bridge
  client started. Then the request is stored and sent to every subscribed
  connection under one `requestId`.
- Otherwise the listener passes the request on unchanged, and the browser
  opens its usual dialog. Sessions created in the browser, and turns a human
  starts from the browser on a bridge thread, are therefore untouched.

Pending requests live in plugin memory. A disconnecting client does not settle
them; `thread/resume` lists them in `pendingRequests` and re-delivers them with
the same id. The first answer from any connection wins; the others get
`request/resolved` and late answers are ignored. When the request's abort
signal fires (turn interrupted from either side, or a tool timeout), the
request is dropped and subscribers get `resolvedBy: "cancelled"`. The plugin
never delivers a request to the browser and a bridge client at the same time.

## Limitations

- Threads only get tools through the agent preset named in `agentPreset`; the
  preset must exist and be usable, or `thread/start` fails with `internal`.
- The session title is pinned with dsh's session-title service at creation;
  a cold thread that the plugin has not registered reads back `title: null`.
- Cold `thread/read` routing fields (`provider`, `model`, `reasoningEffort`)
  come from the last persisted request header and may be missing.
- Registered threads are kept for the lifetime of the process; the plugin
  never disposes agents.
- No authentication; the listener binds to loopback only by default.

## Scripts

Development tools in `scripts/`, run with Node against a real dsh process; none
of them is part of the unit tests.

| Script | Purpose |
|---|---|
| `reinstall.mjs` | bump the patch version (unless `--no-bump`), build, pack, remove the old plugin from the profile, and add the new tarball (`--profile <name>`, `--dsh-bin <entry point>`) |
| `m0-client.mjs` | minimal JSON-RPC client: start a thread, run a few turns, or `--read <threadId>` |
| `m1-client.mjs` | scenario client for approvals, rejection, redelivery, interrupt, questions, busy rejection, cold resume, and reads |
| `browser-prompt.mjs` | send a message to a session the way the browser does, through dsh's web gateway, to test browser-originated turns |

## Development

```sh
pnpm install
pnpm test
pnpm typecheck
```

Tests run the real `Bridge` and JSON-RPC connection against fake dsh services
and a fake socket. The dsh packages are peer dependencies pinned to the dsh
version the plugin targets; the running dsh installation supplies them, so
they are never bundled.
