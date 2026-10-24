import { useEffect } from 'react'
import { flushAutosave, selectActiveDoc, useStore } from './state/store'
import { CommandPalette } from './components/CommandPalette'
import { NotebookView } from './components/NotebookView'
import { Sidebar } from './components/Sidebar'
import { StatusBar } from './components/StatusBar'
import { TabBar } from './components/TabBar'
import { Welcome } from './components/Welcome'

/** Menu commands the renderer executes. Keys match `src/main/menu.ts`. */
function runMenuCommand(command: string): void {
  const state = useStore.getState()
  const path = state.activePath

  switch (command) {
    case 'notebook.new':
      void state.newNotebook()
      return
    case 'notebook.open':
      void state.openNotebookDialog()
      return
    case 'workspace.openFolder':
      void state.openFolder()
      return
    case 'notebook.save':
      void state.save()
      return
    case 'notebook.saveAs':
      void state.saveAs()
      return
    case 'workspace.quickOpen':
      state.setPalette('files')
      return
    case 'app.commandPalette':
      state.setPalette('commands')
      return
    case 'view.toggleSidebar':
      state.toggleSidebar()
      return
  }

  // Everything below needs an open notebook.
  if (!path) return

  switch (command) {
    case 'tab.close':
      state.closeDoc(path)
      break
    case 'cell.run':
      void state.runCell(path)
      break
    case 'cell.runAndAdvance':
      void state.runCell(path, undefined, true)
      break
    case 'cell.runAll':
      void state.runAll(path)
      break
    case 'cell.insertAbove':
      state.insertCell(path, 'above')
      break
    case 'cell.insertBelow':
      state.insertCell(path, 'below')
      break
    case 'cell.insertMarkdownBelow':
      state.insertCell(path, 'below', 'markdown')
      break
    case 'cell.runAndInsertBelow':
      // Run first, then add a fresh cell below the one that just ran.
      void state.runCell(path).then(() => useStore.getState().insertCell(path, 'below'))
      break
    case 'cell.delete':
      state.deleteCell(path)
      break
    case 'cell.toCode':
      state.changeCellType(path, 'code')
      break
    case 'cell.toMarkdown':
      state.changeCellType(path, 'markdown')
      break
    case 'cell.clearOutputs':
      state.clearOutputs(path, state.docs[path]?.selectedCellId ?? undefined)
      break
    case 'notebook.clearAllOutputs':
      state.clearOutputs(path)
      break
    case 'kernel.interrupt':
      void state.interrupt(path)
      break
    case 'kernel.restart':
      void state.restartKernel(path)
      break
    case 'kernel.restartRunAll':
      void state.restartKernel(path, true)
      break
    case 'kernel.change':
      state.setPalette('commands')
      break
    case 'kernel.shutdown':
      void state.shutdownKernel(path)
      break
  }
}

export function App() {
  const init = useStore((s) => s.init)
  const doc = useStore(selectActiveDoc)
  const sidebarVisible = useStore((s) => s.sidebarVisible)
  const toast = useStore((s) => s.toast)
  const setPalette = useStore((s) => s.setPalette)

  useEffect(() => {
    void init()
  }, [init])

  // Kernel traffic and menu commands both arrive over IPC.
  useEffect(() => {
    const store = useStore.getState()
    const offMessage = window.herse.events.onMessage(({ sessionId, channel, message }) =>
      store.handleKernelMessage(sessionId, channel, message),
    )
    const offStatus = window.herse.events.onStatus(({ sessionId, status, reason }) =>
      store.handleKernelStatus(sessionId, status, reason),
    )
    const offMenu = window.herse.events.onMenuCommand(runMenuCommand)

    return () => {
      offMessage()
      offStatus()
      offMenu()
    }
  }, [])

  // Escape closes any overlay, from anywhere.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setPalette(null)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [setPalette])

  // Leaving the window is a natural save point — don't sit on pending edits.
  useEffect(() => {
    const onBlur = () => flushAutosave()
    window.addEventListener('blur', onBlur)
    return () => window.removeEventListener('blur', onBlur)
  }, [])

  return (
    <div className="app">
      <div className="titlebar">
        <span className="titlebar-title">{doc ? doc.name.replace(/\.ipynb$/i, '') : 'Herse'}</span>
        {doc?.dirty && <span className="titlebar-dirty" title="Unsaved changes" />}
      </div>

      <div className="body">
        {sidebarVisible && <Sidebar />}

        <main className="main">
          <TabBar />
          {doc ? <NotebookView key={doc.path} doc={doc} /> : <Welcome />}
          <StatusBar />
        </main>
      </div>

      <CommandPalette />

      {toast && <div className={`toast toast-${toast.tone}`}>{toast.message}</div>}
    </div>
  )
}
