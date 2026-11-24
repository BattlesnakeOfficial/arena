#!/usr/bin/env python3
"""A minimal Battlesnake for local testing: GET / returns metadata,
POST /start and /end return {}, POST /move always moves "up" (the snake
runs into the top wall within a few turns, so games finish quickly).
Python 3 standard library only. Usage: python3 minimal_snake.py [port]"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    # BaseHTTPRequestHandler logs every request to stderr; silence it so a
    # backgrounded snake does not interleave with the CLI's output.
    def log_message(self, *args):
        pass

    def _send(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._send({"apiversion": "1", "color": "#888800", "head": "safe", "tail": "bolt"})

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path.endswith("/move"):
            self._send({"move": "up"})
        else:
            self._send({})


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8085
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
