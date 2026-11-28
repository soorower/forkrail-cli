#!/usr/bin/env python3
"""Can one user, or one project, see another's data through forkrail? End-to-end, on a local
router with workers as local processes (the runtime and its Node shim, as on Lambda):

- two projects (`alpha`, `beta`), each its own deployment, Node app and secret;
- one worker per deployment taking 250 requests at once, so every request shares a process;
- probes: concurrency, framing and desync between router, tunnel, runtime, loopback HTTP/1.1 and
  the app (conflicting lengths, `Expect`, huge and repeated headers, HTTP/1.0, an app that answers
  early, twice, past its length or not at all, a slow client), the response cache's keys and what
  it keeps, headers only the platform may send, a forged worker preface on the tunnel, and what
  the app's environment holds.

After each probe that could leave a connection in a bad state, a batch of other users' requests
must each get only their own answer.

Usage (after `cargo build -p forkrail-router -p forkrail-runtime`):
    python3 tools/isolation-check.py [--port 28480] [--tunnel-port 28443] [--keep]
Exit status 0 when every check passed. Prints one line per check.
"""

import argparse
import concurrent.futures as cf
import gzip
import json
import os
import random
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

APP = r"""
import http from 'node:http';
import fs from 'node:fs';
import zlib from 'node:zlib';
const context = () => globalThis[Symbol.for('@vercel/request-context')].get();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let imageCalls = [];
http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  const p = url.pathname;
  const id = req.headers['x-test-id'] ?? '';
  const json = (status, value, headers = {}) => { res.writeHead(status, { 'content-type': 'application/json', ...headers }); res.end(JSON.stringify(value)); };
  if (p === '/ctx') {
    let body = '';
    for await (const c of req) body += c;
    const seen = [];
    for (let i = 0; i < 4; i++) { await sleep(Math.random() * 20); seen.push(context().headers?.['x-test-id']); }
    console.log('line ' + id);
    res.writeHead(200, { 'content-type': 'application/json' });
    for (let i = 0; i < 3; i++) { await sleep(Math.random() * 5); res.write(' '); }
    return res.end(JSON.stringify({ id, body, cookie: req.headers.cookie, seen, secret: process.env.SECRET, project: process.env.PROJECT }));
  }
  if (p === '/headers') return json(200, req.headers);
  if (p === '/env') {
    const out = { names: Object.keys(process.env).sort(), cwd: process.cwd() };
    try { out.parent = fs.readFileSync('/proc/' + process.ppid + '/environ', 'utf8').split('\0').map((l) => l.split('=')[0]); } catch (e) { out.parent = 'unreadable: ' + e.code; }
    return json(200, out);
  }
  // Answers a POST before reading its body.
  if (p === '/early') { res.writeHead(401, { 'content-type': 'text/plain' }); return res.end('early ' + id); }
  // A second answer, written raw after the first.
  if (p === '/twice') {
    res.end('first ' + id);
    const leak = 'LEAK-' + id;
    setImmediate(() => { try { req.socket.write('HTTP/1.1 200 OK\r\ncontent-length: ' + leak.length + '\r\n\r\n' + leak); } catch {} });
    return;
  }
  // An answer with another one's bytes after it, in one write.
  if (p === '/extra') {
    const leak = 'LEAK-' + id, own = 'short ' + id;
    req.socket.write('HTTP/1.1 200 OK\r\ncontent-length: ' + own.length + '\r\n\r\n' + own + 'HTTP/1.1 200 OK\r\ncontent-length: ' + leak.length + '\r\n\r\n' + leak);
    return;
  }
  // Promises more than it sends, then closes.
  if (p === '/close') { res.writeHead(200, { 'content-type': 'text/plain', 'content-length': '1000' }); res.write('partial ' + id); await sleep(20); return req.socket.destroy(); }
  if (p === '/stream') { res.writeHead(200, { 'content-type': 'text/plain' }); for (let i = 0; i < 10; i++) { await sleep(20); res.write(id + ':' + i + '\n'); } return res.end(); }
  if (p === '/slow') { await sleep(Number(url.searchParams.get('ms') ?? 200)); return res.end('slow ' + id); }
  if (p === '/body') { let n = 0; for await (const c of req) n += c.length; return json(200, { id, bytes: n, te: req.headers['transfer-encoding'] ?? null, cl: req.headers['content-length'] ?? null }); }
  // A CDN answer that shows what made it.
  if (p.startsWith('/cache')) {
    const ae = req.headers['accept-encoding'] ?? '';
    const body = JSON.stringify({ id, cookie: req.headers.cookie ?? null, xfh: req.headers['x-forwarded-host'], xou: req.headers['x-original-url'] ?? null, fwd: req.headers.forwarded ?? null, host: req.headers.host, ae });
    const headers = { 'content-type': 'application/json', 'cache-control': url.searchParams.get('cc') ?? 's-maxage=60' };
    if (url.searchParams.has('cookie')) headers['set-cookie'] = 'session=' + id + '; Path=/; HttpOnly';
    if (url.searchParams.has('vary')) headers.vary = url.searchParams.get('vary');
    if (url.searchParams.has('cdn')) headers['vercel-cdn-cache-control'] = url.searchParams.get('cdn');
    // As `compression` does: gzip for any client whose Accept-Encoding takes it, `*` included.
    if (url.searchParams.has('gz') && /gzip|\*/.test(ae)) {
      res.writeHead(200, { ...headers, 'content-encoding': 'gzip', vary: 'Accept-Encoding' });
      return res.end(zlib.gzipSync(body));
    }
    res.writeHead(200, headers);
    return res.end(body);
  }
  // An image that depends on who asks (a private avatar): its source for /_vercel/image.
  if (p === '/avatar') {
    const who = (req.headers.cookie ?? '').replace(/^.*sid=([^;]*).*$/, '$1') || (req.headers.authorization ?? 'nobody');
    res.writeHead(200, { 'content-type': 'image/png', 'cache-control': 'private, no-store' });
    return res.end('avatar-of-' + who);
  }
  res.writeHead(404); res.end('no ' + p);
}).listen(Number(process.env.PORT), '127.0.0.1');
"""


class Checks:
    def __init__(self):
        self.failed = []
        self.lock = threading.Lock()

    def ok(self, name, passed, detail=""):
        with self.lock:
            print(f"{'PASS' if passed else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""), flush=True)
            if not passed:
                self.failed.append(name)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def raw(port, data, read_for=1.5, chunks=None, pause=0.0):
    """Bytes sent as they are on a new connection; what came back within `read_for` seconds."""
    s = socket.create_connection(("127.0.0.1", port), timeout=read_for)
    try:
        for part in chunks or [data]:
            s.sendall(part)
            if pause:
                time.sleep(pause)
        out = b""
        end = time.time() + read_for
        while time.time() < end:
            try:
                got = s.recv(65536)
            except socket.timeout:
                break
            if not got:
                break
            out += got
        return out
    finally:
        s.close()


# Connections this machine's listen backlog reset before the router read them (macOS keeps 128),
# sent again: the router never saw them.
RESENT = [0]


def http(port, host, method, path, headers=None, body=None, timeout=20):
    """One request; (status, headers as a dict of lists, body bytes)."""
    import http.client

    for attempt in range(30):
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
        try:
            conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
            conn.putheader("Host", host)
            for k, v in (headers or {}).items():
                for value in v if isinstance(v, list) else [v]:
                    conn.putheader(k, value)
            if body is not None:
                conn.putheader("Content-Length", str(len(body)))
            conn.endheaders(body)
            r = conn.getresponse()
        except (ConnectionResetError, BrokenPipeError, ConnectionRefusedError) as e:
            conn.close()
            if attempt == 29:
                raise
            RESENT[0] += 1
            time.sleep(0.02 * (attempt + 1))
            continue
        try:
            hs = {}
            for k, v in r.getheaders():
                hs.setdefault(k.lower(), []).append(v)
            return r.status, hs, r.read()
        finally:
            conn.close()


class Optimizer:
    """Stands in for deploy/image/index.mjs: the "optimized" image is the source's own bytes."""

    def __init__(self):
        import http.server
        import base64

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                event = json.loads(self.rfile.read(int(self.headers["content-length"])))
                body = event.get("source", {}).get("body")
                now = int(time.time())
                answer = {"ok": True, "contentType": "image/webp", "maxAge": 60, "revalidate": True, "immutable": False,
                          "expires": now + 60, "created": now, "body": body or base64.b64encode(b"remote").decode()}
                out = json.dumps(answer).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(out)))
                self.end_headers()
                self.wfile.write(out)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


class Local:
    """A router on loopback with two projects' deployments."""

    def __init__(self, port, tunnel_port, inflight):
        self.port, self.tunnel_port = port, tunnel_port
        self.dir = tempfile.mkdtemp(prefix="forkrail-isolation-")
        deployments = {}
        for project in ("alpha", "beta"):
            app = os.path.join(self.dir, project)
            os.makedirs(app)
            with open(os.path.join(app, "server.mjs"), "w") as f:
                f.write(APP)
            deployments[f"dpl{project}"] = {
                "project": project,
                "function": "local",
                "qualifier": "1",
                "max_inflight": inflight,
                "max_workers": 1,
                "idle_ms": 60000,
                "max_duration_s": 30,
                "memory_mb": 1024,
                "target": "production",
                "runtime": "nodejs24.x",
                "entry": "server.mjs",
                "env": {"FORKRAIL_APP_DIR": app, "FORKRAIL_APP_ENTRY": "server.mjs", "SECRET": f"{project}-secret-{random.getrandbits(64):x}", "PROJECT": project},
            }
        self.secrets = {d["project"]: d["env"]["SECRET"] for d in deployments.values()}
        os.makedirs(os.path.join(self.dir, "static/images"))
        with open(os.path.join(self.dir, "static/images/img1.json"), "w") as f:
            json.dump({"sizes": [64, 128], "minimumCacheTTL": 60, "formats": ["image/webp"], "dangerouslyAllowSVG": False,
                       "contentSecurityPolicy": "script-src 'none'; frame-src 'none'; sandbox;", "contentDispositionType": "inline"}, f)
        for d in deployments.values():
            d["images"] = "img1"
        self.optimizer = Optimizer()
        routes = {"deployments": deployments, "hosts": {"alpha.test": "dplalpha", "beta.test": "dplbeta"}}
        with open(os.path.join(self.dir, "routes.json"), "w") as f:
            json.dump(routes, f)
        self.log = open(os.path.join(self.dir, "router.log"), "w")
        env = {
            **os.environ,
            "FORKRAIL_RUNTIME_BIN": os.path.join(ROOT, "target/debug/forkrail-runtime"),
            "FORKRAIL_LISTEN": f"127.0.0.1:{port}",
            "FORKRAIL_TUNNEL_LISTEN": f"127.0.0.1:{tunnel_port}",
            "FORKRAIL_TUNNEL_ADDR": f"127.0.0.1:{tunnel_port}",
            "FORKRAIL_ROUTES": os.path.join(self.dir, "routes.json"),
            "FORKRAIL_STATIC_DIR": self.dir,
            "FORKRAIL_DEFAULT_POOL": "0",
            "FORKRAIL_BOTS": "0",
            "FORKRAIL_HOME_REGION": "us-east-1",
            "FORKRAIL_PAGES_DIR": os.path.join(self.dir, "pages"),
            "FORKRAIL_IMAGE_URL": self.optimizer.url,
            "RUST_LOG": "info",
        }
        self.proc = subprocess.Popen([os.path.join(ROOT, "target/debug/forkrail-router")], env=env, stdout=self.log, stderr=subprocess.STDOUT, cwd=ROOT)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError("router didn't start: see " + self.log.name)

    def stop(self, keep):
        self.proc.terminate()
        try:
            self.proc.wait(5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        if not keep:
            shutil.rmtree(self.dir, ignore_errors=True)
        else:
            print("kept", self.dir)


def ctx_request(local, host, i):
    project = host.split(".")[0]
    tid = f"{project}-user{i}"
    status, hs, body = http(local.port, host, "POST", f"/ctx?u={tid}", {"x-test-id": tid, "cookie": f"sid={tid}", "content-type": "text/plain"}, f"body-{tid}".encode())
    return project, tid, status, body


def check_answers(local, results):
    """Each answer is its own request's: its id, body, cookie and context, and its project's secret."""
    bad = []
    for project, tid, status, body in results:
        try:
            got = json.loads(body)
        except ValueError:
            bad.append(f"{tid}: {status} {body[:80]!r}")
            continue
        own = got.get("id") == tid and got.get("body") == f"body-{tid}" and got.get("cookie") == f"sid={tid}" and set(got.get("seen", [])) == {tid}
        if status != 200 or not own or got.get("secret") != local.secrets[project] or got.get("project") != project:
            bad.append(f"{tid}: {status} {body[:120]!r}")
    return bad


def victims(local, n=40):
    """Other users' requests right after a probe: each must get only its own answer."""
    with cf.ThreadPoolExecutor(n) as pool:
        results = list(pool.map(lambda i: ctx_request(local, ("alpha.test", "beta.test")[i % 2], 10_000 + i), range(n)))
    return check_answers(local, results)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--tunnel-port", type=int, default=0)
    ap.add_argument("--inflight", type=int, default=250)
    ap.add_argument("--keep", action="store_true", help="keep the run's folder (router log, routes)")
    args = ap.parse_args()
    checks = Checks()
    local = Local(args.port or free_port(), args.tunnel_port or free_port(), args.inflight)
    try:
        run(local, checks)
    finally:
        local.stop(args.keep)
    print(f"INFO  connections reset by this machine's backlog and sent again: {RESENT[0]}")
    print(f"\n{len(checks.failed)} failed" + (": " + ", ".join(checks.failed) if checks.failed else ""))
    sys.exit(1 if checks.failed else 0)


def run(local, checks):
    port = local.port
    # Warm both workers.
    for host in ("alpha.test", "beta.test"):
        http(port, host, "GET", "/slow?ms=1")

    # ---- 1. Concurrency: 250 in one alpha worker and 100 in one beta worker, at once. ----
    with cf.ThreadPoolExecutor(350) as pool:
        jobs = [("alpha.test", i) for i in range(250)] + [("beta.test", i) for i in range(100)]
        results = list(pool.map(lambda j: ctx_request(local, *j), jobs))
    bad = check_answers(local, results)
    checks.ok("concurrency: 350 requests in flight over two projects, each sees only its own", not bad, "; ".join(bad[:3]))

    # Streams and aborted streams interleaved with others.
    def stream(i):
        tid = f"s{i}"
        if i % 3 == 0:
            raw(port, f"GET /stream HTTP/1.1\r\nHost: alpha.test\r\nx-test-id: {tid}\r\n\r\n".encode(), read_for=0.05)
            return True
        _, _, body = http(port, "alpha.test", "GET", "/stream", {"x-test-id": tid})
        return body.decode() == "".join(f"{tid}:{n}\n" for n in range(10))

    with cf.ThreadPoolExecutor(60) as pool:
        streamed = list(pool.map(stream, range(60)))
    checks.ok("streams: 60 at once, a third left mid-stream, each gets its own chunks", all(streamed))
    bad = victims(local)
    checks.ok("after aborted streams, other users get their own answers", not bad, "; ".join(bad[:3]))

    # ---- 2. Framing and desync. ----
    def probe(name, data, expect=None, refuse_body=None, **kw):
        out = raw(port, data, **kw)
        head = out.split(b"\r\n", 1)[0].decode(errors="replace")
        passed = True
        if expect is not None:
            passed = any(head.startswith(f"HTTP/1.1 {e}") or head.startswith(f"HTTP/1.0 {e}") for e in expect)
        if refuse_body is not None and refuse_body in out:
            passed = False
        checks.ok(f"framing: {name}", passed, head or "no answer")
        bad = victims(local)
        checks.ok(f"  then other users get their own answers", not bad, "; ".join(bad[:3]))
        return out

    smuggled = b"GET /ctx HTTP/1.1\r\nHost: beta.test\r\nx-test-id: smuggled\r\n\r\n"
    out = probe(
        "content-length and transfer-encoding together",
        b"POST /body HTTP/1.1\r\nHost: alpha.test\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n" + smuggled,
        expect=[400, 200],
    )
    checks.ok("  no second request smuggled in after it", b"smuggled" not in out)
    probe("two different content-lengths", b"POST /body HTTP/1.1\r\nHost: alpha.test\r\nContent-Length: 5\r\nContent-Length: 50\r\n\r\nhello", expect=[400])
    probe("transfer-encoding: chunked, chunked", b"POST /body HTTP/1.1\r\nHost: alpha.test\r\nTransfer-Encoding: chunked, chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n", expect=[400, 200, 501])
    probe("obfuscated transfer-encoding (xchunked)", b"POST /body HTTP/1.1\r\nHost: alpha.test\r\nTransfer-Encoding: xchunked\r\nContent-Length: 5\r\n\r\nhello", expect=[400, 501, 200])
    out = probe("chunked body with a request after it", b"POST /body HTTP/1.1\r\nHost: alpha.test\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n" + smuggled, expect=[200])
    checks.ok("  the pipelined request after it is the same client's, answered in order", out.count(b"HTTP/1.1 200") == 2 and b'"bytes":5' in out)
    probe("expect: 100-continue", b"POST /body HTTP/1.1\r\nHost: alpha.test\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello", expect=[100, 200])
    probe("expect: something else", b"POST /body HTTP/1.1\r\nHost: alpha.test\r\nExpect: 200-ok\r\nContent-Length: 5\r\n\r\nhello", expect=[417, 200])
    # A header block too big for the tunnel: refused, and never at the cost of others in flight.
    def in_flight(n=20):
        def one(i):
            tid = f"inflight{i}"
            try:
                s, _, b = http(port, "alpha.test", "GET", "/slow?ms=1500", {"x-test-id": tid})
                return s == 200 and b == f"slow {tid}".encode()
            except Exception:  # noqa: BLE001
                return False

        pool = cf.ThreadPoolExecutor(n)
        futures = [pool.submit(one, i) for i in range(n)]
        time.sleep(0.5)
        return pool, futures

    pool, futures = in_flight()
    probe("a 200 KB header", b"GET /headers HTTP/1.1\r\nHost: alpha.test\r\nX-Big: " + b"a" * 200_000 + b"\r\n\r\n", expect=[494])
    lost = sum(not f.result() for f in futures)
    pool.shutdown()
    checks.ok("  requests in flight on the same worker meanwhile all answered", lost == 0, f"{lost} of {len(futures)} failed")
    probe("500 headers", b"GET /headers HTTP/1.1\r\nHost: alpha.test\r\n" + b"".join(b"X-H%d: v\r\n" % i for i in range(500)) + b"\r\n", expect=[431])

    # As much as Vercel takes reaches the app (measured on a Vercel deployment, 2026-11-16): one
    # field of 32 KiB, 499 fields and Host, about 127.5 KB in all; one byte more is refused.
    def sized(fields, path="/headers"):
        return f"GET {path} HTTP/1.1\r\nHost: alpha.test\r\n".encode() + b"".join(f"{k}: {v}\r\n".encode() for k, v in fields) + b"\r\n"

    probe("a 32 KiB field (Vercel's most) reaches the app", sized([("X-Big", "a" * 32_759)]), expect=[200])
    seen = json.loads(http(port, "alpha.test", "GET", "/headers", {"X-Big": "a" * 32_759})[2])
    checks.ok("  and the app saw all of it", seen.get("x-big") == "a" * 32_759)
    probe("a field one byte over 32 KiB", sized([("X-Big", "a" * 32_760)]), expect=[494])
    out = probe("499 fields and Host reach the app", sized([(f"X-N{i}", "v") for i in range(499)]), expect=[200])
    checks.ok("  and the app saw them all", b'"x-n498":"v"' in out)
    probe("501 fields", sized([(f"X-N{i}", "v") for i in range(500)]), expect=[431])
    # 4 x 30 KB and the rest: 127,499 bytes of request line and fields pass, one more doesn't.
    head = lambda rest: [(f"X-T{i}", "a" * 30_000) for i in range(4)] + [("X-Rest", "a" * rest)]
    base = len(sized(head(0))) - 2  # request line and fields, without the blank line
    probe("127,499 bytes of request line and fields reach the app", sized(head(127_499 - base)), expect=[200])
    seen = json.loads(http(port, "alpha.test", "GET", "/headers", dict(head(127_499 - base)))[2])
    checks.ok("  and the app saw all of it", all(seen.get(f"x-t{i}") == "a" * 30_000 for i in range(4)) and len(seen.get("x-rest", "")) == 127_499 - base)
    probe("127,500 bytes of request line and fields", sized(head(127_500 - base)), expect=[431])
    out = probe("two Host headers", b"GET /headers HTTP/1.1\r\nHost: alpha.test\r\nHost: beta.test\r\n\r\n", expect=[400, 404, 200])
    try:
        seen_host = json.loads(out[out.index(b"{") : out.rindex(b"}") + 1]).get("host")
    except ValueError:
        seen_host = None
    print(f"INFO    with two Host headers the router routed by the first, and the app saw host {seen_host!r}")
    out = probe("HTTP/1.0 with keep-alive, two requests", b"GET /slow?ms=1 HTTP/1.0\r\nHost: alpha.test\r\nConnection: keep-alive\r\n\r\nGET /slow?ms=1 HTTP/1.0\r\nHost: alpha.test\r\nConnection: keep-alive\r\n\r\n", expect=[200])
    probe("HTTP/1.0 without Host", b"GET /headers HTTP/1.0\r\n\r\n", expect=[404, 400])
    probe("a bare LF request", b"GET /headers HTTP/1.1\nHost: alpha.test\n\n", expect=[400, 200])
    probe("a header with a NUL", b"GET /headers HTTP/1.1\r\nHost: alpha.test\r\nX-A: a\x00b\r\n\r\n", expect=[400])
    probe("an obs-fold header line", b"GET /headers HTTP/1.1\r\nHost: alpha.test\r\nX-A: a\r\n b\r\n\r\n", expect=[400, 200])

    # The app answers early, twice, past its length, or closes: the next users on the runtime's
    # pooled connection to the app must not get those bytes.
    def app_probe(name, method, path, body=None):
        results = []
        lock = threading.Lock()

        def one(i):
            tid = f"bad{i}"
            try:
                s, _, b = http(port, "alpha.test", method, path, {"x-test-id": tid}, body)
            except Exception as e:  # noqa: BLE001 - a broken answer is allowed, a foreign one isn't
                s, b = 0, repr(e).encode()
            with lock:
                results.append((tid, s, b))

        with cf.ThreadPoolExecutor(20) as pool:
            list(pool.map(one, range(20)))
            # Others at the same time.
            others = pool.submit(victims, local, 40)
            bad = others.result()
        foreign = [(t, b[:60]) for t, s, b in results if b"LEAK" in b or (b"bad" in b and t.encode() not in b)]
        checks.ok(f"app: {name}: its own answers carry nothing of another's", not foreign, str(foreign[:2]))
        checks.ok(f"  others during it get their own answers", not bad, "; ".join(bad[:3]))
        bad = victims(local)
        checks.ok(f"  others after it get their own answers, never a LEAK", not bad, "; ".join(bad[:3]))
        return results

    app_probe("answers a 1 MB POST before reading it", "POST", "/early", b"x" * 1_000_000)
    app_probe("writes a second answer after the first", "GET", "/twice")
    app_probe("writes another answer's bytes past its length", "GET", "/extra")
    app_probe("closes mid-answer", "GET", "/close")

    # A slow client holds its request open; others go on.
    slow = threading.Thread(target=raw, args=(port, None), kwargs={"chunks": [b"POST /body HTTP/1.1\r\n", b"Host: alpha.test\r\n", b"Content-Length: 10\r\n\r\n", b"01234", b"56789"], "pause": 1.0, "read_for": 6})
    slow.start()
    time.sleep(0.5)
    started = time.time()
    bad = victims(local)
    checks.ok("slowloris: others get their own answers while a client trickles", not bad and time.time() - started < 5, f"{time.time() - started:.1f}s")
    slow.join()

    # ---- 3. Headers only the platform sends. ----
    forged = {
        "x-forkrail-function": "/nope",
        "x-forkrail-middleware": "1",
        "x-forkrail-request-id": "forged-id",
        "x-forkrail-run": "token",
        "x-vercel-forwarded-for": "6.6.6.6",
        "x-vercel-ip-country": "XX",
        "x-vercel-id": "forged",
        "x-middleware-subrequest": "middleware",
        "x-middleware-rewrite": "/admin",
        "x-forwarded-for": "6.6.6.6",
        "x-forwarded-host": "evil.example",
        "x-forwarded-proto": "gopher",
        "x-real-ip": "6.6.6.6",
        "x-vercel-cron-schedule": "* * * * *",
        "x-forkrail-schedule": "k",
    }
    _, _, body = http(port, "alpha.test", "GET", "/headers", forged)
    seen = json.loads(body)
    leaked = {k: seen[k] for k in forged if seen.get(k) == forged[k]}
    checks.ok("platform headers a client sends never reach the app as sent", not leaked, json.dumps(leaked))
    passed_through = [k for k in ("forwarded", "x-original-url", "x-rewrite-url", "x-forwarded-port", "x-forwarded-prefix") if k in json.loads(http(port, "alpha.test", "GET", "/headers", {k: "v" for k in ("forwarded", "x-original-url", "x-rewrite-url", "x-forwarded-port", "x-forwarded-prefix")})[2])]
    print(f"INFO  headers a client sends that reach the app as sent (apps that trust them can be fooled): {passed_through}")

    # ---- 4. The response cache. ----
    def cached(path, headers=None, host="alpha.test"):
        s, hs, b = http(port, host, "GET", path, headers)
        if hs.get("content-encoding") == ["gzip"]:
            b = gzip.decompress(b)
        return s, hs.get("x-vercel-cache", [""])[0], json.loads(b) if b.startswith(b"{") else b

    n = random.getrandbits(32)
    # A client's x-forwarded-host doesn't decide what everyone gets.
    _, _, first = cached(f"/cache-xfh?{n}", {"x-test-id": "attacker", "x-forwarded-host": "evil.example"})
    _, label, second = cached(f"/cache-xfh?{n}", {"x-test-id": "victim"})
    checks.ok("cache: x-forwarded-host from a client is replaced before the app sees it", first["xfh"] == "alpha.test" and second["xfh"] == "alpha.test", f"{first['xfh']}, {label}")
    # x-original-url and Forwarded reach the app and aren't in the key.
    _, _, first = cached(f"/cache-xou?{n}", {"x-test-id": "attacker", "x-original-url": "/admin", "forwarded": "host=evil.example"})
    _, label, second = cached(f"/cache-xou?{n}", {"x-test-id": "victim"})
    print(f"INFO  cache: an app that trusts x-original-url / Forwarded can be poisoned: victim got {label} made with xou={second['xou']!r} fwd={second['fwd']!r}")
    # The answer that sets a cookie is never kept.
    cached(f"/cache-cookie?cookie&{n}", {"x-test-id": "alice"})
    _, label, got = cached(f"/cache-cookie?cookie&{n}", {"x-test-id": "bob"})
    checks.ok("cache: an answer with set-cookie is never kept", got["id"] == "bob" and label != "HIT", label)
    for cc in ("private, s-maxage=60", "no-store, s-maxage=60", "no-cache, s-maxage=60", "s-maxage=0"):
        q = urllib.parse.quote(cc)
        cached(f"/cache-cc?cc={q}&{n}", {"x-test-id": "alice"})
        _, label, got = cached(f"/cache-cc?cc={q}&{n}", {"x-test-id": "bob"})
        checks.ok(f"cache: cache-control {cc!r} is never kept", got["id"] == "bob", label)
    # Authorization: not looked up, not kept.
    cached(f"/cache-auth?{n}", {"x-test-id": "alice", "authorization": "Bearer alice"})
    _, label, got = cached(f"/cache-auth?{n}", {"x-test-id": "bob"})
    _, label2, got2 = cached(f"/cache-auth?{n}", {"x-test-id": "carol", "authorization": "Bearer carol"})
    checks.ok("cache: a request with Authorization neither keeps nor gets a kept answer", got["id"] == "bob" and got2["id"] == "carol", f"{label}, {label2}")
    # Vary: Cookie isn't kept; a cookie without Vary is the app's choice (Vercel's too).
    cached(f"/cache-vary?vary=Cookie&{n}", {"x-test-id": "alice", "cookie": "sid=alice"})
    _, label, got = cached(f"/cache-vary?vary=Cookie&{n}", {"x-test-id": "bob", "cookie": "sid=bob"})
    checks.ok("cache: an answer that varies by Cookie is never kept", got["id"] == "bob", label)
    cached(f"/cache-vary?vary=X-User&{n}", {"x-test-id": "alice", "x-user": "alice"})
    _, label, got = cached(f"/cache-vary?vary=X-User&{n}", {"x-test-id": "bob", "x-user": "bob"})
    _, label2, got2 = cached(f"/cache-vary?vary=X-User&{n}", {"x-test-id": "alice2", "x-user": "alice"})
    checks.ok("cache: Vary: X-User keys by it", got["id"] == "bob" and got2["id"] == "alice" and label2 == "HIT", f"{label}, {label2}")
    # Per host and per deployment.
    cached(f"/cache-host?{n}", {"x-test-id": "alpha-user"}, host="alpha.test")
    _, label, got = cached(f"/cache-host?{n}", {"x-test-id": "beta-user"}, host="beta.test")
    checks.ok("cache: another project's host never gets alpha's answer", got["id"] == "beta-user" and got["host"] == "beta.test", label)
    # Accept-Encoding: an answer compressed for one client only goes to clients that take it.
    cached(f"/cache-gz?gz&{n}", {"x-test-id": "gz-client", "accept-encoding": "gzip"})
    s, hs, b = http(port, "alpha.test", "GET", f"/cache-gz?gz&{n}", {"x-test-id": "plain-client"})
    checks.ok("cache: a gzip answer isn't served to a client that takes none", hs.get("content-encoding") is None, f"{hs.get('x-vercel-cache')}, {hs.get('content-encoding')}")
    # `*` takes any coding: the app gzips, the key says none.
    http(port, "alpha.test", "GET", f"/cache-star?gz&{n}", {"x-test-id": "star", "accept-encoding": "*"})
    s, hs, b = http(port, "alpha.test", "GET", f"/cache-star?gz&{n}", {"x-test-id": "plain"})
    checks.ok("cache: Accept-Encoding: * can't make a plain client get gzip", hs.get("content-encoding") is None, f"{hs.get('x-vercel-cache')}, {hs.get('content-encoding')}")

    # Image Optimization from a function: one visitor's image must not be every visitor's.
    path = f"/_vercel/image?url=%2Favatar%3Fn%3D{n}&w=64&q=75"
    s1, hs1, b1 = http(port, "alpha.test", "GET", path, {"cookie": "sid=alice", "accept": "image/webp"})
    s2, hs2, b2 = http(port, "alpha.test", "GET", path, {"cookie": "sid=bob", "accept": "image/webp"})
    s3, hs3, b3 = http(port, "alpha.test", "GET", path, {"accept": "image/webp"})
    checks.ok(
        "images: a function's image made for one visitor (their cookie) is never served to another",
        b"alice" not in b2 and b"alice" not in b3,
        f"alice {s1} {b1[:40]!r} {hs1.get('x-vercel-cache')}; bob {s2} {b2[:40]!r} {hs2.get('x-vercel-cache')}; no cookie {s3} {b3[:40]!r}",
    )
    path = f"/_vercel/image?url=%2Favatar%3Fa%3D{n}&w=64&q=75"
    http(port, "alpha.test", "GET", path, {"authorization": "Bearer alice-token", "accept": "image/webp"})
    s2, hs2, b2 = http(port, "alpha.test", "GET", path, {"accept": "image/webp"})
    checks.ok("images: nor one made with someone's Authorization", b"alice" not in b2, f"{b2[:40]!r} {hs2.get('x-vercel-cache')}")

    # ---- 5. Between projects on the tunnel. ----
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE

    def preface(line):
        try:
            with socket.create_connection(("127.0.0.1", local.tunnel_port), timeout=3) as s, ctx.wrap_socket(s) as t:
                t.sendall(line)
                return t.recv(3)
        except OSError as e:
            return repr(e).encode()

    answers = [
        preface(f"FORKRAIL/1 dplbeta.{random.getrandbits(32):08x} {random.getrandbits(128):032x} 0 0 logs\n".encode()),
        preface(f"FORKRAIL/1 dplbeta.00000000 00000000000000000000000000000000 0 0\n".encode()),
        preface(b"FORKRAIL-EGRESS/1 dplbeta.00000000 00000000000000000000000000000000 analytics\n"),
        preface(b"FORKRAIL/1 dplbeta 0 0 0\n"),
    ]
    checks.ok("tunnel: forged worker prefaces and egress asks are refused", all(a != b"OK\n" for a in answers), str(answers))
    bad = victims(local)
    checks.ok("  and every user still gets their own answer", not bad, "; ".join(bad[:3]))

    # ---- 6. What the app's environment holds. ----
    _, _, body = http(port, "alpha.test", "GET", "/env")
    env = json.loads(body)
    hidden = [k for k in ("FORKRAIL_ENV_KEY", "FORKRAIL_LOCAL_PAYLOAD") if k in env["names"]]
    checks.ok("env: the app never sees FORKRAIL_ENV_KEY or the worker's payload", not hidden, str(hidden))
    checks.ok("env: alpha's app has none of beta's variables", local.secrets["beta"] not in body.decode())
    print(f"INFO  env: the runtime's own environment, read by the app from /proc/<runtime>/environ: {env['parent']}")


if __name__ == "__main__":
    main()
