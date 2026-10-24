import { useStore } from '../state/store'

export function TabBar() {
  const order = useStore((s) => s.order)
  const docs = useStore((s) => s.docs)
  const activePath = useStore((s) => s.activePath)
  const setActive = useStore((s) => s.setActive)
  const closeDoc = useStore((s) => s.closeDoc)

  if (order.length === 0) return null

  return (
    <div className="tabbar">
      {order.map((path) => {
        const doc = docs[path]
        if (!doc) return null
        return (
          <div
            key={path}
            className={`tab${path === activePath ? ' tab-active' : ''}`}
            onClick={() => setActive(path)}
            onAuxClick={(event) => event.button === 1 && closeDoc(path)}
            title={path}
          >
            <span className="tab-name">{doc.name.replace(/\.ipynb$/i, '')}</span>
            <button
              className="tab-close"
              onClick={(event) => {
                event.stopPropagation()
                closeDoc(path)
              }}
              // A dot instead of an × signals unsaved work at a glance.
              title={doc.dirty ? 'Unsaved changes — close anyway' : 'Close'}
            >
              {doc.dirty ? '●' : '✕'}
            </button>
          </div>
        )
      })}
    </div>
  )
}
