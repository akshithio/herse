/**
 * Owns every live KernelSession and fans their events out to renderer windows.
 */

import type { BrowserWindow } from 'electron'
import type { KernelSpec, SessionInfo } from '@shared/jupyter'
import type { KernelMessageEvent, KernelStatusEvent } from '@shared/api'
import { KernelSession } from './session'
import { findKernelSpec } from './kernelspecs'

export class KernelManager {
  private sessions = new Map<string, KernelSession>()
  private windows = new Set<BrowserWindow>()

  registerWindow(window: BrowserWindow): void {
    this.windows.add(window)
    window.on('closed', () => this.windows.delete(window))
  }

  private broadcast(channel: string, payload: unknown): void {
    for (const window of this.windows) {
      if (!window.isDestroyed()) window.webContents.send(channel, payload)
    }
  }

  async start(kernelName: string, cwd: string): Promise<SessionInfo> {
    const spec: KernelSpec | undefined = await findKernelSpec(kernelName)
    if (!spec) throw new Error(`No kernel named "${kernelName}" is installed`)

    const session = new KernelSession(spec, cwd)

    session.on('message', ({ channel, message }) => {
      const event: KernelMessageEvent = { sessionId: session.id, channel, message }
      this.broadcast('kernel:message', event)
    })

    session.on('status', ({ status, reason }) => {
      const event: KernelStatusEvent = { sessionId: session.id, status, reason }
      this.broadcast('kernel:status', event)
    })

    // Register before start() so status events during boot reach the renderer.
    this.sessions.set(session.id, session)

    try {
      await session.start()
    } catch (error) {
      this.sessions.delete(session.id)
      await session.dispose().catch(() => {})
      throw error
    }

    return session.info()
  }

  get(sessionId: string): KernelSession {
    const session = this.sessions.get(sessionId)
    if (!session) throw new Error(`Unknown kernel session: ${sessionId}`)
    return session
  }

  list(): SessionInfo[] {
    return [...this.sessions.values()].map((session) => session.info())
  }

  async shutdown(sessionId: string): Promise<void> {
    const session = this.sessions.get(sessionId)
    if (!session) return
    this.sessions.delete(sessionId)
    await session.dispose()
  }

  async restart(sessionId: string): Promise<SessionInfo> {
    const session = this.get(sessionId)
    await session.restart()
    return session.info()
  }

  /** Called on app quit; kernels are child processes and must not be orphaned. */
  async shutdownAll(): Promise<void> {
    const sessions = [...this.sessions.values()]
    this.sessions.clear()
    await Promise.allSettled(sessions.map((session) => session.dispose()))
  }
}

export const kernelManager = new KernelManager()
