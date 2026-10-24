import { useCallback, useEffect, useState } from 'react'
import type { DirEntry } from '@shared/api'
import { useStore } from '../state/store'

/** Lazy-loaded directory node. Children are fetched on first expand. */
function TreeNode({ entry, depth }: { entry: DirEntry; depth: number }) {
  const [expanded, setExpanded] = useState(false)
  const [children, setChildren] = useState<DirEntry[] | null>(null)
  const [loading, setLoading] = useState(false)

  const openNotebook = useStore((s) => s.openNotebook)
  const activePath = useStore((s) => s.activePath)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      setChildren(await window.herse.workspace.list(entry.path))
    } catch {
      setChildren([])
    } finally {
      setLoading(false)
    }
  }, [entry.path])

  const toggle = () => {
    const next = !expanded
    setExpanded(next)
    if (next && children === null) void load()
  }

  if (entry.isDirectory) {
    return (
      <>
        <button className="tree-row" style={{ paddingLeft: 8 + depth * 12 }} onClick={toggle}>
          <span className={`tree-caret${expanded ? ' tree-caret-open' : ''}`}>›</span>
          <span className="tree-name">{entry.name}</span>
        </button>
        {expanded &&
          (loading ? (
            <div className="tree-hint" style={{ paddingLeft: 20 + depth * 12 }}>
              loading…
            </div>
          ) : (
            children?.map((child) => <TreeNode key={child.path} entry={child} depth={depth + 1} />)
          ))}
      </>
    )
  }

  const isNotebook = entry.name.toLowerCase().endsWith('.ipynb')

  return (
    <button
      className={`tree-row tree-file${activePath === entry.path ? ' tree-row-active' : ''}${
        isNotebook ? '' : ' tree-row-muted'
      }`}
      style={{ paddingLeft: 20 + depth * 12 }}
      disabled={!isNotebook}
      title={entry.path}
      onClick={() => isNotebook && void openNotebook(entry.path)}
      onContextMenu={(event) => {
        event.preventDefault()
        void window.herse.workspace.revealInOS(entry.path)
      }}
    >
      <span className="tree-name">{entry.name.replace(/\.ipynb$/i, '')}</span>
      {entry.notebook && (
        <span className="tree-meta">
          {entry.notebook.cellCount} cells
          {entry.notebook.kernelDisplayName ? ` · ${entry.notebook.kernelDisplayName}` : ''}
        </span>
      )}
    </button>
  )
}

export function FileTree({ root }: { root: string }) {
  const [entries, setEntries] = useState<DirEntry[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    setEntries(null)
    setError(null)

    window.herse.workspace
      .list(root)
      .then((result) => !cancelled && setEntries(result))
      .catch((e: Error) => !cancelled && setError(e.message))

    return () => {
      cancelled = true
    }
  }, [root])

  if (error) return <div className="tree-hint tree-error">{error}</div>
  if (entries === null) return <div className="tree-hint">Reading folder…</div>
  if (entries.length === 0) return <div className="tree-hint">This folder is empty.</div>

  return (
    <div className="tree">
      {entries.map((entry) => (
        <TreeNode key={entry.path} entry={entry} depth={0} />
      ))}
    </div>
  )
}
