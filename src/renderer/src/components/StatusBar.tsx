import { useState } from 'react'
import type { KernelStatus } from '@shared/jupyter'
import { selectActiveDoc, selectActiveSession, useStore } from '../state/store'

const STATUS_LABEL: Record<KernelStatus, string> = {
  starting: 'Starting…',
  idle: 'Idle',
  busy: 'Busy',
  restarting: 'Restarting…',
  dead: 'Dead',
  unknown: 'Unknown',
}

function KernelPicker({ onPick, onClose }: { onPick(name: string): void; onClose(): void }) {
  const specs = useStore((s) => s.specs)

  return (
    <>
      <div className="popover-backdrop" onClick={onClose} />
      <div className="popover">
        <div className="popover-title">Select kernel</div>
        {specs.length === 0 && (
          <div className="popover-empty">
            No kernels found. Install one with
            <code>python3 -m pip install ipykernel</code>
          </div>
        )}
        {specs.map((spec) => (
          <button key={spec.name} className="popover-row" onClick={() => onPick(spec.name)}>
            <span>{spec.display_name}</span>
            <span className="popover-hint">{spec.language}</span>
          </button>
        ))}
      </div>
    </>
  )
}

export function StatusBar() {
  const doc = useStore(selectActiveDoc)
  const session = useStore(selectActiveSession)
  const ensureSession = useStore((s) => s.ensureSession)
  const interrupt = useStore((s) => s.interrupt)
  const restartKernel = useStore((s) => s.restartKernel)
  const notify = useStore((s) => s.notify)
  const [picking, setPicking] = useState(false)

  if (!doc) return <div className="statusbar" />

  const status: KernelStatus = session?.status ?? 'unknown'
  const kernelLabel = session?.displayName ?? doc.notebook.metadata.kernelspec?.display_name ?? 'No kernel'

  return (
    <div className="statusbar">
      <button className="status-kernel" onClick={() => setPicking(true)}>
        <span className={`status-dot status-${status}`} />
        <span>{kernelLabel}</span>
        <span className="status-state">{session ? STATUS_LABEL[status] : 'Not started'}</span>
      </button>

      {session && (
        <>
          <button onClick={() => void interrupt(doc.path)} title="Interrupt the running cell">
            Interrupt
          </button>
          <button onClick={() => void restartKernel(doc.path)}>Restart</button>
        </>
      )}

      <div className="statusbar-spacer" />

      <span className="status-meta">
        {doc.notebook.cells.length} {doc.notebook.cells.length === 1 ? 'cell' : 'cells'}
        {doc.dirty ? ' · unsaved' : ''}
      </span>
      {doc.untitled ? (
        // The synthetic `untitled:…` key is an implementation detail, not a path.
        <span className="status-path">Not saved yet</span>
      ) : (
        // rtl truncates from the left; bdi keeps the path itself reading ltr.
        <span className="status-path" title={doc.path}>
          <bdi>{doc.path}</bdi>
        </span>
      )}

      {picking && (
        <KernelPicker
          onClose={() => setPicking(false)}
          onPick={(name) => {
            setPicking(false)
            void ensureSession(doc.path, name).catch((error: Error) => notify(error.message, 'error'))
          }}
        />
      )}
    </div>
  )
}
