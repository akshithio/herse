# Herse

A desktop workspace for Jupyter notebooks. No `jupyter server`, no browser tab —
Herse discovers the kernels already installed on your machine, launches them
itself, and speaks the Jupyter wire protocol to them directly over ZeroMQ.

The premise: most of the friction in notebook work isn't running cells, it's
*finding* things. Herse treats a folder of notebooks as a workspace you can
browse and quick-open, with each notebook annotated by cell count and kernel.

## Status

Working vertical slice. Open a notebook, execute against a real kernel, see
streamed output, save. Verified end-to-end against a live `ipykernel`:

```
boot: 359ms  ·  restart: 413ms
stdout / execute_result / display_data / error tracebacks  ✓
interrupt → KeyboardInterrupt                              ✓
restart clears kernel state                                ✓
heartbeat death detection                                  ✓
round-trip validates against Python's own nbformat         ✓
```

## Run it

```bash
npm install && npm run dev
```

You need at least one Jupyter kernel installed. If Herse reports none:

```bash
python3 -m pip install ipykernel && python3 -m ipykernel install --user
```

Other commands:

```bash
npm run typecheck   # both tsconfig projects
npm run build       # bundle main + preload + renderer into out/
npm run dist:mac    # package with electron-builder
```

## Architecture

Three processes, one type contract. `src/shared/api.ts` declares `HerseApi`;
the preload implements it and the main-process handlers are checked against it,
so an IPC signature change breaks at compile time rather than at runtime.

```
src/
  shared/           types shared by every process
    nbformat.ts       notebook model (in memory: plain strings)
    jupyter.ts        wire protocol message types
    api.ts            the IPC contract — the source of truth
  main/
    kernel/
      wire.ts         v5.3 framing + HMAC signing
      kernelspecs.ts  discovery (jupyter CLI, then directory scan)
      connection.ts   port reservation, connection files
      session.ts      one kernel: process + 5 ZMQ channels
      manager.ts      session registry, fans events to windows
    notebook/io.ts    .ipynb read/write
    workspace/fs.ts   listing, fuzzy find, cross-notebook search (see below)
    ipc.ts, menu.ts, index.ts
  preload/index.ts    the only bridge; contextIsolation on, node off
  renderer/           React + Zustand
    state/store.ts    documents, sessions, msg_id -> cell routing
    state/outputs.ts  iopub messages -> nbformat outputs
    components/
```

### Kernel lifecycle

`KernelSession` reserves five ports, writes a connection file with a fresh HMAC
key, spawns the kernelspec's `argv`, and connects DEALER/SUB sockets. It then
polls `kernel_info_request` until the kernel answers — this is what guarantees
the SUB subscription is live before the first cell runs, and without it the
first cell's output is routinely dropped.

Restart kills the process and relaunches against the *same* connection file, so
the sockets simply reconnect and the session id stays valid.

### Message routing

Main forwards every kernel message verbatim, tagged with its channel. The
renderer owns the `msg_id → cell` mapping. Because a kernel can publish output
before the IPC round-trip returns that `msg_id`, unmatched messages are parked
and replayed once the mapping registers (`store.ts`, `orphaned`).

## Design decisions worth knowing

- **Auto-save is conditional.** Edits are written 1s after you stop typing (and
  on tab close and window blur). Because a blind auto-save would silently
  clobber a file edited outside the app, the write is conditional on the file's
  mtime matching what we last read. On mismatch nothing is written, auto-save
  pauses for that file, and you're told. An explicit Save is unconditional —
  you asked for your version to win. Untitled notebooks never auto-save, since
  that would mean popping a Save dialog you didn't ask for.
- **New notebooks are untitled and in-memory.** Nothing is written to disk until
  the first save, which opens Save As (pointed at the open folder if there is
  one). This means New Notebook works with no workspace open, and never leaves
  stray `Untitled.ipynb` files behind. Their kernel runs in the open folder, or
  your home directory if there isn't one.
- **In memory, `source` is always a `string`.** On disk it's nbformat's array of
  lines. `notebook/io.ts` is the only place that knows the difference, so saves
  stay diff-clean against notebooks Jupyter wrote.
- **Editor props must stay referentially stable.** react-codemirror dispatches a
  full `StateEffect.reconfigure` whenever `extensions`, `basicSetup`, or
  `onChange` change identity — so an inline arrow or object literal rebuilds the
  language parser, theme, and keymaps on *every keystroke*. `Editor.tsx` reads
  its handlers through a ref and uses module-level setup constants; `Cell` is
  memoized on primitive props rather than taking the whole document. Verified at
  53 keystrokes → 0 editor rebuilds.
- **Output is untrusted.** Everything HTML-ish goes through DOMPurify, and the
  renderer runs under a restrictive CSP with `nodeIntegration: false`.
- **Cross-notebook search exists but is not surfaced.** `workspace.search` walks
  the tree, parses every notebook, and matches cell *sources* — so hits point at
  a jumpable cell and stored outputs never produce false positives. It's wired
  through IPC and tested, but there is no UI for it right now. Re-surfacing it
  is one component; deleting it is `fs.ts` plus one IPC handler.

## Keyboard

`Mod` is Cmd on macOS, Ctrl elsewhere.

| | |
|---|---|
| `Mod P` | Go to file |
| `Shift Mod P` | Command palette |
| `Mod Enter` / `Shift Enter` | Run / run and advance |
| `Alt Enter` | Run and insert a cell below |
| `Shift Mod A` / `Shift Mod B` | Insert code cell above / below |
| `Shift Mod M` | Insert markdown cell below |
| `Escape` | Leave the editor for command mode |
| `A` / `B` | Insert cell above / below |
| `M` / `Y` / `R` | To markdown / code / raw |
| `D D` | Delete cell |
| `J` / `K` | Move selection |

The single-key commands apply after `Escape`, when focus is on the notebook
surface rather than inside an editor. New cells take focus immediately, so you
can insert and start typing without reaching for the mouse.

`Shift Mod A`/`B` mirror the single-key `a`/`b`; `Cmd Shift Up`/`Down` were
avoided because they shadow macOS's select-to-start/end inside the editor.

## Appearance

Light only, and deliberately without an accent colour — hierarchy comes from
weight, size, and value contrast. The one exception is error output (stderr and
tracebacks), where red carries information rather than decoration. Kernel state
is a grey value ramp next to a written label (`Idle` / `Busy` / `Dead`) rather
than a colour code.

Shortcuts render as spelled-out chips (`Cmd` `N`) instead of the macOS glyphs,
which render inconsistently across system UI fonts at small sizes.

## Development notes

`src/renderer/harness.html` loads the renderer in a plain browser against a
stubbed `window.herse`, so UI work doesn't require booting Electron:

```bash
npm run dev   # then open http://localhost:5173/harness.html
```

It's excluded from production builds (only `index.html` is a rollup input).

## Not built yet

- Reloading a file that changed on disk. Auto-save detects the conflict and
  stops, but there's no "reload from disk" command yet — close and reopen.
- Unsaved-changes confirmation when closing an untitled tab
- Completion and inspection are wired through IPC but not surfaced in the editor
- Remote / already-running `jupyter server` connections; the `KernelManager`
  boundary is where that would slot in
- ipywidgets (comm messages are forwarded but not interpreted)
- Multi-window, notebook diffing, variable explorer
