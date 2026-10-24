"""Notebook Tools: an example Herse extension, standard library only.

Herse starts this program and speaks JSON-RPC 2.0 with it, one JSON message
per line on stdin and stdout. See EXTENSIONS.md in the Herse repository.
Anything printed to stderr shows up when something goes wrong.
"""

import html
import json
import re
import sys

_next_id = 0
_queued = []  # messages that arrived while waiting for an answer


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def call(method, params=None):
    """Ask Herse something and wait for the answer."""
    global _next_id
    _next_id += 1
    my_id = f"ext-{_next_id}"
    send({"jsonrpc": "2.0", "id": my_id, "method": method, "params": params or {}})
    for line in sys.stdin:
        message = json.loads(line)
        if message.get("id") == my_id and "method" not in message:
            if "error" in message:
                raise RuntimeError(message["error"]["message"])
            return message.get("result")
        _queued.append(message)
    raise RuntimeError("Herse closed the connection")


def word_count(_notebook):
    nb = call("notebook.get")
    words = sum(len(re.findall(r"\w+", c["source"])) for c in nb["cells"] if c["type"] == "markdown")
    return {"message": f"{words:,} words of prose in {len(nb['cells'])} cells"}


def table_of_contents(_notebook):
    nb = call("notebook.get")
    lines = []
    for cell in nb["cells"]:
        if cell["type"] != "markdown" or cell["source"].startswith("**Contents**"):
            continue
        for match in re.finditer(r"^(#{1,3})\s+(.+)$", cell["source"], re.M):
            depth, title = len(match.group(1)), match.group(2).strip()
            lines.append(f"{'  ' * (depth - 1)}- {title}")
    if not lines:
        return {"message": "No headings to list"}
    call("notebook.insertCell", {"at": 1, "type": "markdown", "source": "**Contents**\n\n" + "\n".join(lines)})
    return {"message": f"Listed {len(lines)} headings in cell 1"}


def render_bars(data, dark):
    """application/vnd.herse.bars+json: {"labels": [...], "values": [...]} as SVG."""
    labels, values = data.get("labels", []), data.get("values", [])
    ink = "#e8e4dc" if dark else "#1c1a17"
    bar = "#5cc8be" if dark else "#0b7a75"
    width, row = 420, 26
    top = max(values) if values else 1
    rows = []
    for i, (label, value) in enumerate(zip(labels, values)):
        y = i * row
        w = 0 if top == 0 else (width - 140) * value / top
        rows.append(
            f'<text x="0" y="{y + 17}" font-family="Helvetica Neue" font-size="12" fill="{ink}">{html.escape(str(label))}</text>'
            f'<rect x="100" y="{y + 5}" width="{w:.1f}" height="16" rx="3" fill="{bar}"/>'
            f'<text x="{106 + w:.1f}" y="{y + 17}" font-family="Helvetica Neue" font-size="11" fill="{ink}" opacity="0.7">{value}</text>'
        )
    height = max(row * len(rows), row)
    return {"svg": f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">{"".join(rows)}</svg>'}


COMMANDS = {"word-count": word_count, "toc": table_of_contents}


def handle(message):
    method, params = message.get("method"), message.get("params") or {}
    if method == "initialize":
        return {}
    if method == "command.run":
        return COMMANDS[params["command"]](params.get("notebook"))
    if method == "render":
        return render_bars(params["data"], params.get("dark", False))
    if method == "event" and params.get("name") == "notebook.saved":
        cells = (params.get("notebook") or {}).get("cells", 0)
        call("ui.message", {"text": f"saved, {cells} cells"})
        return None
    raise ValueError(f"unknown method {method}")


def answer(message):
    try:
        result = handle(message)
        if "id" in message:
            send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    except Exception as error:  # report it to Herse rather than dying
        print(f"notebook-tools: {error}", file=sys.stderr, flush=True)
        if "id" in message:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32000, "message": str(error)}})


def main():
    for line in sys.stdin:
        answer(json.loads(line))
        while _queued:
            answer(_queued.pop(0))


if __name__ == "__main__":
    main()
