import { create } from 'zustand'
import type { WorkspaceState } from '@shared/api'
import type { JupyterMessage, KernelSpec, KernelStatus, SessionInfo } from '@shared/jupyter'
import type { Cell, CellType, Notebook } from '@shared/nbformat'
import { emptyNotebook, isCodeCell, newCell } from '@shared/nbformat'
import { applyMessage } from './outputs'

/* -------------------------------------------------------------------------- */
/* Types                                                                      */
/* -------------------------------------------------------------------------- */

export interface CellRun {
  /** Sent to the kernel but no execute_input yet — shown as `[*]`. */
  queued: boolean
  running: boolean
  startedAt: number | null
  elapsedMs: number | null
  errored: boolean
}

export interface PendingInput {
  cellId: string
  prompt: string
  password: boolean
}

export interface Doc {
  /**
   * Identity for tabs and message routing. For a saved notebook this is its
   * file path; for an untitled one it's a synthetic `untitled:…` key that never
   * reaches the filesystem.
   */
  path: string
  name: string
  notebook: Notebook
  dirty: boolean
  /** True until the notebook has been given a location on disk. */
  untitled: boolean
  /** mtime of the file as of our last read or write; 0 for untitled. */
  savedAt: number
  sessionId: string | null
  selectedCellId: string | null
  /** Markdown cells currently in edit (rather than rendered) mode. */
  editing: Record<string, true>
  /** kernel msg_id -> cell id, for routing iopub messages back to a cell. */
  msgToCell: Record<string, string>
  runs: Record<string, CellRun>
  pendingInput: PendingInput | null
  /** Set to scroll a cell into view once, then cleared. */
  revealCellId: string | null
}

export type PaletteMode = 'commands' | 'files' | null

interface HerseState {
  workspace: WorkspaceState
  specs: KernelSpec[]
  sessions: Record<string, SessionInfo>

  docs: Record<string, Doc>
  order: string[]
  activePath: string | null

  sidebarVisible: boolean
  palette: PaletteMode
  toast: { message: string; tone: 'info' | 'error' } | null

  /* actions */
  init(): Promise<void>
  openFolder(): Promise<void>
  setRoot(path: string): Promise<void>

  openNotebook(path: string, revealCellId?: string): Promise<void>
  newNotebook(): void
  openNotebookDialog(): Promise<void>
  closeDoc(path: string): void
  setActive(path: string): void
  save(path?: string): Promise<void>
  saveAs(path?: string): Promise<void>
  /** Conditional write used by the debounced autosave; no-ops when clean. */
  autosave(path: string): Promise<void>

  selectCell(path: string, cellId: string): void
  setEditing(path: string, cellId: string, editing: boolean): void
  updateSource(path: string, cellId: string, source: string): void
  insertCell(path: string, at: 'above' | 'below', type?: CellType): void
  deleteCell(path: string): void
  moveCell(path: string, direction: -1 | 1): void
  changeCellType(path: string, type: CellType): void
  clearOutputs(path: string, cellId?: string): void

  ensureSession(path: string, kernelName?: string): Promise<string>
  runCell(path: string, cellId?: string, advance?: boolean): Promise<void>
  runAll(path: string): Promise<void>
  interrupt(path: string): Promise<void>
  restartKernel(path: string, runAll?: boolean): Promise<void>
  shutdownKernel(path: string): Promise<void>
  submitInput(path: string, value: string): Promise<void>

  setPalette(mode: PaletteMode): void
  toggleSidebar(): void
  notify(message: string, tone?: 'info' | 'error'): void

  /* event sinks, wired up in App */
  handleKernelMessage(sessionId: string, channel: string, message: JupyterMessage): void
  handleKernelStatus(sessionId: string, status: KernelStatus, reason?: string): void
}

/* -------------------------------------------------------------------------- */
/* Helpers                                                                    */
/* -------------------------------------------------------------------------- */

const IDLE_RUN: CellRun = { queued: false, running: false, startedAt: null, elapsedMs: null, errored: false }

/**
 * Kernels start publishing on iopub the moment they accept an execute_request —
 * which can beat the IPC round-trip that tells the renderer the msg_id. Messages
 * whose parent we don't recognise yet are parked here and replayed as soon as
 * the cell mapping is registered.
 */
const orphaned = new Map<string, Array<{ sessionId: string; channel: string; message: JupyterMessage }>>()
const MAX_ORPHAN_KEYS = 64

function park(parentId: string, entry: { sessionId: string; channel: string; message: JupyterMessage }): void {
  // Drop the oldest key when we're clearly buffering messages nobody will claim
  // (kernel_info during boot, replies to a cell that was deleted mid-run).
  if (!orphaned.has(parentId) && orphaned.size >= MAX_ORPHAN_KEYS) {
    const oldest = orphaned.keys().next().value
    if (oldest !== undefined) orphaned.delete(oldest)
  }
  const list = orphaned.get(parentId) ?? []
  list.push(entry)
  orphaned.set(parentId, list)
}

function baseName(path: string): string {
  return path.split(/[\\/]/).pop() ?? path
}

function dirName(path: string): string {
  const parts = path.split(/[\\/]/)
  parts.pop()
  return parts.join('/') || '/'
}

function cellIndex(doc: Doc, cellId: string): number {
  return doc.notebook.cells.findIndex((c) => c.id === cellId)
}

function activeDoc(state: HerseState): Doc | null {
  return state.activePath ? (state.docs[state.activePath] ?? null) : null
}

/* ---- Autosave bookkeeping (module-level; not part of rendered state) ---- */

/** Quiet period after the last edit before writing. */
const AUTOSAVE_DELAY_MS = 1000
const autosaveTimers = new Map<string, ReturnType<typeof setTimeout>>()
/** Paths where autosave has backed off — disk conflict or write failure. */
const autosaveBlocked = new Set<string>()

type PatchDoc = (path: string, update: (doc: Doc) => Doc) => void

/**
 * Clear `dirty` only if nothing changed while the write was in flight —
 * otherwise edits made during the save would be marked as already saved.
 */
function markSaved(patchDoc: PatchDoc, path: string, snapshot: Notebook, modifiedAt: number): void {
  patchDoc(path, (doc) => ({
    ...doc,
    savedAt: modifiedAt,
    dirty: doc.notebook === snapshot ? false : doc.dirty,
  }))
}

/* -------------------------------------------------------------------------- */
/* Store                                                                      */
/* -------------------------------------------------------------------------- */

export const useStore = create<HerseState>((set, get) => {
  /** Apply a pure update to one doc, leaving everything else untouched. */
  function patchDoc(path: string, update: (doc: Doc) => Doc): void {
    set((state) => {
      const doc = state.docs[path]
      if (!doc) return state
      return { docs: { ...state.docs, [path]: update(doc) } }
    })
  }

  /** Replace one cell by id. Marks the doc dirty unless told otherwise. */
  function patchCell(
    path: string,
    cellId: string,
    update: (cell: Cell) => Cell,
    options: { dirty?: boolean } = {},
  ): void {
    patchDoc(path, (doc) => ({
      ...doc,
      dirty: options.dirty === false ? doc.dirty : true,
      notebook: {
        ...doc.notebook,
        cells: doc.notebook.cells.map((cell) => (cell.id === cellId ? update(cell) : cell)),
      },
    }))
  }

  function setRun(path: string, cellId: string, update: Partial<CellRun>): void {
    patchDoc(path, (doc) => ({
      ...doc,
      runs: { ...doc.runs, [cellId]: { ...(doc.runs[cellId] ?? IDLE_RUN), ...update } },
    }))
  }

  return {
    workspace: { root: null, recentFolders: [], recentNotebooks: [] },
    specs: [],
    sessions: {},
    docs: {},
    order: [],
    activePath: null,
    sidebarVisible: true,
    palette: null,
    toast: null,

    /* ------------------------------ bootstrap ----------------------------- */

    async init() {
      const [workspace, specs, sessions] = await Promise.all([
        window.herse.workspace.getState(),
        window.herse.kernels.listSpecs(),
        window.herse.kernels.listSessions(),
      ])
      set({
        workspace,
        specs,
        sessions: Object.fromEntries(sessions.map((s) => [s.id, s])),
      })
    },

    async openFolder() {
      const path = await window.herse.workspace.pickFolder()
      if (path) await get().setRoot(path)
    },

    async setRoot(path) {
      set({ workspace: await window.herse.workspace.setRoot(path) })
    },

    /* ------------------------------ documents ----------------------------- */

    async openNotebook(path, revealCellId) {
      const existing = get().docs[path]
      if (existing) {
        set({ activePath: path, palette: null })
        if (revealCellId) patchDoc(path, (doc) => ({ ...doc, revealCellId, selectedCellId: revealCellId }))
        return
      }

      try {
        const opened = await window.herse.notebook.open(path)
        const doc: Doc = {
          path,
          name: baseName(path),
          notebook: opened.notebook,
          dirty: false,
          untitled: false,
          savedAt: opened.modifiedAt,
          sessionId: null,
          selectedCellId: revealCellId ?? opened.notebook.cells[0]?.id ?? null,
          editing: {},
          msgToCell: {},
          runs: {},
          pendingInput: null,
          revealCellId: revealCellId ?? null,
        }

        set((state) => ({
          docs: { ...state.docs, [path]: doc },
          order: state.order.includes(path) ? state.order : [...state.order, path],
          activePath: path,
          palette: null,
        }))

        set({ workspace: await window.herse.workspace.getState() })
      } catch (error) {
        get().notify(`Could not open ${baseName(path)}: ${(error as Error).message}`, 'error')
      }
    },

    /**
     * Creates the notebook in memory and asks for a location on first save.
     * Nothing touches the disk here, so this works with or without a folder
     * open — needing a workspace just to start typing is the wrong trade.
     */
    newNotebook() {
      const { specs, docs } = get()

      // Default to the first Python kernel if there is one; it's the common case.
      const preferred = specs.find((s) => s.language === 'python') ?? specs[0]
      const notebook = emptyNotebook(
        preferred
          ? { name: preferred.name, display_name: preferred.display_name, language: preferred.language }
          : undefined,
        preferred ? { name: preferred.language } : undefined,
      )

      const taken = new Set(Object.values(docs).map((doc) => doc.name))
      let n = 1
      while (taken.has(n === 1 ? 'Untitled' : `Untitled ${n}`)) n++
      const name = n === 1 ? 'Untitled' : `Untitled ${n}`

      const doc: Doc = {
        path: `untitled:${Date.now()}-${n}`,
        name,
        notebook,
        // Untitled notebooks start dirty: there is unsaved work by definition.
        dirty: true,
        untitled: true,
        savedAt: 0,
        sessionId: null,
        selectedCellId: notebook.cells[0]?.id ?? null,
        editing: {},
        msgToCell: {},
        runs: {},
        pendingInput: null,
        revealCellId: null,
      }

      set((state) => ({
        docs: { ...state.docs, [doc.path]: doc },
        order: [...state.order, doc.path],
        activePath: doc.path,
        palette: null,
      }))
    },

    async openNotebookDialog() {
      const path = await window.herse.notebook.pickFile()
      if (path) await get().openNotebook(path)
    },

    closeDoc(path) {
      // Don't drop an edit that was still inside the autosave debounce window.
      flushAutosave(path)

      const doc = get().docs[path]
      if (doc?.sessionId) void window.herse.kernels.shutdown(doc.sessionId).catch(() => {})

      set((state) => {
        const docs = { ...state.docs }
        delete docs[path]
        const order = state.order.filter((p) => p !== path)
        const activePath =
          state.activePath === path ? (order[order.length - 1] ?? null) : state.activePath
        return { docs, order, activePath }
      })
    },

    setActive(path) {
      set({ activePath: path, palette: null })
    },

    async save(path) {
      const target = path ?? get().activePath
      if (!target) return
      const doc = get().docs[target]
      if (!doc) return

      // Nowhere to write yet — Save on an untitled notebook is Save As.
      if (doc.untitled) {
        await get().saveAs(target)
        return
      }

      try {
        // Explicit Save is unconditional: the user asked for this file to win.
        const snapshot = doc.notebook
        const result = await window.herse.notebook.save(doc.path, snapshot)
        autosaveBlocked.delete(doc.path)
        markSaved(patchDoc, doc.path, snapshot, result.modifiedAt)
      } catch (error) {
        get().notify(`Save failed: ${(error as Error).message}`, 'error')
      }
    },

    async autosave(path) {
      const doc = get().docs[path]
      if (!doc || !doc.dirty || doc.untitled || autosaveBlocked.has(path)) return

      try {
        const snapshot = doc.notebook
        const result = await window.herse.notebook.save(doc.path, snapshot, doc.savedAt)

        if (result.status === 'conflict') {
          // Someone else wrote the file. Back off rather than overwrite them.
          autosaveBlocked.add(path)
          get().notify(
            `${doc.name} changed on disk — auto-save paused. Save to overwrite, or reopen the file.`,
            'error',
          )
          return
        }

        markSaved(patchDoc, path, snapshot, result.modifiedAt)
      } catch (error) {
        // Don't retry in a loop against a failing disk.
        autosaveBlocked.add(path)
        get().notify(`Auto-save failed: ${(error as Error).message}`, 'error')
      }
    },

    async saveAs(path) {
      const doc = path ? get().docs[path] : activeDoc(get())
      if (!doc) return

      const { root } = get().workspace
      // Point the dialog at the open folder when there is one.
      const suggested = doc.untitled
        ? root
          ? `${root}/${doc.name}.ipynb`
          : `${doc.name}.ipynb`
        : doc.path

      const result = await window.herse.notebook.saveAs(doc.notebook, suggested)
      if (!result) return

      // Re-key the document under its new path.
      set((state) => {
        const docs = { ...state.docs }
        delete docs[doc.path]
        docs[result.path] = {
          ...doc,
          path: result.path,
          name: baseName(result.path),
          dirty: false,
          untitled: false,
          savedAt: result.modifiedAt,
        }
        return {
          docs,
          order: state.order.map((p) => (p === doc.path ? result.path : p)),
          activePath: result.path,
        }
      })
    },

    /* -------------------------------- cells ------------------------------- */

    selectCell(path, cellId) {
      patchDoc(path, (doc) => ({ ...doc, selectedCellId: cellId, revealCellId: null }))
    },

    setEditing(path, cellId, editing) {
      patchDoc(path, (doc) => {
        const next = { ...doc.editing }
        if (editing) next[cellId] = true
        else delete next[cellId]
        return { ...doc, editing: next, selectedCellId: cellId }
      })
    },

    updateSource(path, cellId, source) {
      patchCell(path, cellId, (cell) => ({ ...cell, source }))
    },

    insertCell(path, at, type = 'code') {
      patchDoc(path, (doc) => {
        const cell = newCell(type)
        const index = doc.selectedCellId ? cellIndex(doc, doc.selectedCellId) : -1
        const insertAt = index === -1 ? doc.notebook.cells.length : at === 'above' ? index : index + 1
        const cells = [...doc.notebook.cells]
        cells.splice(insertAt, 0, cell)

        return {
          ...doc,
          dirty: true,
          notebook: { ...doc.notebook, cells },
          selectedCellId: cell.id,
          revealCellId: cell.id,
          // Focus the new cell's editor so you can type straight into it. For
          // markdown this also means edit mode — there is nothing to render yet.
          editing: { ...doc.editing, [cell.id]: true },
        }
      })
    },

    deleteCell(path) {
      patchDoc(path, (doc) => {
        const index = doc.selectedCellId ? cellIndex(doc, doc.selectedCellId) : -1
        if (index === -1) return doc

        const cells = doc.notebook.cells.filter((_, i) => i !== index)
        // A notebook with no cells has no affordance to add one back.
        if (cells.length === 0) cells.push(newCell('code'))
        const nextSelected = cells[Math.min(index, cells.length - 1)]

        return {
          ...doc,
          dirty: true,
          notebook: { ...doc.notebook, cells },
          selectedCellId: nextSelected?.id ?? null,
        }
      })
    },

    moveCell(path, direction) {
      patchDoc(path, (doc) => {
        const index = doc.selectedCellId ? cellIndex(doc, doc.selectedCellId) : -1
        const target = index + direction
        if (index === -1 || target < 0 || target >= doc.notebook.cells.length) return doc

        const cells = [...doc.notebook.cells]
        ;[cells[index], cells[target]] = [cells[target], cells[index]]
        return { ...doc, dirty: true, notebook: { ...doc.notebook, cells } }
      })
    },

    changeCellType(path, type) {
      const doc = get().docs[path]
      const cellId = doc?.selectedCellId
      if (!doc || !cellId) return

      patchCell(path, cellId, (cell) => {
        if (cell.cell_type === type) return cell
        const base = { id: cell.id, metadata: cell.metadata, source: cell.source }
        if (type === 'code') return { ...base, cell_type: 'code', execution_count: null, outputs: [] }
        if (type === 'markdown') return { ...base, cell_type: 'markdown' }
        return { ...base, cell_type: 'raw' }
      })

      // Converting to markdown should show the editor, not an empty render.
      get().setEditing(path, cellId, type === 'markdown')
    },

    clearOutputs(path, cellId) {
      patchDoc(path, (doc) => ({
        ...doc,
        dirty: true,
        runs: cellId ? { ...doc.runs, [cellId]: IDLE_RUN } : {},
        notebook: {
          ...doc.notebook,
          cells: doc.notebook.cells.map((cell) => {
            if (!isCodeCell(cell)) return cell
            if (cellId && cell.id !== cellId) return cell
            return { ...cell, outputs: [], execution_count: null }
          }),
        },
      }))
    },

    /* ------------------------------- kernels ------------------------------ */

    async ensureSession(path, kernelName) {
      const doc = get().docs[path]
      if (!doc) throw new Error('No such document')

      const existing = doc.sessionId ? get().sessions[doc.sessionId] : undefined
      if (existing && existing.status !== 'dead' && !kernelName) return existing.id

      const { specs } = get()
      const name =
        kernelName ??
        doc.notebook.metadata.kernelspec?.name ??
        specs.find((s) => s.language === 'python')?.name ??
        specs[0]?.name

      if (!name) {
        throw new Error(
          'No Jupyter kernels are installed. Try: python3 -m pip install ipykernel && python3 -m ipykernel install --user',
        )
      }

      // Retire the old session before replacing it.
      if (doc.sessionId) await window.herse.kernels.shutdown(doc.sessionId).catch(() => {})

      // An untitled notebook has no directory of its own; run it in the open
      // folder, and let main fall back to the home directory if there is none.
      const cwd = doc.untitled ? (get().workspace.root ?? '') : dirName(path)
      const info = await window.herse.kernels.start(name, cwd)
      const spec = specs.find((s) => s.name === info.kernelName)

      set((state) => ({ sessions: { ...state.sessions, [info.id]: info } }))
      patchDoc(path, (d) => {
        const kernelspec = {
          name: info.kernelName,
          display_name: info.displayName,
          language: info.language,
        }
        const changed = d.notebook.metadata.kernelspec?.name !== kernelspec.name
        return {
          ...d,
          sessionId: info.id,
          dirty: d.dirty || changed,
          notebook: {
            ...d.notebook,
            metadata: {
              ...d.notebook.metadata,
              kernelspec,
              language_info: d.notebook.metadata.language_info ?? { name: spec?.language ?? info.language },
            },
          },
        }
      })

      return info.id
    },

    async runCell(path, cellId, advance = false) {
      const doc = get().docs[path]
      if (!doc) return

      const targetId = cellId ?? doc.selectedCellId
      const cell = doc.notebook.cells.find((c) => c.id === targetId)
      if (!cell || !targetId) return

      // Markdown "runs" by rendering.
      if (cell.cell_type !== 'code') {
        get().setEditing(path, targetId, false)
        if (advance) advanceSelection(path, targetId)
        return
      }

      if (cell.source.trim() === '') {
        if (advance) advanceSelection(path, targetId)
        return
      }

      let sessionId: string
      try {
        sessionId = await get().ensureSession(path)
      } catch (error) {
        get().notify((error as Error).message, 'error')
        return
      }

      // Clear previous outputs up front so the cell visibly resets.
      patchCell(path, targetId, (c) => (isCodeCell(c) ? { ...c, outputs: [], execution_count: null } : c))
      setRun(path, targetId, { queued: true, running: true, startedAt: Date.now(), elapsedMs: null, errored: false })

      try {
        const msgId = await window.herse.kernels.execute(sessionId, cell.source)
        patchDoc(path, (d) => ({ ...d, msgToCell: { ...d.msgToCell, [msgId]: targetId } }))

        // Replay anything that arrived while we were waiting for the msg_id.
        const buffered = orphaned.get(msgId)
        if (buffered) {
          orphaned.delete(msgId)
          for (const entry of buffered) {
            get().handleKernelMessage(entry.sessionId, entry.channel, entry.message)
          }
        }
      } catch (error) {
        setRun(path, targetId, { queued: false, running: false, errored: true })
        get().notify(`Execution failed: ${(error as Error).message}`, 'error')
        return
      }

      if (advance) advanceSelection(path, targetId)
    },

    async runAll(path) {
      const doc = get().docs[path]
      if (!doc) return
      // The kernel serializes execute_requests in arrival order, so firing them
      // all is both correct and cheaper than orchestrating a queue ourselves.
      for (const cell of doc.notebook.cells) {
        if (cell.cell_type === 'code') await get().runCell(path, cell.id)
      }
    },

    async interrupt(path) {
      const sessionId = get().docs[path]?.sessionId
      if (sessionId) await window.herse.kernels.interrupt(sessionId).catch(() => {})
    },

    async restartKernel(path, runAll = false) {
      const doc = get().docs[path]
      if (!doc?.sessionId) return

      try {
        const info = await window.herse.kernels.restart(doc.sessionId)
        set((state) => ({ sessions: { ...state.sessions, [info.id]: info } }))
        patchDoc(path, (d) => ({ ...d, runs: {}, msgToCell: {}, pendingInput: null }))
        get().notify('Kernel restarted')
        if (runAll) await get().runAll(path)
      } catch (error) {
        get().notify(`Restart failed: ${(error as Error).message}`, 'error')
      }
    },

    async shutdownKernel(path) {
      const doc = get().docs[path]
      if (!doc?.sessionId) return
      await window.herse.kernels.shutdown(doc.sessionId).catch(() => {})
      set((state) => {
        const sessions = { ...state.sessions }
        delete sessions[doc.sessionId!]
        return { sessions }
      })
      patchDoc(path, (d) => ({ ...d, sessionId: null, runs: {}, pendingInput: null }))
    },

    async submitInput(path, value) {
      const doc = get().docs[path]
      if (!doc?.sessionId) return
      patchDoc(path, (d) => ({ ...d, pendingInput: null }))
      await window.herse.kernels.sendInput(doc.sessionId, value)
    },

    /* -------------------------------- events ------------------------------ */

    handleKernelMessage(sessionId, channel, message) {
      const msg = message
      const parentId = msg.parent_header?.msg_id
      if (!parentId) return

      const state = get()
      const path = Object.keys(state.docs).find(
        (p) => state.docs[p].sessionId === sessionId && state.docs[p].msgToCell[parentId],
      )

      if (!path) {
        // Unknown parent: either output that outran its msg_id, or traffic we
        // genuinely don't care about. Park it — `runCell` replays on match.
        if (msg.header.msg_type !== 'status') park(parentId, { sessionId, channel, message })
        return
      }

      const doc = state.docs[path]
      const cellId = doc.msgToCell[parentId]

      // stdin is a question, not an output.
      if (channel === 'stdin' && msg.header.msg_type === 'input_request') {
        patchDoc(path, (d) => ({
          ...d,
          pendingInput: {
            cellId,
            prompt: String(msg.content.prompt ?? ''),
            password: Boolean(msg.content.password),
          },
        }))
        return
      }

      const cell = doc.notebook.cells.find((c) => c.id === cellId)
      if (!cell || !isCodeCell(cell)) return

      const result = applyMessage(cell.outputs, message)
      if (!result) return

      patchCell(
        path,
        cellId,
        (c) =>
          isCodeCell(c)
            ? {
                ...c,
                outputs: result.outputs,
                execution_count:
                  result.executionCount !== undefined ? result.executionCount : c.execution_count,
              }
            : c,
        // Output changes make the file dirty; that's real unsaved state.
        { dirty: true },
      )

      if (result.executionCount !== undefined) setRun(path, cellId, { queued: false })
      if (result.errored) setRun(path, cellId, { errored: true })

      if (result.finished) {
        const startedAt = get().docs[path]?.runs[cellId]?.startedAt ?? null
        setRun(path, cellId, {
          queued: false,
          running: false,
          elapsedMs: startedAt ? Date.now() - startedAt : null,
        })
        patchDoc(path, (d) => {
          const msgToCell = { ...d.msgToCell }
          delete msgToCell[parentId]
          return { ...d, msgToCell, pendingInput: d.pendingInput?.cellId === cellId ? null : d.pendingInput }
        })
      }
    },

    handleKernelStatus(sessionId, status, reason) {
      set((state) => {
        const session = state.sessions[sessionId]
        if (!session) return state
        return { sessions: { ...state.sessions, [sessionId]: { ...session, status } } }
      })

      if (status !== 'dead') return

      // A dead kernel leaves every in-flight cell stuck spinning.
      for (const [path, doc] of Object.entries(get().docs)) {
        if (doc.sessionId !== sessionId) continue
        patchDoc(path, (d) => ({
          ...d,
          runs: Object.fromEntries(
            Object.entries(d.runs).map(([id, run]) => [id, { ...run, queued: false, running: false }]),
          ),
          msgToCell: {},
          pendingInput: null,
        }))
      }
      if (reason) get().notify(reason, 'error')
    },

    /* --------------------------------- ui --------------------------------- */

    setPalette(mode) {
      set({ palette: mode })
    },

    toggleSidebar() {
      set((state) => ({ sidebarVisible: !state.sidebarVisible }))
    },

    notify(message, tone = 'info') {
      set({ toast: { message, tone } })
      setTimeout(() => {
        // Only clear if this is still the same toast.
        if (get().toast?.message === message) set({ toast: null })
      }, 6000)
    },
  }

  /** Move selection to the next cell, appending one if we're at the end. */
  function advanceSelection(path: string, fromCellId: string): void {
    patchDoc(path, (doc) => {
      const index = cellIndex(doc, fromCellId)
      const next = doc.notebook.cells[index + 1]
      if (next) return { ...doc, selectedCellId: next.id, revealCellId: next.id }

      const appended = newCell('code')
      return {
        ...doc,
        dirty: true,
        notebook: { ...doc.notebook, cells: [...doc.notebook.cells, appended] },
        selectedCellId: appended.id,
        revealCellId: appended.id,
      }
    })
  }
})

/* -------------------------------------------------------------------------- */
/* Autosave                                                                   */
/* -------------------------------------------------------------------------- */

function cancelAutosave(path: string): void {
  const timer = autosaveTimers.get(path)
  if (timer) {
    clearTimeout(timer)
    autosaveTimers.delete(path)
  }
}

function scheduleAutosave(path: string): void {
  cancelAutosave(path)
  const timer = setTimeout(() => {
    autosaveTimers.delete(path)
    void useStore.getState().autosave(path)
  }, AUTOSAVE_DELAY_MS)
  autosaveTimers.set(path, timer)
}

/** Write pending edits immediately — on tab close and on window blur. */
export function flushAutosave(path?: string): void {
  const paths = path ? [path] : [...autosaveTimers.keys()]
  for (const p of paths) {
    cancelAutosave(p)
    void useStore.getState().autosave(p)
  }
}

/**
 * Watches for content changes and debounces a write. Keyed on notebook object
 * identity, so selection moves and run bookkeeping don't trigger saves —
 * only actual edits and new outputs do.
 */
useStore.subscribe((state, previous) => {
  for (const [path, doc] of Object.entries(state.docs)) {
    if (doc.untitled || !doc.dirty || autosaveBlocked.has(path)) continue
    const before = previous.docs[path]
    if (before && before.notebook === doc.notebook) continue
    scheduleAutosave(path)
  }

  // Drop timers belonging to documents that have since been closed.
  for (const path of [...autosaveTimers.keys()]) {
    if (!state.docs[path]) cancelAutosave(path)
  }
})

/* Convenience selectors used across components. */
export const selectActiveDoc = (state: HerseState): Doc | null =>
  state.activePath ? (state.docs[state.activePath] ?? null) : null

export const selectActiveSession = (state: HerseState): SessionInfo | null => {
  const doc = selectActiveDoc(state)
  return doc?.sessionId ? (state.sessions[doc.sessionId] ?? null) : null
}

export type { HerseState }
export { baseName, dirName }
