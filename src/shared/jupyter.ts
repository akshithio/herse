/**
 * Jupyter messaging protocol (v5.3) types shared by main and renderer.
 * https://jupyter-client.readthedocs.io/en/stable/messaging.html
 */

import type { MimeBundle } from './nbformat'

export interface MessageHeader {
  msg_id: string
  session: string
  username: string
  date: string
  msg_type: string
  version: string
}

export interface JupyterMessage<C = Record<string, unknown>> {
  header: MessageHeader
  parent_header: Partial<MessageHeader>
  metadata: Record<string, unknown>
  content: C
  /** Binary buffers ride alongside the JSON frames; base64 across the IPC bridge. */
  buffers?: string[]
}

export type KernelStatus = 'starting' | 'idle' | 'busy' | 'restarting' | 'dead' | 'unknown'

export interface KernelSpec {
  /** Directory name of the kernelspec — the stable identifier. */
  name: string
  display_name: string
  language: string
  argv: string[]
  env?: Record<string, string>
  interrupt_mode?: 'signal' | 'message'
  metadata?: Record<string, unknown>
  /** Absolute path to the kernelspec directory, for display/debugging. */
  resource_dir?: string
}

export interface SessionInfo {
  id: string
  kernelName: string
  displayName: string
  language: string
  status: KernelStatus
  cwd: string
  /** PID of the kernel process, null once it exits. */
  pid: number | null
  startedAt: number
}

/* ---- Common message content shapes we actually branch on ---- */

export interface StatusContent {
  execution_state: 'busy' | 'idle' | 'starting'
}

export interface StreamContent {
  name: 'stdout' | 'stderr'
  text: string
}

export interface ExecuteInputContent {
  code: string
  execution_count: number
}

export interface ExecuteResultContent {
  execution_count: number
  data: MimeBundle
  metadata: Record<string, unknown>
}

export interface DisplayDataContent {
  data: MimeBundle
  metadata: Record<string, unknown>
  transient?: { display_id?: string }
}

export interface ErrorContent {
  ename: string
  evalue: string
  traceback: string[]
}

export interface ExecuteReplyContent {
  status: 'ok' | 'error' | 'abort'
  execution_count: number
  ename?: string
  evalue?: string
  traceback?: string[]
}

export interface InputRequestContent {
  prompt: string
  password: boolean
}

export interface ClearOutputContent {
  wait: boolean
}

export interface CompleteReplyContent {
  status: 'ok' | 'error'
  matches: string[]
  cursor_start: number
  cursor_end: number
  metadata: Record<string, unknown>
}

export interface InspectReplyContent {
  status: 'ok' | 'error'
  found: boolean
  data: Record<string, unknown>
  metadata: Record<string, unknown>
}

export interface KernelInfoReplyContent {
  status: 'ok' | 'error'
  protocol_version: string
  implementation: string
  implementation_version: string
  language_info: {
    name: string
    version: string
    mimetype: string
    file_extension: string
    pygments_lexer?: string
  }
  banner: string
}
