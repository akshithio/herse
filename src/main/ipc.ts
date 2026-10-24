/**
 * IPC handler registration. Channel names mirror the `HerseApi` shape in
 * `src/shared/api.ts`; the preload is a thin, mechanical wrapper over these.
 */

import { BrowserWindow, app, dialog, ipcMain, shell } from 'electron'
import { stat } from 'node:fs/promises'
import type { DirEntry, OpenedNotebook, SaveResult, SearchHit, WorkspaceState } from '@shared/api'
import type { CompleteReplyContent, InspectReplyContent, KernelSpec, SessionInfo } from '@shared/jupyter'
import type { Notebook } from '@shared/nbformat'

import { kernelManager } from './kernel/manager'
import { discoverKernelSpecs } from './kernel/kernelspecs'
import { readNotebook, writeNotebook } from './notebook/io'
import { assertWithin, findFiles, listDirectory, searchNotebooks } from './workspace/fs'
import { getWorkspaceState, recordRecentNotebook, setWorkspaceRoot } from './workspace/store'

/** Resolve the workspace root, failing loudly if nothing is open yet. */
async function requireRoot(): Promise<string> {
  const { root } = await getWorkspaceState()
  if (!root) throw new Error('No workspace folder is open')
  return root
}

function focusedWindow(): BrowserWindow | undefined {
  return BrowserWindow.getFocusedWindow() ?? BrowserWindow.getAllWindows()[0]
}

/**
 * Where to launch a kernel. Untitled notebooks have no directory of their own,
 * so the renderer may pass nothing — never spawn a kernel in whatever directory
 * the app happened to be started from.
 */
async function resolveKernelCwd(cwd: string): Promise<string> {
  if (cwd) {
    try {
      if ((await stat(cwd)).isDirectory()) return cwd
    } catch {
      // Missing or unreadable; fall through to home.
    }
  }
  return app.getPath('home')
}

export function registerIpcHandlers(): void {
  /* -------------------------------- workspace ------------------------------- */

  ipcMain.handle('workspace:getState', async (): Promise<WorkspaceState> => getWorkspaceState())

  ipcMain.handle('workspace:pickFolder', async (): Promise<string | null> => {
    const window = focusedWindow()
    const result = await dialog.showOpenDialog(window!, {
      title: 'Open Folder',
      properties: ['openDirectory', 'createDirectory'],
      buttonLabel: 'Open',
    })
    return result.canceled ? null : (result.filePaths[0] ?? null)
  })

  ipcMain.handle('workspace:setRoot', async (_event, path: string): Promise<WorkspaceState> =>
    setWorkspaceRoot(path),
  )

  ipcMain.handle('workspace:list', async (_event, dirPath: string): Promise<DirEntry[]> => {
    const root = await requireRoot()
    return listDirectory(assertWithin(root, dirPath))
  })

  ipcMain.handle(
    'workspace:findFiles',
    async (_event, query: string, limit?: number): Promise<DirEntry[]> =>
      findFiles(await requireRoot(), query, limit),
  )

  ipcMain.handle(
    'workspace:search',
    async (_event, query: string, limit?: number): Promise<SearchHit[]> =>
      searchNotebooks(await requireRoot(), query, limit),
  )

  ipcMain.handle('workspace:revealInOS', async (_event, path: string): Promise<void> => {
    shell.showItemInFolder(path)
  })

  /* -------------------------------- notebook -------------------------------- */

  ipcMain.handle('notebook:open', async (_event, path: string): Promise<OpenedNotebook> => {
    const { notebook, modifiedAt } = await readNotebook(path)
    await recordRecentNotebook(path)
    return { path, notebook, modifiedAt }
  })

  ipcMain.handle(
    'notebook:save',
    async (
      _event,
      path: string,
      notebook: Notebook,
      expectedModifiedAt?: number,
    ): Promise<SaveResult> => {
      if (typeof expectedModifiedAt === 'number') {
        try {
          const stats = await stat(path)
          // 1ms of slack: mtime resolution varies across filesystems.
          if (stats.mtimeMs - expectedModifiedAt > 1) {
            return { status: 'conflict', modifiedAt: stats.mtimeMs }
          }
        } catch {
          // File was deleted or moved; writing simply recreates it.
        }
      }
      return { status: 'saved', modifiedAt: await writeNotebook(path, notebook) }
    },
  )

  ipcMain.handle(
    'notebook:saveAs',
    async (
      _event,
      notebook: Notebook,
      suggestedPath?: string,
    ): Promise<{ path: string; modifiedAt: number } | null> => {
      const window = focusedWindow()
      const result = await dialog.showSaveDialog(window!, {
        title: 'Save Notebook As',
        defaultPath: suggestedPath ?? 'Untitled.ipynb',
        filters: [{ name: 'Jupyter Notebook', extensions: ['ipynb'] }],
      })
      if (result.canceled || !result.filePath) return null

      const path = result.filePath.endsWith('.ipynb') ? result.filePath : `${result.filePath}.ipynb`
      const modifiedAt = await writeNotebook(path, notebook)
      await recordRecentNotebook(path)
      return { path, modifiedAt }
    },
  )

  ipcMain.handle('notebook:pickFile', async (): Promise<string | null> => {
    const window = focusedWindow()
    const result = await dialog.showOpenDialog(window!, {
      title: 'Open Notebook',
      properties: ['openFile'],
      filters: [{ name: 'Jupyter Notebook', extensions: ['ipynb'] }],
    })
    return result.canceled ? null : (result.filePaths[0] ?? null)
  })

  /* --------------------------------- kernels -------------------------------- */

  ipcMain.handle('kernels:listSpecs', async (): Promise<KernelSpec[]> => discoverKernelSpecs())

  ipcMain.handle(
    'kernels:start',
    async (_event, kernelName: string, cwd: string): Promise<SessionInfo> =>
      kernelManager.start(kernelName, await resolveKernelCwd(cwd)),
  )

  ipcMain.handle('kernels:listSessions', async (): Promise<SessionInfo[]> => kernelManager.list())

  ipcMain.handle('kernels:shutdown', async (_event, sessionId: string): Promise<void> =>
    kernelManager.shutdown(sessionId),
  )

  ipcMain.handle('kernels:restart', async (_event, sessionId: string): Promise<SessionInfo> =>
    kernelManager.restart(sessionId),
  )

  ipcMain.handle('kernels:interrupt', async (_event, sessionId: string): Promise<void> =>
    kernelManager.get(sessionId).interrupt(),
  )

  ipcMain.handle('kernels:execute', async (_event, sessionId: string, code: string): Promise<string> =>
    kernelManager.get(sessionId).execute(code),
  )

  ipcMain.handle(
    'kernels:complete',
    async (_event, sessionId: string, code: string, cursorPos: number): Promise<CompleteReplyContent> => {
      const reply = await kernelManager.get(sessionId).complete(code, cursorPos)
      return reply.content as unknown as CompleteReplyContent
    },
  )

  ipcMain.handle(
    'kernels:inspect',
    async (
      _event,
      sessionId: string,
      code: string,
      cursorPos: number,
      detailLevel: 0 | 1 = 0,
    ): Promise<InspectReplyContent> => {
      const reply = await kernelManager.get(sessionId).inspect(code, cursorPos, detailLevel)
      return reply.content as unknown as InspectReplyContent
    },
  )

  ipcMain.handle('kernels:sendInput', async (_event, sessionId: string, value: string): Promise<void> =>
    kernelManager.get(sessionId).sendInput(value),
  )

  /* --------------------------------- system --------------------------------- */

  ipcMain.handle('system:openExternal', async (_event, url: string): Promise<void> => {
    // Only ever hand http(s) to the OS browser.
    const parsed = new URL(url)
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
      throw new Error(`Refusing to open non-web URL: ${url}`)
    }
    await shell.openExternal(url)
  })
}
