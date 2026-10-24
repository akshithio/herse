/**
 * CodeMirror 6 wrapper for a single cell.
 *
 * Performance note, because it is easy to regress: react-codemirror dispatches
 * a full `StateEffect.reconfigure` whenever `extensions`, `basicSetup`, or
 * `onChange` change identity. Passing inline objects or arrow functions
 * therefore rebuilds the language parser, theme, and keymaps on every
 * keystroke. Everything below is deliberately referentially stable — the
 * handlers are read through a ref, and the setup objects are module constants.
 */

import { useCallback, useMemo, useRef } from 'react'
import CodeMirror, { type BasicSetupOptions, type ReactCodeMirrorRef } from '@uiw/react-codemirror'
import { EditorView, keymap } from '@codemirror/view'
import { Prec } from '@codemirror/state'
import { python } from '@codemirror/lang-python'
import { markdown } from '@codemirror/lang-markdown'
import type { CellType } from '@shared/nbformat'

export interface EditorProps {
  value: string
  cellType: CellType
  language: string
  editable?: boolean
  autoFocus?: boolean
  onChange(value: string): void
  onRun(): void
  onRunAndAdvance(): void
  onFocus?(): void
  /** Fired on Escape — the UI uses it to leave markdown edit mode. */
  onEscape?(): void
  editorRef?: React.Ref<ReactCodeMirrorRef>
}

const baseTheme = EditorView.theme(
  {
    '&': { backgroundColor: 'transparent', fontSize: '13px' },
    '.cm-content': {
      fontFamily: 'var(--font-mono)',
      padding: '10px 0',
      caretColor: 'var(--text)',
    },
    '.cm-gutters': {
      backgroundColor: 'transparent',
      border: 'none',
      color: 'var(--text-faint)',
    },
    '.cm-activeLine': { backgroundColor: 'var(--cell-active-line)' },
    '.cm-activeLineGutter': { backgroundColor: 'transparent' },
    '&.cm-focused': { outline: 'none' },
    '.cm-selectionBackground, ::selection': { backgroundColor: 'var(--selection) !important' },
    '.cm-cursor': { borderLeftColor: 'var(--text)' },

    // Autocomplete defaults to the system accent blue; keep it neutral.
    '.cm-tooltip': {
      backgroundColor: 'var(--bg)',
      border: '1px solid var(--border-strong)',
      borderRadius: '6px',
      boxShadow: '0 6px 20px rgba(0, 0, 0, 0.12)',
      overflow: 'hidden',
    },
    '.cm-tooltip-autocomplete > ul > li': {
      padding: '3px 8px',
      color: 'var(--text-dim)',
      fontFamily: 'var(--font-mono)',
    },
    '.cm-tooltip-autocomplete > ul > li[aria-selected]': {
      backgroundColor: 'var(--selection)',
      color: 'var(--text)',
    },
    '.cm-completionIcon': { color: 'var(--text-faint)' },
    '.cm-completionMatchedText': {
      textDecoration: 'none',
      fontWeight: '600',
      color: 'var(--text)',
    },
  },
  { dark: false },
)

const COMMON_SETUP = {
  foldGutter: false,
  highlightActiveLine: true,
  highlightActiveLineGutter: false,
  autocompletion: true,
  bracketMatching: true,
  closeBrackets: true,
  indentOnInput: true,
  searchKeymap: false,
} satisfies BasicSetupOptions

const CODE_SETUP: BasicSetupOptions = { ...COMMON_SETUP, lineNumbers: true }
const PROSE_SETUP: BasicSetupOptions = { ...COMMON_SETUP, lineNumbers: false }

function languageExtension(cellType: CellType, language: string) {
  if (cellType === 'markdown') return markdown()
  if (cellType === 'raw') return []
  // Only Python has a bundled grammar so far; other kernels get plain text
  // rather than mis-highlighted Python.
  return language.toLowerCase().startsWith('python') ? python() : []
}

export function Editor({
  value,
  cellType,
  language,
  editable = true,
  autoFocus = false,
  onChange,
  onRun,
  onRunAndAdvance,
  onFocus,
  onEscape,
  editorRef,
}: EditorProps) {
  // Latest handlers, read indirectly so their identity never reaches CodeMirror.
  const latest = useRef({ onChange, onRun, onRunAndAdvance, onFocus, onEscape })
  latest.current = { onChange, onRun, onRunAndAdvance, onFocus, onEscape }

  const handleChange = useCallback((next: string) => latest.current.onChange(next), [])
  const handleFocus = useCallback(() => latest.current.onFocus?.(), [])

  // Depends only on what genuinely changes the editor's configuration.
  const extensions = useMemo(
    () => [
      baseTheme,
      EditorView.lineWrapping,
      languageExtension(cellType, language),
      Prec.highest(
        keymap.of([
          {
            key: 'Shift-Enter',
            run: () => {
              latest.current.onRunAndAdvance()
              return true
            },
          },
          {
            key: 'Mod-Enter',
            run: () => {
              latest.current.onRun()
              return true
            },
          },
          {
            key: 'Escape',
            run: () => {
              latest.current.onEscape?.()
              return true
            },
          },
        ]),
      ),
    ],
    [cellType, language],
  )

  return (
    <CodeMirror
      ref={editorRef}
      value={value}
      theme="light"
      editable={editable}
      autoFocus={autoFocus}
      basicSetup={cellType === 'code' ? CODE_SETUP : PROSE_SETUP}
      extensions={extensions}
      onChange={handleChange}
      onFocus={handleFocus}
    />
  )
}
