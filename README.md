# agent-bridge

`agent-bridge` is a Rust command-line tool that lets an orchestrating agent
(designed with Claude Code as the caller) hand a task to another agent running
on the same machine and follow it to the end. It has two backends:

- `codex`: the OpenAI Codex app-server, spoken to over JSON-RPC on a WebSocket.
- `dsh`: DeepSeek Harness, spoken to through the plugin shipped in
  [`dsh-plugin/`](dsh-plugin), which exposes the same thread/turn/request
  protocol shape on its own WebSocket port.

The commands, exit codes, and stdout events are identical for both backends.
The differences are the vocabulary of some option values and how a human looks
at the running session (a terminal pane for Codex, a browser for dsh).

stdout carries one JSON object per line; diagnostics go to stderr. The server
owns thread history, turns, and pending requests; the CLI moves messages,
appends logs, and decides when to return to its caller.

## Install

```sh
cargo install --path . --root ~/.local
```

The binary is `~/.local/bin/agent-bridge` (`agent-bridge.exe` on Windows); put
that directory on `PATH`. The Codex backend needs `codex` on `PATH` (or
`AGENT_BRIDGE_CODEX_BIN`). The dsh backend needs two more things:

1. The plugin installed into a dsh profile: see
   [`dsh-plugin/README.md`](dsh-plugin/README.md).
2. A way to launch dsh: `dsh` on `PATH`, or `AGENT_BRIDGE_DSH_BIN` pointing at
   dsh's JavaScript entry point (run with Node).

## Directories

| Content | Default location |
|---|---|
| Binary | `~/.local/bin/agent-bridge` |
| Configuration | `~/.local/share/agent-bridge/config.toml` |
| Daemon pid file, daemon log, per-thread logs | `~/.local/state/agent-bridge/<backend>/` |

`~` is `HOME` on Unix and `USERPROFILE` on Windows. The state directory of a
backend contains `daemon.pid`, `daemon.log`, and `logs/<threadId>.jsonl`; it is
created on demand.

Overrides, checked in this order on every platform:

| Path | Resolution |
|---|---|
| Configuration file | `AGENT_BRIDGE_CONFIG` (a file path), else `$XDG_DATA_HOME/agent-bridge/config.toml`, else `~/.local/share/agent-bridge/config.toml`, else `<OS temp dir>/agent-bridge/config.toml` |
| State root | `AGENT_BRIDGE_STATE_DIR`, else `$XDG_STATE_HOME/agent-bridge`, else `~/.local/state/agent-bridge`, else `<OS temp dir>/agent-bridge` |

Empty variables count as unset. The backend name (`codex` or `dsh`) is always
appended to the state root.

## Configuration

The configuration file is optional. It supplies defaults for threads created
by `run`; every table and key is optional:

```toml
[codex]
model = "<model id>"
effort = "<effort>"

[dsh]
provider = "<provider id>"
model = "<model id>"
effort = "<effort>"
```

[config.example.toml](config.example.toml) is a copy of this with comments.
Precedence for `model` and `effort` is command line, then configuration, then
the backend's own default (the field is simply omitted from the request). The
dsh `provider` exists only in configuration; there is no `--provider` option,
and `provider` under `[codex]` is rejected as an unknown key.

A missing file means empty configuration. Invalid TOML, wrong value types, or
unknown keys produce an `error` event naming the file and exit 4. The file is
loaded and validated before any command runs; `--help` and `--version` do not
read it.

Defaults are only applied when `run` creates a new thread. `run --thread`
sends none of them; the server keeps the resumed thread's settings.

## Commands

```text
agent-bridge <codex|dsh> [backend options] <verb> [verb options]
agent-bridge <codex|dsh> daemon <status|start|stop|restart>
```

`--help` works at every layer (`agent-bridge dsh run --help`). Daemon actions
are positional values, so `daemon restart --help` prints the daemon help page.
`agent-bridge --version` prints the crate version.

Backend options, accepted before or after the verb:

| Option | Meaning |
|---|---|
| `--url <ws url>` | endpoint; overrides `AGENT_BRIDGE_<BACKEND>_URL` and the default |
| `--log <file>` | append events to this file instead of the default thread log; conflicts with `--no-log` |
| `--no-log` | no thread log; conflicts with `--log` |
| `--no-pane` | Codex only: do not open or reuse a Herdr pane |

Every verb except `daemon` first makes sure the backend daemon is running
(see [Daemons](#daemons)).

### run

Start a new thread and a first turn, or add a turn to an existing thread:

```sh
agent-bridge codex run --cwd /work/repo --prompt "Summarise the build failure" \
  --sandbox read-only --approval never
agent-bridge codex run --thread <threadId> --prompt "Now fix the test"
```

| Option | Meaning |
|---|---|
| `--cwd <dir>` | working directory of the new thread; required unless `--thread` is given. dsh requires an absolute path and creates it if missing |
| `--thread <id>` | add a turn to this thread instead of creating one; conflicts with every thread-creation option |
| `--prompt <text>` / `--prompt-file <path>` | the turn's input; exactly one is required |
| `--sandbox <mode>` | `read-only`, `workspace-write`, `danger-full-access` |
| `--approval <policy>` | backend-specific vocabulary, see [Backend differences](#backend-differences) |
| `--model <id>`, `--effort <level>` | override the configuration defaults |
| `--developer-instructions-file <path>` | file whose text is sent as developer instructions |
| `--title <text>` | dsh only: the session title shown in the browser |
| `--no-wait` | print `started` and exit 0; the turn keeps running on the server |
| wait options | see [Wait options](#wait-options) |

`run` reads the thread before starting a turn and refuses with exit 4 if a
turn is already active. On success it prints `started` and, unless
`--no-wait`, waits like `wait`.

Thread-creation options (`--cwd`, `--sandbox`, `--approval`, `--model`,
`--effort`, `--developer-instructions-file`, dsh's `--title`) conflict with
`--thread`. For Codex, `--model` goes on the thread and `--effort` on the turn;
for dsh both go on the thread.

### wait

```sh
agent-bridge codex wait --thread <threadId> --stall-secs 300 --timeout-secs 1800
```

Resumes the thread and waits for the next outcome. An already idle thread
reports its last turn immediately (exit 0 or 1 by its status).

### reply

```sh
agent-bridge codex reply --thread <threadId> --decision accept
agent-bridge codex reply --thread <threadId> --request-id 56 \
  --result-json '{"answers":{"q1":{"answers":["yes"]}}}'
agent-bridge dsh reply --thread <threadId> --request-id req-3 \
  --result-json '{"answers":[{"id":"q1","selected":["yes"]}]}'
```

Answers a pending server request, then keeps waiting like `wait`. Exactly one
of `--decision` (backend vocabulary, mapped to the shape the request's method
expects) and `--result-json` (sent verbatim as the JSON-RPC result) is
required.

Without `--request-id`, `reply` resumes the thread, collects the requests the
server re-delivers during the next 10 seconds, and answers if exactly one
distinct id was seen. Zero ids, several ids, or an id that was resolved
meanwhile produce an `error` and exit 4 without answering; several ids ask for
`--request-id`. Wait options only apply after that window.

With `--request-id`, an already idle thread reports its last turn instead;
otherwise the CLI answers as soon as that id is re-delivered, waiting at most
10 seconds. While it waits, dsh ignores other requests; Codex handles them as
usual (auto-decline, or `request` and exit 2).

### steer

```sh
agent-bridge codex steer --thread <threadId> --text "Skip the tests"
```

Adds input to the running turn and prints `steered`. Without `--turn`, the CLI
reads the thread and picks the active turn, or exits 4 if there is none.

### interrupt

```sh
agent-bridge codex interrupt --thread <threadId>
```

Interrupts the active turn (found the same way) and prints `interrupted`.
The turn's outcome is reported by a later `wait` as `status: "interrupted"`.

### status and read

```sh
agent-bridge codex status --thread <threadId>
agent-bridge codex read --thread <threadId>
```

`status` prints `{"threadId","status","cwd","model"}`; Codex keeps its status
object, dsh uses the string `idle` or `running`. `read` prints
`{"threadId","turns":[{"id","status"}],"finalMessage"}`. Neither has an
`event` field and neither opens a pane.

### daemon

```sh
agent-bridge dsh daemon status
agent-bridge dsh daemon start
agent-bridge dsh daemon stop
agent-bridge dsh daemon restart
```

See [Daemons](#daemons).

### Wait options

`run` (without `--no-wait`), `wait`, and `reply` accept:

| Option | Default | Meaning |
|---|---|---|
| `--stall-secs <n>` | 600 | exit 3 with `stalled` when no progress arrives for this long during an active turn; `0` disables |
| `--timeout-secs <n>` | 3600 | exit 3 with `timeout` this long after the wait started; `0` disables |
| `--auto-decline` | off | answer every server request with a JSON-RPC error saying the run is unattended, log it, and keep waiting |
| `--follow` | off | print every event as its own line and keep running; `request` and `stalled` no longer end the process, turn completion and `timeout` still do |

Progress is a main-thread `item/completed` notification for Codex (turn start
and answering or auto-declining a request also count) and any `thread/event`
notification for dsh. Status changes alone are not progress.

### Exit codes

| Code | Meaning | stdout |
|---|---|---|
| 0 | the turn completed, or a non-waiting command succeeded | `turn`, or the command's own output |
| 1 | the turn failed or was interrupted | `turn` |
| 2 | a server request is pending (no `--auto-decline`, no `--follow`); it stays pending | `request` |
| 3 | stalled (without `--follow`) or timed out | `stalled` / `timeout` |
| 4 | usage, configuration, connection, protocol, daemon readiness, stdout, or refusal error | `error`, except for usage errors and failed stdout writes |

Usage errors (missing, unknown, invalid, or conflicting options) print clap's
message on stderr and exit 4; code 2 is reserved for pending requests.
Refusals include an active thread passed to `run`, stopping a daemon the CLI
did not start, an ambiguous or unmatched `reply`, and no active turn for
`steer` or `interrupt`. If stdout cannot be written (for example the reader
closed the pipe), the CLI prints one line on stderr and exits 4.

### Events

`?` marks an optional field. Omission and `null` are distinct.

| Event | Fields |
|---|---|
| `started` | `threadId`, `turnId`, `model`?, `effort`?, `logPath`?; dsh adds `title` and `uiUrl` |
| `turn` | `status` (`completed`, `failed`, `interrupted`), `threadId`, `turnId`, `finalMessage`, `error`?, `durationMs`?, `logPath`?; dsh adds `reason`? |
| `request` | `threadId`, `requestId`, `method`, `params` (verbatim from the server) |
| `stalled` | `threadId`, `turnId`, `stallSecs`, `finalMessage` |
| `timeout` | `threadId`, `turnId`, `timeoutSecs`, `finalMessage` |
| `steered` | `threadId`, `result` (the server's response) |
| `interrupted` | `threadId`, `turnId`, `result` |
| `stopped` | `url`, `pid` (`null` when nothing was recorded) |
| `error` | `message` |

Codex has no `title`, `uiUrl`, or `reason`. `started.model` and
`started.effort` are the values the server reported in its thread-start or
thread-resume response; missing values are omitted, never substituted from the
command line or configuration. `turnId` and the ids in `read` are always
strings, including dsh's integer turn numbers. `result` objects keep the
backend's response shape.

`finalMessage` is the last assistant text of the turn. For Codex it is the
text of the last completed `agentMessage` item on the main thread. For dsh the
plugin's `finalMessage` in `turn/completed` is authoritative when present;
otherwise the CLI uses the last nonempty assistant text it saw. On an idle
thread, `read` and `wait` report the newest turn's `finalMessage`, or an empty
string.

Daemon commands print a state object instead of an event:
`{"url","ready","managed","pid"?}`, plus `uiUrl` for dsh. `daemon restart`
prints `stopped` first.

### Logs

By default every thread gets `<state dir>/<backend>/logs/<threadId>.jsonl`,
appended to on every command. `--log <file>` appends to that file instead;
`--no-log` disables logging. `started` and `turn` carry `logPath` only when a
log is active. A new thread's log starts when the server returns the thread id;
nothing earlier is backfilled.

The log contains every stdout event plus log-only entries: `item` (progress
summaries; for dsh the `seq`, wire `type` as `itemType`, and unchanged `data`
of each `thread/event`), `status`, `replied`, `autoDeclined`,
`requestResolved`, and `serverError` (Codex). `--follow` does not turn these
into stdout events.

## Backend differences

| Area | Codex | dsh |
|---|---|---|
| `run --approval` | `untrusted`, `on-request`, `never` | `ask`, `never` |
| `reply --decision` | `accept`, `accept-for-session`, `decline` | `accept` (sent as `allowed-once`), `decline` (sent as `rejected`) |
| `run --title` | not available | optional session title |
| `--no-pane` | available | not available |
| Thread id | string assigned by the app-server | `bridge-<uuid>` for threads the plugin created; a dsh session id otherwise |
| Turn id | string assigned by the app-server | integer on the wire, printed as a string |
| Request id | number (`--request-id 56`) | string (`--request-id req-3`) |
| Request methods | app-server methods: `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/tool/requestUserInput`, `item/permissions/requestApproval`, `mcpServer/elicitation/request`, `item/tool/call` | `approval/request`, `userQuestion/request` |
| `request.params` | the app-server's params; shapes in `docs/codex-schema/*Params.json` | `{threadId, requestId, toolName, callId?, reason?}` or `{threadId, requestId, questions}` |
| `--decision` applies to | command and file-change approvals (`{"decision"}`), elicitation (`{"action"}`, `accept-for-session` refused) | `approval/request` only |
| `--result-json` needed for | `item/tool/requestUserInput` (`{"answers":{"<questionId>":{"answers":["..."]}}}`), `item/permissions/requestApproval` (`{"permissions":{...},"scope":"turn"}`), `item/tool/call`, and any other method; shapes in `docs/codex-schema/*Response.json` | `userQuestion/request` (`{"answers":[{"id","selected":["..."],"custom"?}]}`); may also carry `{"decision":"allowed-once"}` or `{"decision":"rejected"}` |
| Handshake | `initialize` then `initialized` | `initialize` only |
| Busy state | `status.type == "active"` | `status == "running"`; the plugin also rejects a racing `turn/start` with `thread_busy` |
| Subagents | discovered on the main thread and unsubscribed; their events ignored | the plugin only registers root sessions and refuses to resume subagent sessions |
| Human view | Herdr pane | browser |

A `--decision` that has no mapping for the request's method is an error
(exit 4) and nothing is sent; use `--result-json`. An unknown `--decision`
value is a usage error.

## Daemons

Every verb except `daemon` probes `GET /readyz` on the HTTP form of `--url`
and, if that fails, spawns the backend as a detached process and polls
readiness every 250 ms for up to 15 seconds
(`AGENT_BRIDGE_<BACKEND>_READY_TIMEOUT_MS`). Another process becoming ready on
the same port also satisfies the check.

| Backend | Spawn command |
|---|---|
| Codex | `codex app-server --listen <url>` |
| dsh | `dsh --profile <profile> --port <ui port>`, or `node <AGENT_BRIDGE_DSH_BIN> --profile <profile> --port <ui port>` |

The child starts in the user's home directory (falling back to the backend
state directory), its stdin is closed, and its stdout and stderr are appended
to `daemon.log`. It does not inherit the caller's standard handles, so piping
the CLI's stdout does not keep the pipe open after the CLI exits. On Windows
the child gets a hidden console (`CREATE_NO_WINDOW`); on Unix it is put in its
own session. Its pid is written to `daemon.pid`. Codex receives no `-c`
overrides; dsh receives neither `--no-open` nor any permission-mode variable.
The dsh UI port and the plugin's WebSocket port are different things: `--url`
selects the plugin endpoint and does not configure the plugin's listener.

| Action | Behaviour |
|---|---|
| `status` | probe only; `managed` is true when the recorded pid is alive |
| `start` | ensure readiness (spawning if needed), then print the state |
| `stop` | terminate the recorded pid, delete the pid file, print `stopped` |
| `restart` | `stop`, wait until `/readyz` stops answering (at most half the readiness timeout), then `start` |

`stop` and `restart` refuse with exit 4 a server that answers `/readyz` but has
no recorded pid: whoever started it by hand stops it. A stale pid file is
removed and reported as stopped.

For dsh, `daemon status` also prints `uiUrl`: the last complete
`dsh web: http://...` line in `daemon.log`, including its login token, or
`http://127.0.0.1:<ui port>/` when there is none. `status` never waits, so a
stale log can return a previous process's token. After `start` or `restart`
actually spawn a process, they wait up to `AGENT_BRIDGE_DSH_UI_URL_WAIT_MS`
(5000) for a new `dsh web:` line written by that process; on timeout they warn
on stderr and fall back to the last logged URL. Other verbs never wait for this
line.

## Human view

### Codex: Herdr pane

`run`, `wait`, `reply`, and `steer` open a terminal pane with a Codex TUI
attached to the thread when all of these hold: `HERDR_ENV=1` is set,
`--no-pane` is absent, and a `herdr` executable is found (on `PATH`, or
`AGENT_BRIDGE_CODEX_HERDR_BIN`). The pane is named `codex-` plus the first 8
characters of the thread id and is reused across commands for the same thread:
an existing agent with that name means nothing is done; a pane still carrying
that label (its TUI closed) gets Codex started again; otherwise a new pane is
split to the right (ratio 0.45) in the thread's cwd, labelled, and after a
short settling delay Herdr runs `codex resume <threadId> --remote <url>` in it.
Pane problems are one line on stderr each and never affect stdout or the exit
code. The CLI never closes a pane. Ctrl+C in the TUI detaches that view; the
server's turn keeps running.

To open a remote TUI by hand for a new thread, pass the working directory,
because new remote threads otherwise inherit the daemon's cwd (the home
directory):

```sh
codex --remote ws://127.0.0.1:12897 -C /work/repo
```

Resuming a known thread keeps its recorded cwd and needs no `-C`:

```sh
codex resume <threadId> --remote ws://127.0.0.1:12897
```

### dsh: browser

Open the `uiUrl` from `started` or from `agent-bridge dsh daemon status` and
find the session by its `title` in the workspace sidebar; there is no per-session
link. `started.uiUrl` prefers the last complete `dsh web:` URL in `daemon.log`
(with its token), then the token-free URL from the plugin handshake, then the
default UI URL.

Opening a token URL logs the browser in with a cookie valid for 30 days that
survives daemon restarts; the token itself changes with every process. dsh
opens a browser on its own every time the daemon is launched.

The browser shows progress and lets a human steer or cancel. Approvals and
questions raised during a turn the bridge started are delivered to the bridge
client (`request` event, `reply`) and do not open a browser dialog. A turn a
human starts from the browser on the same session belongs to the browser:
its approvals open there, the bridge sees only `thread/event` and
`thread/status` traffic, and `run --thread` on that session is refused as busy
until it ends.

## Environment variables

Empty values count as unset.

| Shared | Purpose |
|---|---|
| `AGENT_BRIDGE_CONFIG` | configuration file path; default per [Directories](#directories) |
| `AGENT_BRIDGE_STATE_DIR` | state root; `codex/` or `dsh/` is appended |
| `XDG_DATA_HOME` | configuration base (`<value>/agent-bridge/config.toml`) on every platform |
| `XDG_STATE_HOME` | state root base (`<value>/agent-bridge`) on every platform |
| `HOME` (Unix), `USERPROFILE` (Windows) | home for the default paths and the daemon's cwd |
| `PATH` | lookup of `codex`, `herdr`, `dsh`, and `node` when no explicit executable is set |

| Codex | Default and purpose |
|---|---|
| `AGENT_BRIDGE_CODEX_URL` | `ws://127.0.0.1:12897`; endpoint, overridden by `--url` |
| `AGENT_BRIDGE_CODEX_BIN` | `codex`; executable used to spawn the app-server |
| `AGENT_BRIDGE_CODEX_HERDR_BIN` | `herdr` on `PATH`; Herdr executable for panes |
| `AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS` | `15000`; readiness deadline, and twice the restart shutdown deadline |
| `AGENT_BRIDGE_CODEX_PANE_DELAY_MS` | `2000`; settling delay after splitting a pane |
| `AGENT_BRIDGE_CODEX_PANE_RETRY_MS` | `500`; delay between agent-rename retries |
| `HERDR_ENV` | `1` enables pane integration |

| dsh | Default and purpose |
|---|---|
| `AGENT_BRIDGE_DSH_URL` | `ws://127.0.0.1:12898`; plugin endpoint, overridden by `--url` |
| `AGENT_BRIDGE_DSH_BIN` | unset; dsh's JavaScript entry point, run with Node. When unset, `dsh` is looked up on `PATH` (`dsh.exe`, `dsh.com`, `dsh.cmd`, `dsh.bat` on Windows); if it is not there the CLI exits 4 asking for this variable |
| `AGENT_BRIDGE_DSH_NODE_BIN` | unset; Node executable used with `AGENT_BRIDGE_DSH_BIN`. When unset, Windows searches `PATH` for `node.exe`, `node.cmd`, `node.bat`; Unix runs `node` |
| `AGENT_BRIDGE_DSH_PROFILE` | `bridge`; dsh profile passed as `--profile` |
| `AGENT_BRIDGE_DSH_UI_PORT` | `12899`; browser UI port passed as `--port` |
| `AGENT_BRIDGE_DSH_READY_TIMEOUT_MS` | `15000`; readiness deadline, and twice the restart shutdown deadline |
| `AGENT_BRIDGE_DSH_UI_URL_WAIT_MS` | `5000`; how long `daemon start`/`restart` wait for the new `dsh web:` line |

`DSH_BRIDGE_PORT` is read by the plugin inside dsh, not by this CLI; see the
plugin README.

## Use from Claude Code

Run `agent-bridge <backend> run ...` in a background shell and act on its exit
code: 0 means read `finalMessage`, 2 means answer the `request` with `reply`,
3 means decide whether to `wait` again or `interrupt`. For reactions during a
turn, subscribe a monitor to `agent-bridge <backend> wait --follow --thread
<id>` and use `steer`. Wrapping this in a Claude Code skill keeps the
dispatch rules in one place.

## Known limitations

- dsh approvals are one-time: there is no `accept-for-session`, and the plugin
  treats any other decision as a rejection.
- When `wait` or `reply` attaches in the middle of a turn, `finalMessage` in
  `stalled` and `timeout` events, and in Codex `turn` events, contains only
  assistant text seen since attaching. dsh `turn` events carry the plugin's
  own final message for the whole turn.
- Stall detection only sees events. A long model-generation step that emits
  no items can exceed `--stall-secs`; raise it or use `0` for such work.
- `reply` without `--request-id` on an idle thread spends the full 10-second
  window and then exits 4; use `wait` to read an idle thread's outcome.
- `read` on dsh reports only the newest turn's `finalMessage`.
- dsh `daemon status` can report a token from a previous daemon process, and
  dsh opens a browser window on every daemon launch.
- The Codex pane is never closed by the CLI.
- A dsh thread resumed after a daemon restart uses the server's current model
  selection, not necessarily the one it was created with.

## Development

```sh
cargo fmt
cargo build
cargo test
cargo clippy --all-targets -- -D warnings

cd dsh-plugin
pnpm install
pnpm test
pnpm typecheck
```

Rust tests use temporary state roots, mock HTTP/WebSocket servers on ephemeral
ports, and fake `codex`, `node`, and `herdr` executables; plugin tests run the
real bridge and JSON-RPC framing against fake dsh services. No test needs a
real backend.

```text
src/main.rs          top-level parsing and backend dispatch
src/cli.rs           shared argument shapes and verbs
src/core/            configuration, daemons, logs, waits, replies, output
src/backend/         backend trait, normalized events, codex/ and dsh/ adapters
tests/               Rust integration tests and protocol mocks
docs/codex-schema/   JSON schemas of the Codex app-server protocol, generated with `codex app-server generate-json-schema`
dsh-plugin/          the dsh plugin (npm package dsh-bridge-plugin)
```

## License

MIT. See [LICENSE](LICENSE).
