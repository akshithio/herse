import { useEffect, useMemo, useRef, useState } from 'react'
import type { DirEntry } from '@shared/api'
import { baseName, useStore } from '../state/store'
import { type KeyName, Keys } from './Keys'

interface Command {
  id: string
  label: string
  keys?: KeyName[]
  run(): void
}

function useCommands(): Command[] {
  const store = useStore
  const activePath = useStore((s) => s.activePath)

  return useMemo(() => {
    const s = () => store.getState()
    const withDoc = (fn: (path: string) => void) => () => {
      const path = s().activePath
      if (path) fn(path)
    }

    return [
      { id: 'notebook.new', label: 'New Notebook', keys: ['Mod', 'N'], run: () => void s().newNotebook() },
      {
        id: 'notebook.open',
        label: 'Open Notebook…',
        keys: ['Mod', 'O'],
        run: () => void s().openNotebookDialog(),
      },
      {
        id: 'workspace.openFolder',
        label: 'Open Folder…',
        keys: ['Shift', 'Mod', 'O'],
        run: () => void s().openFolder(),
      },
      { id: 'notebook.save', label: 'Save Notebook', keys: ['Mod', 'S'], run: () => void s().save() },
      { id: 'notebook.saveAs', label: 'Save Notebook As…', run: () => void s().saveAs() },
      { id: 'cell.runAll', label: 'Run All Cells', run: withDoc((p) => void s().runAll(p)) },
      {
        id: 'kernel.restartRunAll',
        label: 'Restart Kernel and Run All',
        run: withDoc((p) => void s().restartKernel(p, true)),
      },
      { id: 'kernel.restart', label: 'Restart Kernel', run: withDoc((p) => void s().restartKernel(p)) },
      {
        id: 'kernel.interrupt',
        label: 'Interrupt Kernel',
        keys: ['Mod', '.'],
        run: withDoc((p) => void s().interrupt(p)),
      },
      { id: 'kernel.shutdown', label: 'Shut Down Kernel', run: withDoc((p) => void s().shutdownKernel(p)) },
      {
        id: 'notebook.clearAllOutputs',
        label: 'Clear All Outputs',
        run: withDoc((p) => s().clearOutputs(p)),
      },
      { id: 'cell.toCode', label: 'Change Cell to Code', run: withDoc((p) => s().changeCellType(p, 'code')) },
      {
        id: 'cell.toMarkdown',
        label: 'Change Cell to Markdown',
        run: withDoc((p) => s().changeCellType(p, 'markdown')),
      },
      { id: 'view.toggleSidebar', label: 'Toggle Sidebar', keys: ['Mod', 'B'], run: () => s().toggleSidebar() },
      { id: 'tab.close', label: 'Close Tab', keys: ['Mod', 'W'], run: withDoc((p) => s().closeDoc(p)) },
    ]
  }, [store, activePath])
}

interface Row {
  key: string
  label: string
  /** File rows show a path; command rows show their shortcut. */
  path?: string
  keys?: KeyName[]
  run(): void
}

export function CommandPalette() {
  const mode = useStore((s) => s.palette)
  const setPalette = useStore((s) => s.setPalette)
  const openNotebook = useStore((s) => s.openNotebook)
  const workspaceRoot = useStore((s) => s.workspace.root)
  const commands = useCommands()

  const [query, setQuery] = useState('')
  const [files, setFiles] = useState<DirEntry[]>([])
  const [cursor, setCursor] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)

  const isFileMode = mode === 'files'

  useEffect(() => {
    setQuery('')
    setCursor(0)
    setFiles([])
    if (mode) setTimeout(() => inputRef.current?.focus(), 0)
  }, [mode])

  // File mode delegates matching to the main process, which walks the tree.
  useEffect(() => {
    if (!isFileMode || !workspaceRoot) return
    let cancelled = false
    const timer = setTimeout(() => {
      window.herse.workspace
        .findFiles(query, 40)
        .then((result) => !cancelled && setFiles(result))
        .catch(() => !cancelled && setFiles([]))
    }, 120)

    return () => {
      cancelled = true
      clearTimeout(timer)
    }
  }, [query, isFileMode, workspaceRoot])

  const filteredCommands = useMemo(() => {
    const needle = query.trim().toLowerCase()
    if (!needle) return commands
    return commands.filter((command) => command.label.toLowerCase().includes(needle))
  }, [commands, query])

  if (!mode) return null

  const rows: Row[] = isFileMode
    ? files.map((file) => ({
        key: file.path,
        label: baseName(file.path),
        path: file.path,
        run: () => void openNotebook(file.path),
      }))
    : filteredCommands.map((command) => ({
        key: command.id,
        label: command.label,
        keys: command.keys,
        run: command.run,
      }))

  const commit = (index: number) => {
    const row = rows[index]
    if (!row) return
    setPalette(null)
    row.run()
  }

  return (
    <>
      <div className="popover-backdrop" onClick={() => setPalette(null)} />
      <div className="palette">
        <input
          ref={inputRef}
          className="palette-input"
          placeholder={isFileMode ? 'Go to file…' : 'Type a command…'}
          value={query}
          onChange={(event) => {
            setQuery(event.target.value)
            setCursor(0)
          }}
          onKeyDown={(event) => {
            if (event.key === 'Escape') setPalette(null)
            else if (event.key === 'ArrowDown') setCursor((c) => Math.min(c + 1, rows.length - 1))
            else if (event.key === 'ArrowUp') setCursor((c) => Math.max(c - 1, 0))
            else if (event.key === 'Enter') commit(cursor)
            else return
            event.preventDefault()
          }}
        />

        <div className="palette-rows">
          {rows.length === 0 && <div className="palette-empty">Nothing matches.</div>}
          {rows.map((row, index) => (
            <button
              key={row.key}
              className={`palette-row${index === cursor ? ' palette-row-active' : ''}`}
              onMouseEnter={() => setCursor(index)}
              onClick={() => commit(index)}
            >
              <span className="palette-label">{row.label}</span>
              {row.keys && <Keys keys={row.keys} />}
              {row.path && (
                <span className="palette-path" title={row.path}>
                  {/* rtl truncates from the left; bdi keeps the path reading ltr. */}
                  <bdi>{row.path}</bdi>
                </span>
              )}
            </button>
          ))}
        </div>
      </div>
    </>
  )
}
