#!/usr/bin/env python3
"""Run security-smoke through a loopback proxy that removes exactly one header.

Usage: python3 scripts/tests/test-live-smoke.py [http://127.0.0.1:18018]
The positive run uses the same real server before introducing the fault.
"""
import http.client
import http.server
import os
import pathlib
import subprocess
import sys
import threading
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parents[2]
url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18018"
parsed = urllib.parse.urlsplit(url)
if parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or not parsed.port:
    raise SystemExit("Expected an explicit loopback HTTP port")


class Proxy(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def do_GET(self):
        connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=10)
        connection.request("GET", self.path)
        response = connection.getresponse()
        self.send_response_only(response.status)
        for key, value in response.getheaders():
            if key.lower() not in ("x-content-type-options", "transfer-encoding", "connection"):
                self.send_header(key, value)
        self.end_headers()
        self.wfile.write(response.read())
        connection.close()


subprocess.run(["bash", "scripts/security-smoke.sh"], cwd=ROOT,
               env={**os.environ, "YHOTOS_BENCH_URL": url}, check=True)
with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy) as server:
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    result = subprocess.run(["bash", "scripts/security-smoke.sh"], cwd=ROOT,
        env={**os.environ, "YHOTOS_BENCH_URL": f"http://127.0.0.1:{server.server_port}"},
        capture_output=True, text=True)
    server.shutdown()
    thread.join()
    if result.returncode == 0 or not result.stderr.rstrip().endswith("headers for /"):
        raise SystemExit("Smoke script failed to detect the removed header")
    print("Removed X-Content-Type-Options: smoke script rejected the real response as expected")
