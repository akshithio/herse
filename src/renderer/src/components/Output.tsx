/**
 * Output rendering.
 *
 * A MIME bundle can carry several representations of the same value; we pick
 * the richest one we can render. Everything HTML-ish goes through DOMPurify —
 * notebook output is untrusted content that arrives from whoever wrote the file.
 */

import { useMemo } from 'react'
import Convert from 'ansi-to-html'
import DOMPurify from 'dompurify'
import type { MimeBundle, Output } from '@shared/nbformat'
import { joinSource } from '@shared/nbformat'

const ansi = new Convert({
  // Default foreground for unstyled runs; ANSI-coloured runs override it.
  fg: '#1b1b19',
  bg: 'transparent',
  newline: false,
  escapeXML: true,
  stream: false,
})

/** Richest first. The first match in a bundle wins. */
const MIME_PRIORITY = [
  'text/html',
  'image/svg+xml',
  'image/png',
  'image/jpeg',
  'image/gif',
  'text/markdown',
  'text/latex',
  'application/json',
  'text/plain',
] as const

function pickMime(data: MimeBundle): string | null {
  for (const mime of MIME_PRIORITY) {
    if (data[mime] !== undefined) return mime
  }
  return Object.keys(data)[0] ?? null
}

function sanitize(html: string): string {
  return DOMPurify.sanitize(html, {
    // Notebook HTML output is largely tables (pandas) and inline styles.
    ADD_TAGS: ['style'],
    ADD_ATTR: ['target'],
    FORBID_TAGS: ['script', 'iframe', 'object', 'embed', 'form'],
    FORBID_ATTR: ['onerror', 'onload', 'onclick'],
  })
}

function AnsiText({ text, stderr = false }: { text: string; stderr?: boolean }) {
  const html = useMemo(() => sanitize(ansi.toHtml(text)), [text])
  return (
    <pre
      className={`out-stream${stderr ? ' out-stderr' : ''}`}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  )
}

function MimeView({ data }: { data: MimeBundle }) {
  const mime = pickMime(data)
  const raw = mime ? data[mime] : undefined

  // Hooks must run unconditionally, so sanitize up front for the markup cases.
  const markup = useMemo(() => {
    if (mime !== 'text/html' && mime !== 'image/svg+xml') return ''
    return sanitize(joinSource(raw as string))
  }, [mime, raw])

  if (!mime) return null

  switch (mime) {
    case 'text/html':
      return <div className="out-html" dangerouslySetInnerHTML={{ __html: markup }} />

    case 'image/svg+xml':
      return <div className="out-image" dangerouslySetInnerHTML={{ __html: markup }} />

    case 'image/png':
    case 'image/jpeg':
    case 'image/gif': {
      // Image payloads are base64 in the bundle.
      const base64 = joinSource(raw as string).replace(/\s/g, '')
      return (
        <div className="out-image">
          <img src={`data:${mime};base64,${base64}`} alt="" />
        </div>
      )
    }

    case 'application/json':
      return <pre className="out-text">{JSON.stringify(raw, null, 2)}</pre>

    case 'text/latex':
    case 'text/markdown':
    case 'text/plain':
    default:
      return <pre className="out-text">{joinSource(raw as string)}</pre>
  }
}

function ErrorView({ ename, evalue, traceback }: { ename: string; evalue: string; traceback: string[] }) {
  // The traceback already contains ename/evalue; fall back only if it's empty.
  const text = traceback.length > 0 ? traceback.join('\n') : `${ename}: ${evalue}`
  const html = useMemo(() => sanitize(ansi.toHtml(text)), [text])
  return <pre className="out-error" dangerouslySetInnerHTML={{ __html: html }} />
}

export function OutputView({ output }: { output: Output }) {
  switch (output.output_type) {
    case 'stream':
      return <AnsiText text={joinSource(output.text)} stderr={output.name === 'stderr'} />
    case 'display_data':
    case 'execute_result':
      return <MimeView data={output.data} />
    case 'error':
      return <ErrorView ename={output.ename} evalue={output.evalue} traceback={output.traceback} />
  }
}

export function OutputList({ outputs }: { outputs: Output[] }) {
  if (outputs.length === 0) return null
  return (
    <div className="outputs">
      {outputs.map((output, i) => (
        <OutputView key={i} output={output} />
      ))}
    </div>
  )
}
