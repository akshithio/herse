/**
 * Tiny JSON-file persistence for workspace state. Deliberately not a dependency
 * — this is a handful of strings in `userData/workspace.json`.
 */

import { app } from 'electron'
import { readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import type { WorkspaceState } from '@shared/api'

const MAX_RECENTS = 12

const DEFAULT_STATE: WorkspaceState = {
  root: null,
  recentFolders: [],
  recentNotebooks: [],
}

let cache: WorkspaceState | null = null

function statePath(): string {
  return join(app.getPath('userData'), 'workspace.json')
}

export async function getWorkspaceState(): Promise<WorkspaceState> {
  if (cache) return cache
  try {
    const parsed = JSON.parse(await readFile(statePath(), 'utf8'))
    cache = {
      root: typeof parsed.root === 'string' ? parsed.root : null,
      recentFolders: Array.isArray(parsed.recentFolders) ? parsed.recentFolders.map(String) : [],
      recentNotebooks: Array.isArray(parsed.recentNotebooks) ? parsed.recentNotebooks.map(String) : [],
    }
  } catch {
    cache = { ...DEFAULT_STATE }
  }
  return cache
}

async function persist(state: WorkspaceState): Promise<WorkspaceState> {
  cache = state
  await writeFile(statePath(), JSON.stringify(state, null, 2), 'utf8').catch(() => {
    // A failed write costs the user their recents list, not their work.
  })
  return state
}

function pushRecent(list: string[], value: string): string[] {
  return [value, ...list.filter((item) => item !== value)].slice(0, MAX_RECENTS)
}

export async function setWorkspaceRoot(root: string): Promise<WorkspaceState> {
  const state = await getWorkspaceState()
  return persist({ ...state, root, recentFolders: pushRecent(state.recentFolders, root) })
}

export async function recordRecentNotebook(path: string): Promise<void> {
  const state = await getWorkspaceState()
  await persist({ ...state, recentNotebooks: pushRecent(state.recentNotebooks, path) })
}
