# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Minimal HTTP log receiver standing in for a SIEM's ingestion endpoint (Splunk HEC, Elastic/
# Fluent Bit HTTP input, a generic webhook collector) — stdlib only, no dependencies. Accepts any
# POST, logs the JSON body to stdout (so `kubectl logs` shows exactly what Atlas exported), and
# always returns 200. This is what ATLAS_AUDIT_EXPORT_URL points at in the lab; a real deployment
# points it at the bank's actual ingestion endpoint instead.
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            parsed = json.loads(body)
            n = len(parsed.get("audit_logs", []))
            print(f"[siem-lab] received POST {self.path} with {n} audit_logs entr{'y' if n == 1 else 'ies'}:", flush=True)
            print(json.dumps(parsed, indent=2), flush=True)
        except json.JSONDecodeError:
            print(f"[siem-lab] received POST {self.path} with non-JSON body ({len(body)} bytes)", flush=True)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')

    def log_message(self, format, *args):  # noqa: A002 - matches BaseHTTPRequestHandler's signature
        pass  # keep stdout to just the payload logging above


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    print(f"[siem-lab] listening on :{port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
