import { baseName, useStore } from '../state/store'
import { FileTree } from './FileTree'

export function Sidebar() {
  const workspace = useStore((s) => s.workspace)
  const openFolder = useStore((s) => s.openFolder)
  const setRoot = useStore((s) => s.setRoot)
  const openNotebook = useStore((s) => s.openNotebook)
  const newNotebook = useStore((s) => s.newNotebook)

  if (!workspace.root) {
    return (
      <aside className="sidebar">
        <div className="panel panel-empty">
          <p>No folder open.</p>
          <button className="primary" onClick={() => void openFolder()}>
            Open Folder…
          </button>

          {workspace.recentFolders.length > 0 && (
            <>
              <div className="panel-label">Recent folders</div>
              {workspace.recentFolders.map((folder) => (
                <button key={folder} className="link-row" title={folder} onClick={() => void setRoot(folder)}>
                  {baseName(folder)}
                </button>
              ))}
            </>
          )}
        </div>
      </aside>
    )
  }

  return (
    <aside className="sidebar">
      <div className="panel">
        <div className="panel-header">
          <span className="panel-root" title={workspace.root}>
            {baseName(workspace.root)}
          </span>
          <div className="panel-actions">
            <button title="New notebook" onClick={() => void newNotebook()}>
              ＋
            </button>
            <button title="Open another folder" onClick={() => void openFolder()}>
              ⤢
            </button>
          </div>
        </div>

        <FileTree root={workspace.root} />

        {workspace.recentNotebooks.length > 0 && (
          <>
            <div className="panel-label">Recent</div>
            {workspace.recentNotebooks.slice(0, 6).map((path) => (
              <button key={path} className="link-row" title={path} onClick={() => void openNotebook(path)}>
                {baseName(path).replace(/\.ipynb$/i, '')}
              </button>
            ))}
          </>
        )}
      </div>
    </aside>
  )
}
