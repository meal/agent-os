#!/usr/bin/env python3
"""Answers POST /v1/messages with canned SSE files in order (the last repeats), logs the
interesting body fields. Usage: cli-scripted-stub.py PORT LOG WORKDIR SSE..."""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT, LOG, WORKDIR, FILES = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4:]
calls = []


class H(BaseHTTPRequestHandler):
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length") or 0))
        path = self.path.split("?")[0]
        entry = {"path": self.path, "n": len(calls)}
        if path == "/v1/messages":
            body = json.loads(raw)
            entry.update(model=body.get("model"), stream=body.get("stream"), max_tokens=body.get("max_tokens"),
                         thinking=body.get("thinking"), output_config=body.get("output_config"),
                         context_management=body.get("context_management"),
                         tools=[t.get("name") for t in body.get("tools", [])][:40])
            data = open(FILES[min(len(calls), len(FILES) - 1)], "rb").read().replace(b"/WORKDIR", WORKDIR.encode())
            calls.append(1)
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        else:
            self.send_response(404); self.send_header("content-length", "0"); self.end_headers()
        open(LOG, "a").write(json.dumps(entry) + "\n")

    do_GET = do_POST

    def log_message(self, *a):
        pass


HTTPServer(("127.0.0.1", PORT), H).serve_forever()
