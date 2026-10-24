/**
 * Turning iopub messages into nbformat outputs.
 *
 * Kept separate from the store because this is the fiddly, spec-driven part:
 * stream coalescing, carriage-return handling, and `display_id` updates.
 */

import type { Output, StreamOutput } from '@shared/nbformat'
import type {
  ClearOutputContent,
  DisplayDataContent,
  ErrorContent,
  ExecuteResultContent,
  JupyterMessage,
  StreamContent,
} from '@shared/jupyter'

/**
 * Collapse `\r` the way a terminal would, so tqdm/progress bars render as a
 * single updating line instead of hundreds of stacked ones.
 */
export function applyCarriageReturns(text: string): string {
  if (!text.includes('\r')) return text

  const out: string[] = []
  for (const line of text.split('\n')) {
    if (!line.includes('\r')) {
      out.push(line)
      continue
    }
    // Each \r returns to column 0; later writes overwrite from there.
    let rendered = ''
    for (const segment of line.split('\r')) {
      rendered = segment + rendered.slice(segment.length)
    }
    out.push(rendered)
  }
  return out.join('\n')
}

/** Cap a single stream output so a runaway loop can't lock up the renderer. */
const MAX_STREAM_CHARS = 200_000

function appendStream(outputs: Output[], content: StreamContent): Output[] {
  const last = outputs[outputs.length - 1]

  if (last?.output_type === 'stream' && last.name === content.name) {
    const merged = applyCarriageReturns((last.text as string) + content.text)
    const text =
      merged.length > MAX_STREAM_CHARS
        ? `…[output truncated]…\n${merged.slice(-MAX_STREAM_CHARS)}`
        : merged
    const updated: StreamOutput = { ...last, text }
    return [...outputs.slice(0, -1), updated]
  }

  return [
    ...outputs,
    { output_type: 'stream', name: content.name, text: applyCarriageReturns(content.text) },
  ]
}

export interface ApplyResult {
  outputs: Output[]
  /** Set when the message carried a new execution count for the cell. */
  executionCount?: number | null
  /** True once the kernel reports this execution finished. */
  finished?: boolean
  /** True when the message was an error, so the UI can flag the cell. */
  errored?: boolean
}

/**
 * Fold one kernel message into a cell's existing outputs. Returns null when the
 * message doesn't affect outputs at all.
 */
export function applyMessage(outputs: Output[], message: JupyterMessage): ApplyResult | null {
  const type = message.header.msg_type

  switch (type) {
    case 'execute_input':
      return {
        outputs,
        executionCount: (message.content as { execution_count: number }).execution_count,
      }

    case 'stream':
      return { outputs: appendStream(outputs, message.content as unknown as StreamContent) }

    case 'display_data': {
      const content = message.content as unknown as DisplayDataContent
      return {
        outputs: [
          ...outputs,
          {
            output_type: 'display_data',
            data: content.data,
            metadata: content.metadata ?? {},
            transient: content.transient,
          },
        ],
      }
    }

    case 'update_display_data': {
      const content = message.content as unknown as DisplayDataContent
      const displayId = content.transient?.display_id
      if (!displayId) return null
      // Replace in place — this is how animated/updating widgets work.
      return {
        outputs: outputs.map((output) =>
          'transient' in output && output.transient?.display_id === displayId
            ? { ...output, data: content.data, metadata: content.metadata ?? {} }
            : output,
        ),
      }
    }

    case 'execute_result': {
      const content = message.content as unknown as ExecuteResultContent
      return {
        outputs: [
          ...outputs,
          {
            output_type: 'execute_result',
            execution_count: content.execution_count,
            data: content.data,
            metadata: content.metadata ?? {},
          },
        ],
        executionCount: content.execution_count,
      }
    }

    case 'error': {
      const content = message.content as unknown as ErrorContent
      return {
        outputs: [
          ...outputs,
          {
            output_type: 'error',
            ename: content.ename,
            evalue: content.evalue,
            traceback: content.traceback ?? [],
          },
        ],
        errored: true,
      }
    }

    case 'clear_output': {
      const content = message.content as unknown as ClearOutputContent
      // `wait: true` means "clear when the next output arrives". We approximate
      // by clearing immediately, which is what the common case (a redraw loop
      // that writes right after) looks like anyway.
      return { outputs: content.wait ? outputs : [] }
    }

    case 'execute_reply': {
      const content = message.content as { status?: string; execution_count?: number }
      return {
        outputs,
        executionCount: content.execution_count ?? undefined,
        finished: true,
        errored: content.status === 'error',
      }
    }

    default:
      return null
  }
}
