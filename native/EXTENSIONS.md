# Herse extensions

An extension adds to Herse from outside it, in any language: commands in
⌘K, drawings of output types Herse doesn't know, and reactions to what
happens in a notebook. It can read and change the notebook in front through
the same operations the assistant (⌘I) uses.

There is no store and no sandbox. An extension is a folder you put in place,
and once you allow it, Herse runs it as you, with your files and network.
Only allow extensions you trust.

## Install

Put the extension's folder in

```
~/Library/Application Support/Herse/extensions/
```

(⌘K ▸ Open Extensions Folder opens it), then ⌘K ▸ Reload Extensions.
An extension that can't load says why, in a banner above the page.

Nothing in the folder runs until you've allowed it. The first time an
extension would start, a banner above the page asks, with the exact command
and where it runs:

> **Notebook Tools wants to run: python3 main.py**
> in ~/Library/Application Support/Herse/extensions/notebook-tools

**Allow** starts it (and runs the command you chose, if that's what asked);
**Don't Allow** keeps it from running; **Show in Finder** opens its folder,
to look at it first. Closing the banner answers nothing: it asks again the
next time it's needed. An extension that listens for events asks when Herse
starts; the others ask when first used. Until it's allowed:

- its ⌘K commands are listed ("Extension · asks first") and ask when chosen;
- outputs it draws show "Notebook Tools draws this — Allow?" with the command
  it would run and an Allow button;
- it hears no events.

The answer is kept in `~/Library/Application Support/Herse/settings.json`
(`extension_trust`), by the extension's folder and a SHA-256 of its manifest
plus each file in its folder that `run` names (`main.py` here). Change
either, and it asks again. Other files it loads aren't covered: allowing an
extension trusts its folder.

Settings ▸ Extensions… (or ⌘K ▸ Extensions…) lists each extension, what it
runs, and whether it's Allowed or Not allowed; choosing one revokes it (it
stops at once) or allows it.

[examples/extensions/notebook-tools](examples/extensions/notebook-tools) is a
complete example in Python with no dependencies: two commands, a renderer and
an event listener. To try it, copy the folder into place:

```bash
mkdir -p ~/Library/"Application Support"/Herse/extensions && cp -R examples/extensions/notebook-tools ~/Library/"Application Support"/Herse/extensions/
```

## The manifest

`herse-extension.json` in the extension's folder (changing it asks for
permission again):

```json
{
  "id": "notebook-tools",
  "name": "Notebook Tools",
  "version": "0.1.0",
  "api": 1,
  "run": ["python3", "main.py"],
  "commands": [{ "id": "word-count", "title": "Count Words" }],
  "renderers": [{ "mime": "application/vnd.herse.bars+json", "title": "Bar chart" }],
  "events": ["notebook.saved"]
}
```

| Field | |
|---|---|
| `id` | Unique; letters, digits, `-`, `_`, `.`. |
| `name`, `version` | Shown in ⌘K and Settings. |
| `api` | The protocol version it was written for. Herse speaks `1`; it refuses an extension asking for more. |
| `run` | The command that starts it. A first element that names a file in the folder runs that file; otherwise it's looked up on your shell's `PATH` (`python3`, `node`, `deno`…). It runs in its folder with your login shell's environment, plus `HERSE_EXTENSION_API`. |
| `commands` | Each becomes a ⌘K row, "Name: Title". |
| `renderers` | Output MIME types it draws. Herse asks it only for types it doesn't draw itself. |
| `events` | Which events to hear. Listing any starts the extension with Herse (once allowed; until then it asks at launch); otherwise it starts on first use. |

## The protocol

JSON-RPC 2.0 over the process's stdin and stdout: one JSON object per line,
UTF-8, in both directions. Requests carry an `id` and get exactly one answer
(a `result` or an `error` with a `message`); notifications have no `id`.
Herse waits 30 s for an answer. Write logs to stderr: when the extension
exits, its last lines of stderr are part of the error Herse shows.

Answer requests in order. While waiting for one of your own requests to be
answered, queue anything else Herse sends and handle it afterwards (the
example's `call` does this).

### Herse asks the extension

**`initialize`**: sent first, once.
`{"api": 1, "herse": "0.1.0", "extension": {"id": "…", "folder": "…"}}`.
Answer anything (`{}`).

**`command.run`**: a command was chosen in ⌘K.
`{"command": "word-count", "notebook": {"path": "…", "title": "…", "cells": 12}}`
(`notebook` is `null` with none open). Answer `{"message": "…"}` to show a
message in the status line, or `{}`. An error is shown above the page, where
it can be read and copied.

**`render`**: draw an output.
`{"mime": "application/vnd.herse.bars+json", "data": <the output's data for that type>, "dark": false}`.
Answer one of:

| Answer | Drawn as |
|---|---|
| `{"svg": "<svg …>"}` | a vector image, at the SVG's `width` × `height` |
| `{"png": "<base64>"}` | an image, at half its pixel size (draw at 2× for Retina) |
| `{"markdown": "…"}` | Markdown, with math |
| `{"text": "…"}` | plain text |

Drawings are cached per output, appearance and data: the same output isn't
asked for twice. Draw for `dark` so it sits on the page (see DESIGN.md's ink
and dew colours).

**`event`** (a notification): `{"name": "…", "notebook": {…}, "detail": …}`.

| Event | `detail` |
|---|---|
| `notebook.opened` | `null` |
| `notebook.saved` | `null` |
| `cell.executed` | `{"cell": 3, "status": "ok"}` (or `"error"`, `"aborted"`) |

### The extension asks Herse

These act on the notebook in front (the one used last). Cells are numbered
from 1, as Herse shows them.

| Method | Params | Result |
|---|---|---|
| `notebook.get` | `{"outputs": false}` | `{"path", "title", "kernel", "cells": [{"number", "type", "source", "executionCount", "outputs"?}]}`; outputs in nbformat's shape when asked for |
| `notebook.readCells` | `{"from": 2, "to": 4}` | `{"text"}`: source and outputs as text |
| `notebook.editCell` | `{"cell": 3, "source": "…"}` | `{"text"}` |
| `notebook.insertCell` | `{"at": 1, "type": "markdown", "source": "…"}` | `{"text"}` |
| `notebook.deleteCell` | `{"cell": 3}` | `{"text"}` |
| `notebook.runCells` | `{"from": 1, "to": 3, "timeout_seconds": 180}` | `{"text"}`: their outputs, once they finish |
| `kernel.evaluate` | `{"code": "df.shape"}` | `{"text"}`: what it printed or returned; runs in the kernel without adding a cell |
| `ui.message` | `{"text": "…"}` | `null`: shown in the status line, with the extension's name |

Edits are ordinary edits: the reader sees each one (the cell scrolls into
view, marked "Name added this"), and each is one undo step.

## What's not here yet

Panels and sidebars of an extension's own, settings, keyboard shortcuts,
kernel messages, and a way to find and install extensions in the app. See
TODO.md.
