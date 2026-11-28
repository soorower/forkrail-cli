import os
from http.server import BaseHTTPRequestHandler


class handler(BaseHTTPRequestHandler):
    def do_GET(self):
        ok = self.headers.get("Authorization") == f"Bearer {os.environ.get('CRON_SECRET')}"
        print(f"cron ran: authorized={ok} schedule={self.headers.get('x-vercel-cron-schedule')}")
        self.send_response(200 if ok else 401)
        self.send_header("Content-Type", "text/plain")
        self.end_headers()
        self.wfile.write(b"ok" if ok else b"unauthorized")
