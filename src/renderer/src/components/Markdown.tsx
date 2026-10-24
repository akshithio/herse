/**
 * Markdown cell rendering, with KaTeX for `$…$` and `$$…$$`.
 *
 * Math is extracted before the markdown pass and re-inserted after, so the
 * markdown parser can't mangle backslashes or underscores inside formulas —
 * the classic failure mode when you render math after markdown.
 */

import { useMemo } from 'react'
import DOMPurify from 'dompurify'
import katex from 'katex'
import { marked } from 'marked'

marked.setOptions({ gfm: true, breaks: false })

interface Extracted {
  text: string
  math: string[]
}

/**
 * Pull code spans out first (a `$` inside `` `code` `` is not math), then math.
 * Both become placeholders that markdown passes through untouched.
 */
function extractMath(source: string): Extracted {
  const math: string[] = []
  const code: string[] = []

  const withCodeHeld = source.replace(/```[\s\S]*?```|~~~[\s\S]*?~~~|`[^`\n]*`/g, (match) => {
    code.push(match)
    return `%%HERSECODE${code.length - 1}%%`
  })

  const holdMath = (tex: string, display: boolean): string => {
    try {
      math.push(katex.renderToString(tex, { displayMode: display, throwOnError: false, output: 'html' }))
    } catch {
      // Malformed TeX renders as literal source rather than breaking the cell.
      math.push(display ? `<pre>${tex}</pre>` : `<code>${tex}</code>`)
    }
    return `%%HERSEMATH${math.length - 1}%%`
  }

  const text = withCodeHeld
    .replace(/\$\$([\s\S]+?)\$\$/g, (_m, tex: string) => holdMath(tex.trim(), true))
    .replace(/(?<!\\)\$([^$\n]+?)(?<!\\)\$/g, (_m, tex: string) => holdMath(tex.trim(), false))
    // Code goes back in before the markdown pass so it still parses as code.
    .replace(/%%HERSECODE(\d+)%%/g, (_m, i: string) => code[Number(i)] ?? '')

  return { text, math }
}

function restoreMath(html: string, math: string[]): string {
  return html.replace(/%%HERSEMATH(\d+)%%/g, (_m, index: string) => math[Number(index)] ?? '')
}

export function Markdown({ source }: { source: string }) {
  const html = useMemo(() => {
    const { text, math } = extractMath(source)
    const parsed = marked.parse(text, { async: false }) as string
    return DOMPurify.sanitize(restoreMath(parsed, math), {
      ADD_TAGS: ['math', 'semantics', 'annotation', 'mrow', 'mi', 'mo', 'mn', 'msup', 'msub'],
      ADD_ATTR: ['target', 'rel'],
      FORBID_TAGS: ['script', 'iframe', 'object', 'embed', 'form'],
    })
  }, [source])

  return <div className="markdown" dangerouslySetInnerHTML={{ __html: html }} />
}
