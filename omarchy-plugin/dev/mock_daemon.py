#!/usr/bin/env python3
"""Mock muesli-daemon for developing the Omarchy bar plugin (stdlib only).

Speaks the newline-JSON protocol from crates/core/src/ipc.rs on
$XDG_RUNTIME_DIR/muesli.sock (or $MUESLI_HOME/muesli.sock).

    python3 mock_daemon.py                 # start idle, wait for commands
    python3 mock_daemon.py --cycle 8       # idle → prompting → recording → processing → needs_name, 8 s each
    python3 mock_daemon.py --state recording

Jump to a state from another shell (mock-only request):
    python3 mock_daemon.py --set needs_name

Never run this while the real daemon is up: it refuses if the socket answers.
"""

import argparse
import json
import math
import os
import random
import signal
import socket
import socketserver
import sys
import threading
import time

STEPS = ["saved", "cleaned", "transcribed", "notes"]


def sock_path():
    home = os.environ.get("MUESLI_HOME")
    if home:
        return os.path.join(home, "muesli.sock")
    return os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "muesli.sock")


def now_ms():
    return int(time.time() * 1000)


def pipeline(done, active=True):
    status = lambda i: "done" if i < done else ("active" if i == done and active else "pending")
    return {"steps": [{"kind": k, "status": status(i), "detail": None} for i, k in enumerate(STEPS)]}


class Mock:
    def __init__(self):
        self.lock = threading.RLock()
        self.watchers = []
        self.auto_record = False
        self.session = {"kind": "idle"}
        self.processing = []
        self.needs_name = []
        self.seq = 0

    # ------------------------------------------------------------- snapshots
    def status(self):
        return {
            "session": self.session,
            "auto_record": self.auto_record,
            "processing": self.processing,
            "needs_name": self.needs_name,
            "api_key": True,
        }

    def broadcast(self, event):
        line = (json.dumps(event) + "\n").encode()
        with self.lock:
            alive = []
            for w in self.watchers:
                try:
                    w.sendall(line)
                    alive.append(w)
                except OSError:
                    pass
            self.watchers = alive

    def changed(self):
        self.broadcast({"event": "status", "data": self.status()})

    # ----------------------------------------------------------- transitions
    def new_id(self):
        self.seq += 1
        return f"mock-{int(time.time())}-{self.seq}"

    def set_state(self, name):
        with self.lock:
            if name == "idle":
                self.session = {"kind": "idle"}
                self.processing, self.needs_name = [], []
            elif name == "prompting":
                self.session = {"kind": "prompting", "app": "discord", "hint": "Hiking crew · #planning", "since": now_ms() - 1000}
            elif name == "recording":
                self.session = {
                    "kind": "recording", "meeting_id": self.new_id(), "app": "zoom",
                    "hint": "Zoom Meeting · Lena Hofer", "started_at": now_ms() - (24 * 60 + 7) * 1000, "manual": False,
                }
            elif name == "processing":
                self.session = {"kind": "idle"}
                self.processing = [{"meeting_id": self.new_id(), "title": "Zoom call · Oct 17, 14:02", "app": "zoom", "pipeline": pipeline(2)}]
            elif name == "needs_name":
                self.session = {"kind": "idle"}
                self.processing = []
                self.needs_name = [self.naming_row(self.new_id(), True)]
            elif name == "both":
                self.session = {"kind": "idle"}
                self.processing = [{"meeting_id": self.new_id(), "title": "Discord call · Oct 17, 15:30", "app": "discord", "pipeline": pipeline(2)}]
                self.needs_name = [self.naming_row(self.new_id(), True)]
            else:
                raise ValueError(f"unknown state {name}")
        self.changed()

    def naming_row(self, mid, ready):
        return {
            "meeting_id": mid,
            "title": "Zoom call · Oct 17, 14:02",
            "suggestions": ["Offline sync spec review", "Conflict handling with Marco", "Dec 12 release check-in"],
            "app": "zoom",
            "duration_ms": 37 * 60 * 1000,
            "notes_ready": ready,
        }

    def run_pipeline(self, mid, app):
        def work():
            for done in range(len(STEPS) + 1):
                with self.lock:
                    for p in self.processing:
                        if p["meeting_id"] == mid:
                            p["pipeline"] = pipeline(done)
                    if done == len(STEPS):
                        self.processing = [p for p in self.processing if p["meeting_id"] != mid]
                        self.needs_name.insert(0, self.naming_row(mid, True))
                self.changed()
                time.sleep(2.5)

        with self.lock:
            self.processing.append({"meeting_id": mid, "title": "Zoom call · Oct 17, 14:02", "app": app, "pipeline": pipeline(0)})
        threading.Thread(target=work, daemon=True).start()

    # -------------------------------------------------------------- requests
    def handle(self, req):
        cmd = req.get("cmd")
        with self.lock:
            kind = self.session["kind"]
            if cmd in ("ping", "status"):
                return {"ok": True, "data": self.status()} if cmd == "status" else {"ok": True}
            if cmd in ("record", "start"):
                if kind == "recording":
                    return {"ok": False, "error": "already recording"}
                prev = self.session
                self.session = {
                    "kind": "recording", "meeting_id": self.new_id(), "app": prev.get("app"),
                    "hint": prev.get("hint"), "started_at": now_ms(), "manual": kind == "idle",
                }
            elif cmd == "ignore":
                if kind != "prompting":
                    return {"ok": False, "error": "nothing to ignore"}
                self.session = {"kind": "idle"}
            elif cmd in ("stop", "discard"):
                if kind != "recording":
                    return {"ok": False, "error": "not recording"}
                s = self.session
                self.session = {"kind": "idle"}
                if cmd == "stop":
                    self.run_pipeline(s["meeting_id"], s.get("app"))
            elif cmd == "set_auto_record":
                self.auto_record = bool(req.get("value"))
            elif cmd in ("rename", "confirm_title"):
                mid = req.get("id")
                if not any(n["meeting_id"] == mid for n in self.needs_name):
                    return {"ok": False, "error": f"no meeting {mid}"}
                print(f"mock: {cmd} {mid} {req.get('title', '')!r}", flush=True)
                self.needs_name = [n for n in self.needs_name if n["meeting_id"] != mid]
                self.broadcast({"event": "meeting_updated", "data": {"id": mid}})
            elif cmd == "mock_set":
                threading.Thread(target=self.set_state, args=(req["state"],), daemon=True).start()
                return {"ok": True}
            elif cmd == "reload_config":
                return {"ok": True}
            else:
                return {"ok": False, "error": f"unsupported: {cmd}"}
        self.changed()
        return {"ok": True}

    def levels_loop(self):
        t = 0.0
        while True:
            time.sleep(0.125)
            t += 0.125
            if self.session["kind"] != "recording":
                continue
            speak = lambda phase: max(0.0, math.sin(t * 0.7 + phase)) * random.uniform(0.4, 1.0)
            self.broadcast({"event": "levels", "data": {"me": round(speak(0), 3), "them": round(speak(math.pi), 3)}})


MOCK = Mock()


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        for raw in self.rfile:
            try:
                req = json.loads(raw)
            except ValueError:
                self.wfile.write(b'{"ok":false,"error":"bad json"}\n')
                continue
            if req.get("cmd") == "watch":
                self.wfile.write(b'{"ok":true}\n')
                self.wfile.flush()
                self.connection.sendall((json.dumps({"event": "status", "data": MOCK.status()}) + "\n").encode())
                with MOCK.lock:
                    MOCK.watchers.append(self.connection)
                # Block until the client goes away.
                while self.rfile.read(1):
                    pass
                return
            self.wfile.write((json.dumps(MOCK.handle(req)) + "\n").encode())


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


def socket_alive(path):
    try:
        s = socket.socket(socket.AF_UNIX)
        s.connect(path)
        s.close()
        return True
    except OSError:
        return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--state", default="idle")
    ap.add_argument("--cycle", type=float, default=0, help="seconds per state; 0 = hold")
    ap.add_argument("--set", help="tell a running mock to jump to a state, then exit")
    args = ap.parse_args()
    path = sock_path()

    if args.set:
        s = socket.socket(socket.AF_UNIX)
        s.connect(path)
        s.sendall((json.dumps({"cmd": "mock_set", "state": args.set}) + "\n").encode())
        print(s.makefile().readline().strip())
        return

    if socket_alive(path):
        sys.exit(f"{path} is live (real daemon?) — refusing to start the mock")
    if os.path.exists(path):
        os.unlink(path)

    server = Server(path, Handler)
    MOCK.set_state(args.state)
    threading.Thread(target=MOCK.levels_loop, daemon=True).start()

    if args.cycle > 0:
        def cycle():
            order = ["idle", "prompting", "recording", "processing", "needs_name"]
            i = order.index(args.state) if args.state in order else 0
            while True:
                time.sleep(args.cycle)
                i = (i + 1) % len(order)
                print("mock: →", order[i], flush=True)
                MOCK.set_state(order[i])
        threading.Thread(target=cycle, daemon=True).start()

    def on_term(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, on_term)
    print(f"mock muesli-daemon on {path}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        if os.path.exists(path):
            os.unlink(path)


if __name__ == "__main__":
    main()
