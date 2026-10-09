#!/usr/bin/env python3
"""Records what an agent CLI sends to its model endpoint; answers every request with an
error, so nothing is ever forwarded and no key is needed. Usage: cli-traffic-stub.py PORT LOG"""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = sys.argv[2]
SKIP = {"host", "content-length", "accept-encoding", "connection"}


class Handler(BaseHTTPRequestHandler):
    def _record(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            body = json.loads(raw)
            shape = {
                "model": body.get("model"),
                "stream": body.get("stream"),
                "max_tokens": body.get("max_tokens"),
                "keys": sorted(body) if isinstance(body, dict) else type(body).__name__,
            }
        except ValueError:
            shape = {"raw_bytes": len(raw)}
        entry = {
            "method": self.command,
            "path": self.path,
            "headers": {k.lower(): ("<redacted>" if k.lower() in ("x-api-key", "authorization") else v)
                        for k, v in self.headers.items() if k.lower() not in SKIP},
            "body": shape,
        }
        with open(LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")
        err = json.dumps({"type": "error", "error": {"type": "api_error", "message": "stub"}}).encode()
        self.send_response(500)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(err)))
        self.end_headers()
        self.wfile.write(err)

    do_GET = do_POST = do_PUT = do_HEAD = do_DELETE = _record

    def log_message(self, *a):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
