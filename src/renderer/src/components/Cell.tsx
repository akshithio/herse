import { memo, useCallback, useEffect, useRef } from 'react'
import type { Cell as CellModel } from '@shared/nbformat'
import { isCodeCell } from '@shared/nbformat'
import { type CellRun, type PendingInput, useStore } from '../state/store'
import { Editor } from './Editor'
import { Markdown } from './Markdown'
import { OutputList } from './Output'

/**
 * Props are primitives and stable object references rather than the whole
 * document, so `memo` can skip every cell except the one being edited. Passing
 * `doc` here would re-render the entire notebook on each keystroke.
 */
interface CellProps {
  path: string
  cell: CellModel
  language: string
  selected: boolean
  running: boolean
  errored: boolean
  elapsedMs: number | null
  editing: boolean
  pendingInput: PendingInput | null
  reveal: boolean
}

/** `[ ]`, `[*]`, or `[12]` — the prompt marker, same idiom as Jupyter. */
function ExecutionPrompt({ cell, running }: { cell: CellModel; running: boolean }) {
  if (!isCodeCell(cell)) return null
  const text = running ? '[*]' : cell.execution_count === null ? '[ ]' : `[${cell.execution_count}]`
  return <div className={`prompt${running ? ' prompt-running' : ''}`}>{text}</div>
}

function formatElapsed(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`
  const minutes = Math.floor(ms / 60_000)
  return `${minutes}m ${Math.round((ms % 60_000) / 1000)}s`
}

export const Cell = memo(function Cell({
  path,
  cell,
  language,
  selected,
  running,
  errored,
  elapsedMs,
  editing,
  pendingInput,
  reveal,
}: CellProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLInputElement>(null)

  const selectCell = useStore((s) => s.selectCell)
  const setEditing = useStore((s) => s.setEditing)
  const updateSource = useStore((s) => s.updateSource)
  const runCell = useStore((s) => s.runCell)
  const submitInput = useStore((s) => s.submitInput)

  const cellId = cell.id

  // Store actions are stable, so these keep a stable identity too — which is
  // what keeps CodeMirror from reconfiguring on every render.
  const handleChange = useCallback(
    (value: string) => updateSource(path, cellId, value),
    [updateSource, path, cellId],
  )
  const handleRun = useCallback(() => void runCell(path, cellId), [runCell, path, cellId])
  const handleRunAndAdvance = useCallback(() => void runCell(path, cellId, true), [runCell, path, cellId])
  const handleFocus = useCallback(() => selectCell(path, cellId), [selectCell, path, cellId])
  /**
   * Escape leaves the editor and hands focus to the notebook surface, which is
   * what makes the single-key command shortcuts (a, b, m, y, dd) reachable —
   * the standard Jupyter edit/command mode split.
   */
  const handleEscape = useCallback(() => {
    setEditing(path, cellId, false)
    containerRef.current?.closest<HTMLElement>('.notebook-scroll')?.focus()
  }, [setEditing, path, cellId])

  const showEditor = cell.cell_type === 'code' || editing || cell.source.trim() === ''

  // Scroll newly-selected / jumped-to cells into view.
  useEffect(() => {
    if (!reveal) return
    containerRef.current?.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }, [reveal])

  useEffect(() => {
    if (pendingInput) inputRef.current?.focus()
  }, [pendingInput])

  return (
    <div
      ref={containerRef}
      className={[
        'cell',
        `cell-${cell.cell_type}`,
        selected ? 'cell-selected' : '',
        running ? 'cell-running' : '',
        errored ? 'cell-errored' : '',
      ]
        .filter(Boolean)
        .join(' ')}
      onClick={() => !selected && selectCell(path, cellId)}
    >
      {/* Sits in the page margin so the editor itself runs edge to edge. */}
      <div className="cell-marker">
        <ExecutionPrompt cell={cell} running={running} />
        {isCodeCell(cell) && (
          <button
            className="cell-run"
            title="Run cell"
            onClick={(event) => {
              event.stopPropagation()
              handleRun()
            }}
          >
            ▶
          </button>
        )}
      </div>

      {showEditor ? (
        <div className="cell-input">
          <Editor
            value={cell.source}
            cellType={cell.cell_type}
            language={language}
            autoFocus={editing}
            onChange={handleChange}
            onFocus={handleFocus}
            onRun={handleRun}
            onRunAndAdvance={handleRunAndAdvance}
            onEscape={handleEscape}
          />
        </div>
      ) : (
        <div
          className="cell-rendered"
          onDoubleClick={() => setEditing(path, cellId, true)}
          title="Double-click to edit"
        >
          {cell.cell_type === 'markdown' ? (
            <Markdown source={cell.source} />
          ) : (
            <pre className="raw-cell">{cell.source}</pre>
          )}
        </div>
      )}

      {pendingInput && (
        <form
          className="stdin-prompt"
          onSubmit={(event) => {
            event.preventDefault()
            const value = inputRef.current?.value ?? ''
            if (inputRef.current) inputRef.current.value = ''
            void submitInput(path, value)
          }}
        >
          <label>{pendingInput.prompt || 'Input:'}</label>
          <input ref={inputRef} type={pendingInput.password ? 'password' : 'text'} autoComplete="off" />
          <button type="submit">Send</button>
        </form>
      )}

      {isCodeCell(cell) && <OutputList outputs={cell.outputs} />}

      {elapsedMs != null && !running && <div className="cell-timing">{formatElapsed(elapsedMs)}</div>}
    </div>
  )
})

export type { CellRun }
