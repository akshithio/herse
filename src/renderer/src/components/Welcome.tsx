import { baseName, useStore } from '../state/store'
import { type KeyName, Keys } from './Keys'

const SHORTCUTS: Array<[KeyName[], string]> = [
  [['Mod', 'P'], 'Go to file'],
  [['Shift', 'Mod', 'P'], 'Command palette'],
  [['Mod', 'Enter'], 'Run cell'],
  [['Shift', 'Enter'], 'Run cell, select next'],
  [['Alt', 'Enter'], 'Run cell, insert below'],
  [['Shift', 'Mod', 'B'], 'Insert code cell below'],
  [['Shift', 'Mod', 'A'], 'Insert code cell above'],
  [['Shift', 'Mod', 'M'], 'Insert markdown cell below'],
  [['Escape'], 'Leave the editor'],
  [['A'], 'Insert cell above'],
  [['B'], 'Insert cell below'],
  [['M'], 'Change to markdown'],
  [['Y'], 'Change to code'],
  [['D', 'D'], 'Delete cell'],
]

export function Welcome() {
  const workspace = useStore((s) => s.workspace)
  const specs = useStore((s) => s.specs)
  const openFolder = useStore((s) => s.openFolder)
  const openNotebook = useStore((s) => s.openNotebook)
  const openNotebookDialog = useStore((s) => s.openNotebookDialog)
  const newNotebook = useStore((s) => s.newNotebook)

  return (
    <div className="welcome">
      <div className="welcome-inner">
        <h1>Herse</h1>
        <p className="welcome-tagline">Notebooks, on your desktop. No server, no browser tab.</p>

        <div className="welcome-actions">
          <button className="primary" onClick={() => void openFolder()}>
            Open Folder…
          </button>
          <button onClick={() => void openNotebookDialog()}>Open Notebook…</button>
          <button onClick={() => void newNotebook()}>New Notebook</button>
        </div>

        {workspace.recentNotebooks.length > 0 && (
          <div className="welcome-section">
            <h2>Recent notebooks</h2>
            {workspace.recentNotebooks.slice(0, 8).map((path) => (
              <button key={path} className="link-row" title={path} onClick={() => void openNotebook(path)}>
                <span>{baseName(path).replace(/\.ipynb$/i, '')}</span>
                <span className="link-row-hint">{path}</span>
              </button>
            ))}
          </div>
        )}

        <div className="welcome-section">
          <h2>Kernels</h2>
          {specs.length === 0 ? (
            <p className="welcome-warning">
              No Jupyter kernels found. Install one with:
              <code>python3 -m pip install ipykernel</code>
              <code>python3 -m ipykernel install --user</code>
            </p>
          ) : (
            <ul className="welcome-kernels">
              {specs.map((spec) => (
                <li key={spec.name}>
                  <span>{spec.display_name}</span>
                  <span className="link-row-hint">{spec.language}</span>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="welcome-section welcome-keys">
          <h2>Shortcuts</h2>
          <dl>
            {SHORTCUTS.map(([keys, description]) => (
              <div key={description}>
                <dt>
                  <Keys keys={keys} />
                </dt>
                <dd>{description}</dd>
              </div>
            ))}
          </dl>
          <p className="welcome-note">
            The single-key commands apply after Escape, when focus is on the notebook rather than
            inside an editor.
          </p>
        </div>
      </div>
    </div>
  )
}
