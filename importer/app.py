"""Local GUI for importing Claude Code sessions into Envoy.

Run it:

    zivon-envoy/.venv/bin/python zivon-desktop/importer/app.py

…then open the printed http://127.0.0.1:<port> in your browser. Pick a target
envoy, select the sessions (all selected by default), click Import, and watch
the progress. Each imported session prints an Envoy URL you can open and
continue.

Stdlib-only HTTP server + pymongo (from the envoy venv). No framework.
"""

from __future__ import annotations

import json
import socket
import threading
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import ingest
import parser

HERE = Path(__file__).resolve().parent

# Parsed sessions cached in-memory so import reuses turns without re-reading.
_SESSIONS: dict[str, dict] = {}
_LOCK = threading.Lock()


def _load_sessions() -> list[dict]:
    with _LOCK:
        found = parser.discover_sessions()
        _SESSIONS.clear()
        for s in found:
            _SESSIONS[s["session_id"]] = s
        return found


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # quieter console
        pass

    # -- helpers -----------------------------------------------------------
    def _send_json(self, obj, status=200):
        body = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_html(self, html: str):
        body = html.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # -- routes ------------------------------------------------------------
    def do_GET(self):
        if self.path == "/" or self.path.startswith("/index"):
            self._send_html((HERE / "index.html").read_text())
            return
        if self.path == "/api/sessions":
            try:
                found = _load_sessions()
                self._send_json({"sessions": parser.session_summaries(found)})
            except Exception as e:
                self._send_json({"error": str(e)}, 500)
            return
        if self.path == "/api/targets":
            try:
                client = ingest.get_client()
                orgs = []
                for slug in ingest.list_orgs(client):
                    orgs.append({"slug": slug, "envoys": ingest.list_envoys(client, slug)})
                client.close()
                self._send_json({"orgs": orgs})
            except Exception as e:
                self._send_json({"error": str(e)}, 500)
            return
        self._send_json({"error": "not found"}, 404)

    def do_POST(self):
        if self.path != "/api/import":
            self._send_json({"error": "not found"}, 404)
            return
        length = int(self.headers.get("Content-Length", 0))
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            self._send_json({"error": "bad json"}, 400)
            return

        org = body.get("org")
        envoy_id = body.get("envoy_id")
        session_ids = body.get("session_ids", [])
        if not org or not envoy_id or not session_ids:
            self._send_json({"error": "org, envoy_id and session_ids required"}, 400)
            return

        # Stream NDJSON progress.
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def emit(obj):
            try:
                self.wfile.write((json.dumps(obj) + "\n").encode("utf-8"))
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                raise

        try:
            client = ingest.get_client()
            envoy = next(
                (e for e in ingest.list_envoys(client, org) if e["envoy_id"] == envoy_id),
                None,
            )
            if envoy is None:
                emit({"type": "error", "message": "envoy not found"})
                return

            from bson import ObjectId

            db = client[f"{org}{ingest.ORG_DB_SUFFIX}"]
            num = ingest._next_session_number(db, ObjectId(envoy_id))

            total_sessions = len(session_ids)
            for idx, sid in enumerate(session_ids):
                sess = _SESSIONS.get(sid)
                if not sess:
                    emit({"type": "session_error", "session_id": sid, "message": "not found"})
                    continue
                emit(
                    {
                        "type": "session_start",
                        "session_id": sid,
                        "index": idx + 1,
                        "total": total_sessions,
                        "turns": sess["turn_count"],
                        "project": sess["project"],
                    }
                )

                def progress(s_id, done, tot):
                    emit({"type": "turn", "session_id": s_id, "done": done, "total": tot})

                result = ingest.import_session(
                    client, org, envoy, sess, number=num, progress=progress
                )
                num += 1
                emit({"type": "session_done", "session_id": sid, **result})

            emit({"type": "all_done", "count": total_sessions})
            client.close()
        except Exception as e:
            try:
                emit({"type": "error", "message": str(e)})
            except Exception:
                pass


def _free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def main():
    port = _free_port()
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    url = f"http://127.0.0.1:{port}"
    print(f"\n  Envoy ▸ Claude Session Importer")
    print(f"  Open: {url}\n")
    try:
        webbrowser.open(url)
    except Exception:
        pass
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n  Stopped.")
        server.shutdown()


if __name__ == "__main__":
    main()
