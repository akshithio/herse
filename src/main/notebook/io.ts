/**
 * Reading and writing `.ipynb`.
 *
 * Two invariants:
 *   1. In memory, every multiline field is a plain string.
 *   2. On disk, they are arrays of lines — matching nbformat's own serializer,
 *      so a notebook saved by Herse produces a clean diff against one saved by
 *      Jupyter.
 */

import { readFile, stat, writeFile } from 'node:fs/promises'
import type {
  Cell,
  MimeBundle,
  MultilineString,
  Notebook,
  Output,
} from '@shared/nbformat'
import { joinSource, newCell, newCellId, splitSource } from '@shared/nbformat'

/** MIME types whose payload is text and therefore line-split on disk. */
function isTextMime(mime: string): boolean {
  return mime.startsWith('text/') || mime === 'image/svg+xml' || mime === 'application/javascript'
}

function normalizeBundle(bundle: MimeBundle | undefined): MimeBundle {
  const result: MimeBundle = {}
  for (const [mime, value] of Object.entries(bundle ?? {})) {
    result[mime] = isTextMime(mime) ? joinSource(value as MultilineString) : (value as never)
  }
  return result
}

function denormalizeBundle(bundle: MimeBundle | undefined): MimeBundle {
  const result: MimeBundle = {}
  for (const [mime, value] of Object.entries(bundle ?? {})) {
    result[mime] = isTextMime(mime) ? splitSource(String(value ?? '')) : (value as never)
  }
  return result
}

function normalizeOutput(raw: Record<string, unknown>): Output | null {
  switch (raw.output_type) {
    case 'stream':
      return {
        output_type: 'stream',
        name: raw.name === 'stderr' ? 'stderr' : 'stdout',
        text: joinSource(raw.text as MultilineString),
      }
    case 'display_data':
      return {
        output_type: 'display_data',
        data: normalizeBundle(raw.data as MimeBundle),
        metadata: (raw.metadata as Record<string, unknown>) ?? {},
        transient: raw.transient as { display_id?: string } | undefined,
      }
    case 'execute_result':
      return {
        output_type: 'execute_result',
        execution_count: (raw.execution_count as number | null) ?? null,
        data: normalizeBundle(raw.data as MimeBundle),
        metadata: (raw.metadata as Record<string, unknown>) ?? {},
        transient: raw.transient as { display_id?: string } | undefined,
      }
    case 'error':
      return {
        output_type: 'error',
        ename: String(raw.ename ?? 'Error'),
        evalue: String(raw.evalue ?? ''),
        traceback: Array.isArray(raw.traceback) ? raw.traceback.map(String) : [],
      }
    default:
      // Unknown output types are dropped rather than crashing the open.
      return null
  }
}

function denormalizeOutput(output: Output): Record<string, unknown> {
  switch (output.output_type) {
    case 'stream':
      return { output_type: 'stream', name: output.name, text: splitSource(output.text as string) }
    case 'display_data':
      return {
        output_type: 'display_data',
        data: denormalizeBundle(output.data),
        metadata: output.metadata,
      }
    case 'execute_result':
      return {
        output_type: 'execute_result',
        execution_count: output.execution_count,
        data: denormalizeBundle(output.data),
        metadata: output.metadata,
      }
    case 'error':
      return {
        output_type: 'error',
        ename: output.ename,
        evalue: output.evalue,
        traceback: output.traceback,
      }
  }
}

function normalizeCell(raw: Record<string, unknown>): Cell {
  const type = raw.cell_type
  const base = {
    // nbformat < 4.5 has no cell ids; synthesize stable-enough ones on read.
    id: typeof raw.id === 'string' && raw.id.length > 0 ? raw.id : newCellId(),
    metadata: (raw.metadata as Record<string, unknown>) ?? {},
    source: joinSource(raw.source as MultilineString),
  }

  if (type === 'code') {
    const rawOutputs = Array.isArray(raw.outputs) ? raw.outputs : []
    return {
      ...base,
      cell_type: 'code',
      execution_count: (raw.execution_count as number | null) ?? null,
      outputs: rawOutputs
        .map((o) => normalizeOutput(o as Record<string, unknown>))
        .filter((o): o is Output => o !== null),
    }
  }

  if (type === 'markdown') {
    return { ...base, cell_type: 'markdown', attachments: raw.attachments as never }
  }

  return { ...base, cell_type: 'raw', attachments: raw.attachments as never }
}

function denormalizeCell(cell: Cell): Record<string, unknown> {
  const base: Record<string, unknown> = {
    cell_type: cell.cell_type,
    id: cell.id,
    metadata: cell.metadata,
    source: splitSource(cell.source),
  }

  if (cell.cell_type === 'code') {
    base.execution_count = cell.execution_count
    base.outputs = cell.outputs.map(denormalizeOutput)
  } else if (cell.attachments) {
    base.attachments = cell.attachments
  }

  return base
}

export class NotebookFormatError extends Error {}

export function parseNotebook(text: string): Notebook {
  let raw: Record<string, unknown>
  try {
    raw = JSON.parse(text)
  } catch (error) {
    throw new NotebookFormatError(`Not valid JSON: ${(error as Error).message}`)
  }

  const nbformat = Number(raw.nbformat ?? 0)
  if (nbformat < 4) {
    throw new NotebookFormatError(
      `nbformat v${nbformat || '?'} is not supported. Convert it with: jupyter nbconvert --to notebook --nbformat 4`,
    )
  }

  const rawCells = Array.isArray(raw.cells) ? raw.cells : []
  const cells = rawCells.map((c) => normalizeCell(c as Record<string, unknown>))

  return {
    nbformat,
    // We write cell ids, which requires >= 4.5.
    nbformat_minor: Math.max(Number(raw.nbformat_minor ?? 5), 5),
    metadata: (raw.metadata as Notebook['metadata']) ?? {},
    cells: cells.length > 0 ? cells : [newCell('code')],
  }
}

export function serializeNotebook(notebook: Notebook): string {
  const out = {
    cells: notebook.cells.map(denormalizeCell),
    metadata: notebook.metadata,
    nbformat: notebook.nbformat,
    nbformat_minor: notebook.nbformat_minor,
  }
  // One-space indent + trailing newline is what nbformat writes.
  return JSON.stringify(out, null, 1) + '\n'
}

export async function readNotebook(path: string): Promise<{ notebook: Notebook; modifiedAt: number }> {
  const [text, stats] = await Promise.all([readFile(path, 'utf8'), stat(path)])
  return { notebook: parseNotebook(text), modifiedAt: stats.mtimeMs }
}

export async function writeNotebook(path: string, notebook: Notebook): Promise<number> {
  await writeFile(path, serializeNotebook(notebook), 'utf8')
  return (await stat(path)).mtimeMs
}
