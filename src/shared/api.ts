/**
 * The contract between the renderer and the main process.
 *
 * `HerseApi` is what `window.herse` exposes. Keeping it here (rather than in
 * the preload) means the renderer and the main-process handlers are typechecked
 * against the same declaration — if you change a handler signature, the call
 * sites break at compile time.
 */

import type { Notebook } from './nbformat'
import type {
  CompleteReplyContent,
  InspectReplyContent,
  JupyterMessage,
  KernelSpec,
  KernelStatus,
  SessionInfo,
} from './jupyter'

export interface DirEntry {
  name: string
  path: string
  isDirectory: boolean
  size: number
  modifiedAt: number
  /** Populated for `.ipynb` files so the browser can show useful metadata. */
  notebook?: NotebookSummary
}

export interface NotebookSummary {
  cellCount: number
  codeCellCount: number
  kernelDisplayName?: string
  language?: string
  /** True when any code cell has stored outputs. */
  hasOutputs: boolean
}

export interface SearchHit {
  path: string
  name: string
  cellId: string
  cellIndex: number
  cellType: string
  /** The matching line, trimmed, with surrounding context stripped. */
  preview: string
  lineNumber: number
}

export interface OpenedNotebook {
  path: string
  notebook: Notebook
  modifiedAt: number
}

/**
 * `conflict` means the file changed on disk since the caller last read it, and
 * nothing was written. Auto-save uses this to stop rather than clobber edits
 * made outside the app; an explicit Save skips the check and overwrites.
 */
export type SaveResult =
  | { status: 'saved'; modifiedAt: number }
  | { status: 'conflict'; modifiedAt: number }

export interface WorkspaceState {
  root: string | null
  recentFolders: string[]
  recentNotebooks: string[]
}

/**
 * Every message the kernel sends us is forwarded verbatim, tagged with the
 * channel it arrived on. The renderer owns the msg_id -> cell mapping, so it
 * needs the shell `execute_reply` (final execution_count, error status) just as
 * much as the iopub stream.
 */
export interface KernelMessageEvent {
  sessionId: string
  channel: 'shell' | 'iopub' | 'stdin' | 'control'
  message: JupyterMessage
}

export interface KernelStatusEvent {
  sessionId: string
  status: KernelStatus
  /** Set when the kernel exited unexpectedly. */
  reason?: string
}

export interface HerseApi {
  workspace: {
    getState(): Promise<WorkspaceState>
    /** Opens the OS folder picker; resolves null if the user cancels. */
    pickFolder(): Promise<string | null>
    setRoot(path: string): Promise<WorkspaceState>
    list(dirPath: string): Promise<DirEntry[]>
    /** Recursive filename match for the quick-open palette. */
    findFiles(query: string, limit?: number): Promise<DirEntry[]>
    /** Full-text search across cell sources of every notebook under the root. */
    search(query: string, limit?: number): Promise<SearchHit[]>
    revealInOS(path: string): Promise<void>
  }

  notebook: {
    open(path: string): Promise<OpenedNotebook>
    /**
     * Pass `expectedModifiedAt` to make the write conditional on the file not
     * having changed underneath you. Omit it to overwrite unconditionally.
     */
    save(path: string, notebook: Notebook, expectedModifiedAt?: number): Promise<SaveResult>
    /** Opens a save dialog; resolves null if the user cancels. */
    saveAs(notebook: Notebook, suggestedPath?: string): Promise<{ path: string; modifiedAt: number } | null>
    pickFile(): Promise<string | null>
  }

  kernels: {
    listSpecs(): Promise<KernelSpec[]>
    /** Boots a kernel process and returns once its channels are connected. */
    start(kernelName: string, cwd: string): Promise<SessionInfo>
    listSessions(): Promise<SessionInfo[]>
    shutdown(sessionId: string): Promise<void>
    restart(sessionId: string): Promise<SessionInfo>
    interrupt(sessionId: string): Promise<void>
    /** Returns the msg_id; outputs arrive asynchronously on the iopub channel. */
    execute(sessionId: string, code: string): Promise<string>
    complete(sessionId: string, code: string, cursorPos: number): Promise<CompleteReplyContent>
    inspect(sessionId: string, code: string, cursorPos: number, detailLevel?: 0 | 1): Promise<InspectReplyContent>
    /** Answers an `input_request` from the kernel's stdin channel. */
    sendInput(sessionId: string, value: string): Promise<void>
  }

  events: {
    /** Every subscribe returns its own unsubscribe function. */
    onMessage(handler: (event: KernelMessageEvent) => void): () => void
    onStatus(handler: (event: KernelStatusEvent) => void): () => void
    /** Fired when the main process wants the UI to run a menu command. */
    onMenuCommand(handler: (command: string) => void): () => void
  }

  system: {
    platform: NodeJS.Platform
    openExternal(url: string): Promise<void>
  }
}

declare global {
  interface Window {
    herse: HerseApi
  }
}
