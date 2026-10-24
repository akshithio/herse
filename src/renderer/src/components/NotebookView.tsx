import { useEffect, useRef } from 'react'
import type { Doc } from '../state/store'
import { useStore } from '../state/store'
import { Cell } from './Cell'

interface NotebookViewProps {
  doc: Doc
}

/**
 * Command-mode keyboard handling, Jupyter-style: when focus is on the notebook
 * surface rather than inside an editor, single keys act on the selected cell.
 *
 * State is read from the store inside the handler rather than closed over, so
 * the listener is attached once per document instead of on every keystroke.
 */
function useCommandMode(path: string, containerRef: React.RefObject<HTMLDivElement | null>) {
  useEffect(() => {
    const element = containerRef.current
    if (!element) return

    const onKeyDown = (event: KeyboardEvent) => {
      // Never steal keys from a focused editor or input.
      const target = event.target as HTMLElement | null
      if (target?.closest('.cm-editor') || target?.closest('input, textarea')) return
      if (event.metaKey || event.ctrlKey || event.altKey) return

      const state = useStore.getState()
      const doc = state.docs[path]
      if (!doc) return

      const cells = doc.notebook.cells
      const index = cells.findIndex((c) => c.id === doc.selectedCellId)

      const select = (i: number) => {
        const cell = cells[Math.max(0, Math.min(i, cells.length - 1))]
        if (cell) state.selectCell(path, cell.id)
      }

      switch (event.key) {
        case 'j':
        case 'ArrowDown':
          select(index + 1)
          break
        case 'k':
        case 'ArrowUp':
          select(index - 1)
          break
        case 'a':
          state.insertCell(path, 'above')
          break
        case 'b':
          state.insertCell(path, 'below')
          break
        case 'm':
          state.changeCellType(path, 'markdown')
          break
        case 'y':
          state.changeCellType(path, 'code')
          break
        case 'r':
          state.changeCellType(path, 'raw')
          break
        case 'Enter':
          if (doc.selectedCellId) state.setEditing(path, doc.selectedCellId, true)
          break
        case 'd':
          // `dd` deletes, matching Jupyter (and vim muscle memory).
          if (element.dataset.lastKey === 'd') {
            state.deleteCell(path)
            element.dataset.lastKey = ''
            break
          }
          element.dataset.lastKey = 'd'
          setTimeout(() => {
            if (element.dataset.lastKey === 'd') element.dataset.lastKey = ''
          }, 600)
          return
        default:
          return
      }

      if (event.key !== 'd') element.dataset.lastKey = ''
      event.preventDefault()
    }

    element.addEventListener('keydown', onKeyDown)
    return () => element.removeEventListener('keydown', onKeyDown)
  }, [path, containerRef])
}

export function NotebookView({ doc }: NotebookViewProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const insertCell = useStore((s) => s.insertCell)
  const language =
    doc.notebook.metadata.language_info?.name ?? doc.notebook.metadata.kernelspec?.language ?? 'python'

  useCommandMode(doc.path, containerRef)

  return (
    <div className="notebook-scroll" ref={containerRef} tabIndex={-1}>
      <div className="notebook">
        {doc.notebook.cells.map((cell) => {
          const run = doc.runs[cell.id]
          return (
            <Cell
              key={cell.id}
              path={doc.path}
              cell={cell}
              language={language}
              selected={doc.selectedCellId === cell.id}
              running={run?.running ?? false}
              errored={run?.errored ?? false}
              elapsedMs={run?.elapsedMs ?? null}
              editing={Boolean(doc.editing[cell.id])}
              pendingInput={doc.pendingInput?.cellId === cell.id ? doc.pendingInput : null}
              reveal={doc.revealCellId === cell.id}
            />
          )
        })}

        <div className="cell-adders">
          <button onClick={() => insertCell(doc.path, 'below', 'code')}>+ Code</button>
          <button onClick={() => insertCell(doc.path, 'below', 'markdown')}>+ Markdown</button>
        </div>
      </div>
    </div>
  )
}
