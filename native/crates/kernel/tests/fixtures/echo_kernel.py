"""A minimal single-threaded Jupyter kernel on raw pyzmq, for measuring the
client and transport without IPython's own per-execution cost.

`execute_request` publishes busy / execute_input / execute_result(code) / idle
and replies immediately. Code `skip-idle` omits the idle status (simulating a
dropped IOPub message). Usage: python echo_kernel.py [--bad-key] -f <file>
(`--bad-key` signs with the wrong key, so every message must be rejected).
"""

import datetime
import hashlib
import hmac
import json
import sys
import uuid

import zmq

cfg = json.load(open(sys.argv[sys.argv.index("-f") + 1]))
ctx = zmq.Context()


def bind(kind, port):
    s = ctx.socket(kind)
    s.bind(f"tcp://127.0.0.1:{port}")
    return s


shell = bind(zmq.ROUTER, cfg["shell_port"])
control = bind(zmq.ROUTER, cfg["control_port"])
stdin = bind(zmq.ROUTER, cfg["stdin_port"])
iopub = bind(zmq.PUB, cfg["iopub_port"])
hb = bind(zmq.ROUTER, cfg["hb_port"])
key = cfg["key"].encode() + (b"-wrong" if "--bad-key" in sys.argv else b"")
session = str(uuid.uuid4())
count = 0


def send(sock, idents, msg_type, parent, content):
    header = json.dumps(
        {
            "msg_id": str(uuid.uuid4()),
            "session": session,
            "username": "echo",
            "date": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "msg_type": msg_type,
            "version": "5.3",
        }
    ).encode()
    parts = [header, parent, b"{}", json.dumps(content).encode()]
    mac = hmac.new(key, digestmod=hashlib.sha256)
    for p in parts:
        mac.update(p)
    sock.send_multipart(idents + [b"<IDS|MSG>", mac.hexdigest().encode()] + parts)


poller = zmq.Poller()
for s in (shell, control, hb):
    poller.register(s, zmq.POLLIN)

while True:
    for sock, _ in poller.poll():
        frames = sock.recv_multipart()
        if sock is hb:
            hb.send_multipart(frames)
            continue
        i = frames.index(b"<IDS|MSG>")
        idents, header = frames[:i], frames[i + 2]
        msg_type = json.loads(header)["msg_type"]
        content = json.loads(frames[i + 5])
        send(iopub, [b"status"], "status", header, {"execution_state": "busy"})
        if msg_type == "kernel_info_request":
            send(sock, idents, "kernel_info_reply", header, {
                "status": "ok", "protocol_version": "5.3", "implementation": "echo",
                "implementation_version": "0", "banner": "",
                "language_info": {"name": "echo", "version": "0", "file_extension": ".txt"},
            })
        elif msg_type == "execute_request":
            count += 1
            send(iopub, [b"execute_input"], "execute_input", header,
                 {"code": content["code"], "execution_count": count})
            send(iopub, [b"execute_result"], "execute_result", header,
                 {"execution_count": count, "data": {"text/plain": content["code"]}, "metadata": {}})
            send(sock, idents, "execute_reply", header, {"status": "ok", "execution_count": count})
        elif msg_type == "shutdown_request":
            send(sock, idents, "shutdown_reply", header, {"status": "ok", "restart": False})
            send(iopub, [b"status"], "status", header, {"execution_state": "idle"})
            sys.exit(0)
        if not (msg_type == "execute_request" and content.get("code") == "skip-idle"):
            send(iopub, [b"status"], "status", header, {"execution_state": "idle"})
