/**
 * Workspace browsing: directory listing, fuzzy file finding, and full-text
 * search across notebook cell sources.
 *
 * The search is the reason this file exists. Jupyter's web tree can only match
 * filenames; being able to ask "where did I write `train_test_split`?" across a
 * folder of notebooks is most of what makes a desktop client worth using.
 */

import { readdir, readFile, stat } from 'node:fs/promises'
import { basename, join, relative, resolve, sep } from 'node:path'
import type { DirEntry, NotebookSummary, SearchHit } from '@shared/api'
import { parseNotebook } from '../notebook/io'

const IGNORED_DIRS = new Set([
  'node_modules',
  '.git',
  '.hg',
  '.svn',
  '.ipynb_checkpoints',
  '__pycache__',
  '.venv',
  'venv',
  'env',
  '.tox',
  '.mypy_cache',
  '.pytest_cache',
  'dist',
  'build',
  '.next',
  'target',
])

/** Notebooks above this size are skipped by search — they're output-heavy dumps. */
const MAX_SEARCHABLE_BYTES = 12 * 1024 * 1024
const MAX_WALK_ENTRIES = 40_000

export function isNotebook(name: string): boolean {
  return name.toLowerCase().endsWith('.ipynb')
}

/** Guards against a renderer bug walking outside the opened workspace. */
export function assertWithin(root: string, target: string): string {
  const resolvedRoot = resolve(root)
  const resolvedTarget = resolve(target)
  if (resolvedTarget !== resolvedRoot && !resolvedTarget.startsWith(resolvedRoot + sep)) {
    throw new Error(`Path is outside the workspace: ${target}`)
  }
  return resolvedTarget
}

async function summarizeNotebook(path: string): Promise<NotebookSummary | undefined> {
  try {
    const stats = await stat(path)
    if (stats.size > MAX_SEARCHABLE_BYTES) return undefined
    const notebook = parseNotebook(await readFile(path, 'utf8'))
    const codeCells = notebook.cells.filter((c) => c.cell_type === 'code')
    return {
      cellCount: notebook.cells.length,
      codeCellCount: codeCells.length,
      kernelDisplayName: notebook.metadata.kernelspec?.display_name,
      language: notebook.metadata.language_info?.name ?? notebook.metadata.kernelspec?.language,
      hasOutputs: codeCells.some((c) => c.cell_type === 'code' && c.outputs.length > 0),
    }
  } catch {
    // Corrupt or unreadable — the entry still lists, just without a summary.
    return undefined
  }
}

export async function listDirectory(dirPath: string): Promise<DirEntry[]> {
  const dirents = await readdir(dirPath, { withFileTypes: true })
  const entries: DirEntry[] = []

  for (const dirent of dirents) {
    // Hide dotfiles and noise directories; they are never what you're browsing for.
    if (dirent.name.startsWith('.')) continue
    if (dirent.isDirectory() && IGNORED_DIRS.has(dirent.name)) continue

    const path = join(dirPath, dirent.name)
    let stats: Awaited<ReturnType<typeof stat>>
    try {
      stats = await stat(path)
    } catch {
      continue // Broken symlink or a race with an external delete.
    }

    entries.push({
      name: dirent.name,
      path,
      isDirectory: stats.isDirectory(),
      size: stats.size,
      modifiedAt: stats.mtimeMs,
      notebook: stats.isDirectory() || !isNotebook(dirent.name) ? undefined : await summarizeNotebook(path),
    })
  }

  // Directories first, then alphabetical — the ordering people expect.
  return entries.sort((a, b) => {
    if (a.isDirectory !== b.isDirectory) return a.isDirectory ? -1 : 1
    return a.name.localeCompare(b.name, undefined, { numeric: true })
  })
}

interface WalkedFile {
  path: string
  name: string
  size: number
  modifiedAt: number
  isDirectory: boolean
}

async function* walk(root: string): AsyncGenerator<WalkedFile> {
  const queue: string[] = [root]
  let seen = 0

  while (queue.length > 0) {
    const dir = queue.shift()!
    let dirents
    try {
      dirents = await readdir(dir, { withFileTypes: true })
    } catch {
      continue
    }

    for (const dirent of dirents) {
      if (dirent.name.startsWith('.')) continue
      const path = join(dir, dirent.name)

      if (dirent.isDirectory()) {
        if (!IGNORED_DIRS.has(dirent.name)) queue.push(path)
        continue
      }

      if (++seen > MAX_WALK_ENTRIES) return

      let stats
      try {
        stats = await stat(path)
      } catch {
        continue
      }

      yield {
        path,
        name: dirent.name,
        size: stats.size,
        modifiedAt: stats.mtimeMs,
        isDirectory: false,
      }
    }
  }
}

/**
 * Subsequence match with a score that rewards contiguous runs and matches at
 * word boundaries — the behaviour people expect from an editor's quick-open.
 * Returns null when the query doesn't match at all.
 */
function fuzzyScore(text: string, query: string): number | null {
  const haystack = text.toLowerCase()
  const needle = query.toLowerCase()
  if (needle.length === 0) return 0

  let score = 0
  let cursor = 0
  let previousIndex = -1

  for (const char of needle) {
    const index = haystack.indexOf(char, cursor)
    if (index === -1) return null

    if (index === previousIndex + 1) score += 8 // contiguous
    if (index === 0 || /[\s\-_./]/.test(haystack[index - 1])) score += 6 // word start

    score -= Math.min(index - cursor, 12) // penalize distance from the last match
    previousIndex = index
    cursor = index + 1
  }

  // Prefer shorter names when scores are otherwise close.
  return score - haystack.length * 0.05
}

export async function findFiles(root: string, query: string, limit = 50): Promise<DirEntry[]> {
  const scored: Array<{ entry: DirEntry; score: number }> = []

  for await (const file of walk(root)) {
    const relativePath = relative(root, file.path)
    // Score the name, but fall back to the full relative path so "data/clean"
    // finds `data/cleaning.ipynb`.
    const score = fuzzyScore(file.name, query) ?? fuzzyScore(relativePath, query)
    if (score === null) continue

    scored.push({
      entry: {
        name: file.name,
        path: file.path,
        isDirectory: false,
        size: file.size,
        modifiedAt: file.modifiedAt,
      },
      // Nudge notebooks up: this is a notebook app.
      score: score + (isNotebook(file.name) ? 15 : 0),
    })
  }

  return scored
    .sort((a, b) => b.score - a.score)
    .slice(0, limit)
    .map((s) => s.entry)
}

export async function searchNotebooks(root: string, query: string, limit = 200): Promise<SearchHit[]> {
  const trimmed = query.trim()
  if (trimmed.length < 2) return []

  const needle = trimmed.toLowerCase()
  const hits: SearchHit[] = []

  for await (const file of walk(root)) {
    if (hits.length >= limit) break
    if (!isNotebook(file.name) || file.size > MAX_SEARCHABLE_BYTES) continue

    let notebook
    try {
      notebook = parseNotebook(await readFile(file.path, 'utf8'))
    } catch {
      continue
    }

    notebook.cells.forEach((cell, cellIndex) => {
      if (hits.length >= limit) return
      // Cheap reject before splitting into lines.
      if (!cell.source.toLowerCase().includes(needle)) return

      const lines = cell.source.split('\n')
      for (let i = 0; i < lines.length; i++) {
        if (hits.length >= limit) return
        if (!lines[i].toLowerCase().includes(needle)) continue

        hits.push({
          path: file.path,
          name: basename(file.path),
          cellId: cell.id,
          cellIndex,
          cellType: cell.cell_type,
          preview: lines[i].trim().slice(0, 240),
          lineNumber: i + 1,
        })
      }
    })
  }

  return hits
}
