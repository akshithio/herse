/**
 * The only bridge between the renderer and Node. Context isolation is on and
 * node integration is off; this file exposes exactly the `HerseApi` surface and
 * nothing else.
 */

import { contextBridge, ipcRenderer } from 'electron'
import type { HerseApi, KernelMessageEvent, KernelStatusEvent } from '@shared/api'

/** Wrap an ipcRenderer.on subscription so callers get an unsubscribe function. */
function subscribe<T>(channel: string, handler: (payload: T) => void): () => void {
  const listener = (_event: Electron.IpcRendererEvent, payload: T) => handler(payload)
  ipcRenderer.on(channel, listener)
  return () => ipcRenderer.removeListener(channel, listener)
}

const api: HerseApi = {
  workspace: {
    getState: () => ipcRenderer.invoke('workspace:getState'),
    pickFolder: () => ipcRenderer.invoke('workspace:pickFolder'),
    setRoot: (path) => ipcRenderer.invoke('workspace:setRoot', path),
    list: (dirPath) => ipcRenderer.invoke('workspace:list', dirPath),
    findFiles: (query, limit) => ipcRenderer.invoke('workspace:findFiles', query, limit),
    search: (query, limit) => ipcRenderer.invoke('workspace:search', query, limit),
    revealInOS: (path) => ipcRenderer.invoke('workspace:revealInOS', path),
  },

  notebook: {
    open: (path) => ipcRenderer.invoke('notebook:open', path),
    save: (path, notebook, expectedModifiedAt) =>
      ipcRenderer.invoke('notebook:save', path, notebook, expectedModifiedAt),
    saveAs: (notebook, suggestedPath) => ipcRenderer.invoke('notebook:saveAs', notebook, suggestedPath),
    pickFile: () => ipcRenderer.invoke('notebook:pickFile'),
  },

  kernels: {
    listSpecs: () => ipcRenderer.invoke('kernels:listSpecs'),
    start: (kernelName, cwd) => ipcRenderer.invoke('kernels:start', kernelName, cwd),
    listSessions: () => ipcRenderer.invoke('kernels:listSessions'),
    shutdown: (sessionId) => ipcRenderer.invoke('kernels:shutdown', sessionId),
    restart: (sessionId) => ipcRenderer.invoke('kernels:restart', sessionId),
    interrupt: (sessionId) => ipcRenderer.invoke('kernels:interrupt', sessionId),
    execute: (sessionId, code) => ipcRenderer.invoke('kernels:execute', sessionId, code),
    complete: (sessionId, code, cursorPos) =>
      ipcRenderer.invoke('kernels:complete', sessionId, code, cursorPos),
    inspect: (sessionId, code, cursorPos, detailLevel) =>
      ipcRenderer.invoke('kernels:inspect', sessionId, code, cursorPos, detailLevel),
    sendInput: (sessionId, value) => ipcRenderer.invoke('kernels:sendInput', sessionId, value),
  },

  events: {
    onMessage: (handler) => subscribe<KernelMessageEvent>('kernel:message', handler),
    onStatus: (handler) => subscribe<KernelStatusEvent>('kernel:status', handler),
    onMenuCommand: (handler) => subscribe<string>('menu:command', handler),
  },

  system: {
    platform: process.platform,
    openExternal: (url) => ipcRenderer.invoke('system:openExternal', url),
  },
}

contextBridge.exposeInMainWorld('herse', api)
