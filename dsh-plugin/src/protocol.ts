/**
 * Pure wire protocol types and constants. JSON-RPC 2.0 over WebSocket, with shapes matching a subset of the Codex app-server.
 * This layer does not touch dsh, so the M1 Rust CLI and vitest can share the same protocol table.
 */

/** JSON-RPC request id: string or number; server-initiated reverse requests always use strings. */
export type RpcId = string | number

export interface RpcRequest {
  jsonrpc: '2.0'
  id: RpcId
  method: string
  params?: unknown
}

export interface RpcNotification {
  jsonrpc: '2.0'
  method: string
  params?: unknown
}

export interface RpcErrorBody {
  code: number
  message: string
  data?: unknown
}

export interface RpcResponse {
  jsonrpc: '2.0'
  id: RpcId | null
  result?: unknown
  error?: RpcErrorBody
}

/** `data.kind`: stable error categories that clients can branch on. */
export type BridgeErrorKind =
  | 'thread_not_found'
  | 'thread_busy'
  | 'no_active_turn'
  | 'turn_mismatch'
  | 'invalid_cwd'
  | 'not_resumable'
  | 'invalid_params'
  | 'internal'

/** All business errors share -32000; their category goes in data.kind. */
export const BRIDGE_ERROR_CODE = -32000

export class BridgeError extends Error {
  constructor(readonly kind: BridgeErrorKind, message: string) {
    super(message)
    this.name = 'BridgeError'
  }

  toBody(): RpcErrorBody {
    return { code: BRIDGE_ERROR_CODE, message: this.message, data: { kind: this.kind } }
  }
}

/** Thread status, with a one-to-one mapping to dsh's `agent.status`. */
export type ThreadStatus = 'idle' | 'running'

/** Terminal turn status; dsh's `turn/end` reasons map to these three values. */
export type TurnStatus = 'completed' | 'interrupted' | 'failed'

/** Allowlist of session event types forwarded unchanged to clients, for logging and stall timer progress signals. */
export const FORWARDED_EVENT_TYPES: readonly string[] = [
  'user/message',
  'assistant/message',
  'tool/call',
  'tool/result',
  'step/start',
  'step/end',
  'turn/start',
  'turn/end',
  'session/title',
]

export const SANDBOX_MODE_VALUES: readonly string[] = ['read-only', 'workspace-write', 'danger-full-access']
export const APPROVAL_POLICY_VALUES: readonly string[] = ['ask', 'never']

/** Reverse request method names (server -> client). */
export const APPROVAL_REQUEST_METHOD = 'approval/request'
export const USER_QUESTION_REQUEST_METHOD = 'userQuestion/request'

/** Valid approval decisions; all other values are treated as `rejected`. */
export const APPROVAL_DECISIONS: readonly string[] = ['allowed-once', 'rejected']

/** The party that resolved the request in `request/resolved`. */
export type ResolvedBy = 'client' | 'browser' | 'cancelled'
