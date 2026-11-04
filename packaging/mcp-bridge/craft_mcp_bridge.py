#!/usr/bin/env python3
"""Craft MCP HTTP bridge: GridCraft (Excel) and WordCraft (Word) for Gemini Scribe and other MCP
clients, over MCP's Streamable HTTP transport.

Each Craft app has an MCP server that speaks JSON-RPC over stdio (`gridcraft-cli mcp`,
`wordcraft-cli mcp`). This bridge serves them over HTTP, one endpoint per app:

    http://HOST:PORT/mcp        GridCraft (spreadsheets: .xlsx, .csv ...)  - unchanged
    http://HOST:PORT/word/mcp   WordCraft (documents: .docx, .odt, .rtf, .md, PDF export ...)

Every MCP session (each client connection, identified by the Mcp-Session-Id header) gets its own
app process, so two clients never edit each other's open document. Sessions idle for
CRAFT_MCP_IDLE_MINUTES are closed.

Settings (environment variables; the defaults are in the CONFIG section below):
    CRAFT_MCP_HOST, CRAFT_MCP_PORT        where the bridge listens (GRIDCRAFT_HOST and
                                          GRIDCRAFT_PORT, the older names, still work)
    GRIDCRAFT_BINARY                      path to gridcraft-cli
    WORDCRAFT_BINARY                      path to wordcraft-cli
    WORDCRAFT_DOCS                        folder WordCraft works in: relative paths ("Notes/a.docx")
                                          resolve against it, and WordCraft may only read and write
                                          files inside it. Leave empty for no confinement (relative
                                          paths then resolve against the bridge's folder).
    WORDCRAFT_AUTHOR                      name on WordCraft's tracked changes and comments
    CRAFT_MCP_TIMEOUT                     seconds to wait for an app's reply (default 120)
    CRAFT_MCP_IDLE_MINUTES                close sessions unused this long (default 30)

An endpoint whose app is missing answers 503 with the reason; the other keeps working.
"""

import json
import os
import queue
import secrets
import subprocess
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ---------------------------------------------------------------------------------------------
# CONFIG
# ---------------------------------------------------------------------------------------------

# The GRIDCRAFT_* names are the ones the GridCraft-only bridge used, so its service settings keep
# working unchanged; the CRAFT_MCP_* names win when both are set.
HOST = os.environ.get("CRAFT_MCP_HOST", os.environ.get("GRIDCRAFT_HOST", "0.0.0.0"))
PORT = int(os.environ.get("CRAFT_MCP_PORT", os.environ.get("GRIDCRAFT_PORT", "8090")))
MAX_BODY = 16 * 1024 * 1024
REPLY_TIMEOUT = float(os.environ.get("CRAFT_MCP_TIMEOUT", "120"))
IDLE_SECONDS = float(os.environ.get("CRAFT_MCP_IDLE_MINUTES", "30")) * 60
KEEPALIVE_SECONDS = 15

GRIDCRAFT_BINARY = os.environ.get("GRIDCRAFT_BINARY", "/opt/gridcraft/bin/gridcraft-cli")
WORDCRAFT_BINARY = os.environ.get("WORDCRAFT_BINARY", "/opt/wordcraft/bin/wordcraft-cli")
WORDCRAFT_DOCS = os.environ.get("WORDCRAFT_DOCS", "")
WORDCRAFT_AUTHOR = os.environ.get("WORDCRAFT_AUTHOR", "Gemini (Obsidian)")


class Backend:
    """One Craft app's MCP server: how to start it, and whether it can be started."""

    def __init__(self, name, binary, args, cwd=None):
        self.name = name
        self.binary = binary
        self.args = args
        self.cwd = cwd or None

    def problem(self):
        """Why this app can't be started, or None when it can."""
        if not os.path.isfile(self.binary) or not os.access(self.binary, os.X_OK):
            return f"{self.name} binary is missing or not executable: {self.binary}"
        if self.cwd and not os.path.isdir(self.cwd):
            return f"{self.name} folder does not exist: {self.cwd}"
        return None

    def command(self):
        return [self.binary, *self.args]


def wordcraft_args():
    args = ["mcp"]
    if WORDCRAFT_AUTHOR:
        args += ["--author", WORDCRAFT_AUTHOR]
    if WORDCRAFT_DOCS:
        # Read and write only inside the documents folder (links that lead out are refused).
        args += ["--automation-read-root", WORDCRAFT_DOCS, "--automation-write-root", WORDCRAFT_DOCS]
    return args


# Endpoint path -> app. "/mcp" stays GridCraft so existing client settings keep working.
BACKENDS = {
    "/mcp": Backend("GridCraft", GRIDCRAFT_BINARY, ["mcp"]),
    "/word/mcp": Backend("WordCraft", WORDCRAFT_BINARY, wordcraft_args(), cwd=WORDCRAFT_DOCS),
}

# ---------------------------------------------------------------------------------------------
# Sessions: one app process per MCP session
# ---------------------------------------------------------------------------------------------


class Session:
    def __init__(self, backend):
        self.id = secrets.token_urlsafe(24)
        self.backend = backend
        self.last_used = time.monotonic()
        self.proc = subprocess.Popen(
            backend.command(),
            cwd=backend.cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            # Keep service logs free of document data: the app's own log lines are dropped.
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self.replies = queue.Queue()
        self.lock = threading.Lock()
        self.closed = False
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        try:
            for line in self.proc.stdout:
                line = line.strip()
                if line:
                    self.replies.put(line)
        finally:
            self.closed = True
            self.replies.put(None)

    def send(self, message):
        """Send one JSON-RPC message; return the reply to a request, None for a notification."""
        self.last_used = time.monotonic()
        with self.lock:
            if self.closed or self.proc.poll() is not None:
                raise RuntimeError(f"{self.backend.name} MCP process is not running")

            self.proc.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
            self.proc.stdin.flush()

            if not isinstance(message, dict) or "id" not in message or "method" not in message:
                return None

            deadline = time.monotonic() + REPLY_TIMEOUT
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(f"{self.backend.name} did not answer within {REPLY_TIMEOUT:.0f} s")
                try:
                    reply = self.replies.get(timeout=remaining)
                except queue.Empty:
                    continue

                if reply is None:
                    raise RuntimeError(f"{self.backend.name} MCP process exited")

                try:
                    value = json.loads(reply)
                except json.JSONDecodeError:
                    continue  # not protocol output

                if isinstance(value, dict) and value.get("id") == message["id"]:
                    self.last_used = time.monotonic()
                    return value
                # The Craft servers send no requests of their own; anything else is a stale
                # reply to a request that timed out.

    def close(self):
        if not self.closed:
            self.closed = True
            self.proc.terminate()
            try:
                self.proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.proc.kill()


sessions = {}
sessions_lock = threading.Lock()


def new_session(backend):
    session = Session(backend)
    with sessions_lock:
        sessions[session.id] = session
    return session


def get_session(sid, backend):
    """The session `sid` of this endpoint's app (a session id from the other endpoint is not)."""
    with sessions_lock:
        session = sessions.get(sid)
    if session is not None and session.backend is backend and not session.closed:
        return session
    return None


def drop_session(sid):
    with sessions_lock:
        session = sessions.pop(sid, None)
    if session:
        session.close()


def reap_idle_sessions():
    """Close sessions no client has used for IDLE_SECONDS (a client that went away without
    ending its session would otherwise keep its app process forever)."""
    while True:
        time.sleep(60)
        now = time.monotonic()
        with sessions_lock:
            stale = [sid for sid, s in sessions.items() if s.closed or now - s.last_used > IDLE_SECONDS]
        for sid in stale:
            drop_session(sid)


# ---------------------------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------------------------


def rpc_error(code, message):
    return {"jsonrpc": "2.0", "id": None, "error": {"code": code, "message": message}}


class Handler(BaseHTTPRequestHandler):
    server_version = "Craft-MCP-Bridge/2.0"

    def log_message(self, fmt, *args):
        # Do not log URLs, headers, or request content; keep service logs secret-free.
        return

    def _backend(self):
        path = self.path.split("?", 1)[0].rstrip("/")
        return BACKENDS.get(path)

    def _headers(self, status, content_type="application/json", sid=None):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Expose-Headers", "Mcp-Session-Id")
        if sid:
            self.send_header("Mcp-Session-Id", sid)
        self.end_headers()

    def _json(self, status, value, sid=None):
        data = json.dumps(value, separators=(",", ":")).encode()
        self._headers(status, sid=sid)
        self.wfile.write(data)

    def do_OPTIONS(self):
        self.send_response(HTTPStatus.NO_CONTENT)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header(
            "Access-Control-Allow-Headers",
            "Content-Type, Accept, Authorization, Mcp-Session-Id, Mcp-Protocol-Version, Last-Event-ID",
        )
        self.send_header("Access-Control-Allow-Methods", "POST, GET, DELETE, OPTIONS")
        self.end_headers()

    def do_POST(self):
        backend = self._backend()
        if backend is None:
            self._json(404, {"error": "not found"})
            return

        problem = backend.problem()
        if problem:
            self._json(503, rpc_error(-32603, problem))
            return

        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > MAX_BODY:
                raise ValueError("invalid request size")

            message = json.loads(self.rfile.read(length))
            if not isinstance(message, (dict, list)):
                raise ValueError("JSON-RPC message must be an object or batch")

            sid = self.headers.get("Mcp-Session-Id")
            session = get_session(sid, backend) if sid else None
            if session is None:
                session = new_session(backend)
                sid = session.id

            if isinstance(message, list):
                replies = [r for r in (session.send(item) for item in message) if r is not None]
                response = replies if replies else None
            else:
                response = session.send(message)

            if response is None:
                self._headers(202, sid=sid)
            else:
                self._json(200, response, sid=sid)

        except (ValueError, json.JSONDecodeError) as e:
            self._json(400, rpc_error(-32600, str(e)))
        except Exception as e:  # the app stopped or timed out: say so, keep serving
            self._json(500, rpc_error(-32603, str(e)))

    def do_GET(self):
        backend = self._backend()
        if backend is None:
            self._json(404, {"error": "not found"})
            return

        sid = self.headers.get("Mcp-Session-Id")
        session = get_session(sid, backend) if sid else None
        if session is None:
            self._json(404, {"error": "unknown MCP session"})
            return

        # Streamable HTTP permits an SSE GET for server-initiated messages. The Craft servers send
        # none; keep the stream open with periodic comments while the session lives.
        self._headers(200, "text/event-stream", sid=sid)
        try:
            while not session.closed:
                self.wfile.write(b": keepalive\n\n")
                self.wfile.flush()
                session.last_used = time.monotonic()
                time.sleep(KEEPALIVE_SECONDS)
        except (BrokenPipeError, ConnectionResetError, OSError):
            return

    def do_DELETE(self):
        backend = self._backend()
        if backend is None:
            self._json(404, {"error": "not found"})
            return
        sid = self.headers.get("Mcp-Session-Id")
        if sid and get_session(sid, backend):
            drop_session(sid)
        self._headers(204)


if __name__ == "__main__":
    available = []
    for path, backend in BACKENDS.items():
        problem = backend.problem()
        if problem:
            print(f"  {path}: unavailable ({problem})", flush=True)
        else:
            available.append(path)
            print(f"  {path}: {backend.name}", flush=True)
    if not available:
        raise SystemExit("No Craft MCP app can be started; check the binary paths.")

    threading.Thread(target=reap_idle_sessions, daemon=True).start()
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    server.daemon_threads = True
    print(f"Craft MCP HTTP bridge listening on {HOST}:{PORT}", flush=True)
    server.serve_forever()
