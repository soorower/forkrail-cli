from http.server import BaseHTTPRequestHandler
import json

from api._utils import shout


class handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"function": "api/index.py", "path": self.path, "shout": shout("hi")}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        data = self.rfile.read(length)
        self.send_response(201)
        self.send_header("Content-Type", "text/plain")
        self.end_headers()
        self.wfile.write(b"got " + data)
