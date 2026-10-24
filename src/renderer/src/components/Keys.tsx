/**
 * Keyboard shortcuts rendered as spelled-out chips.
 *
 * The macOS glyphs (⌘ ⇧ ⏎ ⌥) render inconsistently depending on the system UI
 * font and read as noise at small sizes, so we name the keys instead.
 */

const IS_MAC = typeof navigator !== 'undefined' && navigator.platform.toLowerCase().includes('mac')

/** `Mod` resolves to Cmd on macOS and Ctrl everywhere else. */
export type KeyName = 'Mod' | 'Shift' | 'Alt' | 'Enter' | (string & {})

function label(key: KeyName): string {
  if (key === 'Mod') return IS_MAC ? 'Cmd' : 'Ctrl'
  if (key === 'Alt') return IS_MAC ? 'Option' : 'Alt'
  return key
}

export function Keys({ keys }: { keys: KeyName[] }) {
  return (
    <span className="keys">
      {keys.map((key, i) => (
        <kbd key={i}>{label(key)}</kbd>
      ))}
    </span>
  )
}
