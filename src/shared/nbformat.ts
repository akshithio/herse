/**
 * Minimal, practical nbformat v4 types.
 *
 * On disk, nbformat stores multiline strings as arrays of lines. In memory we
 * always work with plain strings — `src/main/notebook/io.ts` normalizes on read
 * and denormalizes on write so saved files stay diff-friendly and byte-similar
 * to what Jupyter itself would emit.
 */

export type MultilineString = string | string[]

export interface MimeBundle {
  [mimeType: string]: MultilineString | Record<string, unknown> | undefined
}

export interface StreamOutput {
  output_type: 'stream'
  name: 'stdout' | 'stderr'
  text: MultilineString
}

export interface DisplayDataOutput {
  output_type: 'display_data'
  data: MimeBundle
  metadata: Record<string, unknown>
  /** Present when the output was published with a display_id for later update. */
  transient?: { display_id?: string }
}

export interface ExecuteResultOutput {
  output_type: 'execute_result'
  execution_count: number | null
  data: MimeBundle
  metadata: Record<string, unknown>
  transient?: { display_id?: string }
}

export interface ErrorOutput {
  output_type: 'error'
  ename: string
  evalue: string
  traceback: string[]
}

export type Output = StreamOutput | DisplayDataOutput | ExecuteResultOutput | ErrorOutput

export interface BaseCell {
  id: string
  metadata: Record<string, unknown>
  source: string
}

export interface CodeCell extends BaseCell {
  cell_type: 'code'
  execution_count: number | null
  outputs: Output[]
}

export interface MarkdownCell extends BaseCell {
  cell_type: 'markdown'
  attachments?: Record<string, MimeBundle>
}

export interface RawCell extends BaseCell {
  cell_type: 'raw'
  attachments?: Record<string, MimeBundle>
}

export type Cell = CodeCell | MarkdownCell | RawCell
export type CellType = Cell['cell_type']

export interface KernelSpecMetadata {
  name?: string
  display_name?: string
  language?: string
}

export interface LanguageInfo {
  name?: string
  version?: string
  file_extension?: string
  mimetype?: string
  pygments_lexer?: string
}

export interface NotebookMetadata {
  kernelspec?: KernelSpecMetadata
  language_info?: LanguageInfo
  [key: string]: unknown
}

export interface Notebook {
  nbformat: number
  nbformat_minor: number
  metadata: NotebookMetadata
  cells: Cell[]
}

export function isCodeCell(cell: Cell): cell is CodeCell {
  return cell.cell_type === 'code'
}

/** Collapse nbformat's `string | string[]` into a single string. */
export function joinSource(value: MultilineString | undefined): string {
  if (value == null) return ''
  return Array.isArray(value) ? value.join('') : value
}

/**
 * Split a string back into nbformat's line-array representation: every line
 * keeps its trailing newline except the last. This is exactly what nbformat's
 * own serializer does, which keeps our writes diff-clean against Jupyter's.
 */
export function splitSource(value: string): string[] {
  if (value === '') return []
  const lines = value.split('\n')
  return lines.map((line, i) => (i === lines.length - 1 ? line : line + '\n')).filter((l) => l !== '')
}

export function emptyNotebook(kernel?: KernelSpecMetadata, language?: LanguageInfo): Notebook {
  return {
    nbformat: 4,
    nbformat_minor: 5,
    metadata: {
      ...(kernel ? { kernelspec: kernel } : {}),
      ...(language ? { language_info: language } : {}),
    },
    cells: [newCell('code')],
  }
}

/** nbformat 4.5 cell ids: 1-64 chars of [a-zA-Z0-9-_]. */
export function newCellId(): string {
  return Math.random().toString(36).slice(2, 10) + Math.random().toString(36).slice(2, 6)
}

export function newCell(type: CellType, source = ''): Cell {
  const base = { id: newCellId(), metadata: {}, source }
  if (type === 'code') return { ...base, cell_type: 'code', execution_count: null, outputs: [] }
  if (type === 'markdown') return { ...base, cell_type: 'markdown' }
  return { ...base, cell_type: 'raw' }
}
