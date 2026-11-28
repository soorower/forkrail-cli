"""Isolation between concurrent requests in one Python app under the shim (shim.py), as the
runtime runs it: one uvicorn process, with its control fd.

What each request must see of itself only, however many are in flight: its own context variables
across awaits, threads it hands work to (asyncio.to_thread, Starlette's thread pool), tasks and
callbacks that outlive its response; its own lines tagged with its own request id; its outgoing
calls reported with it. A WSGI app's threads, and a plain thread the app starts (which has no
context in Python 3.13), are covered too: the latter's lines go untagged, never to another request.

Run with the Python that runs apps (uvicorn and FastAPI installed):
    target/python-venv/bin/python crates/runtime/src/shim_test.py
"""

import http.client
import http.server
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor

SHIM = os.environ.get("FORKRAIL_TEST_SHIM") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "shim.py")
IN_FLIGHT = 200
ABORTED_EVERY = 10
TAG = re.compile(r"^\x1e([A-Za-z0-9_-]+)\x1f(.*)$", re.S)


def rid(i):
    return f"rid{i:06x}"


def tid(i):
    return f"user{i}"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Upstream:
    """A server the app calls, which notes the id each call carried."""

    def __init__(self):
        seen = self.seen = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                seen.append(self.path.split("id=", 1)[-1])
                self.send_response(200)
                self.send_header("content-length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

            def log_message(self, *args):
                pass

        class Server(http.server.ThreadingHTTPServer):
            request_queue_size = 1024

        self.server = Server(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()


class App:
    """The app in `files`, started as the runtime starts it: `python shim.py ENTRY` (its control fd
    is another number here, which the shim takes from FORKRAIL_CONTROL_FD as it does 3)."""

    def __init__(self, files, entry, env):
        self.dir = tempfile.mkdtemp(prefix="forkrail-shim-test-")
        for name, source in files.items():
            path = os.path.join(self.dir, name)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write(source)
        self.port = free_port()
        read, write = os.pipe()
        os.set_inheritable(write, True)
        self.proc = subprocess.Popen(
            [sys.executable, SHIM, entry],
            cwd=self.dir,
            env={**os.environ, **env, "PORT": str(self.port), "FORKRAIL_CONTROL_FD": str(write), "PYTHONUNBUFFERED": "1"},
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            pass_fds=(write,),
        )
        os.close(write)
        self.control = []
        self.stdout, self.stderr = [], []
        listening = threading.Event()

        def pump(stream, into):
            for chunk in iter(lambda: stream.read1(65536), b""):
                into.append(chunk.decode())

        def control():
            with os.fdopen(read, "rb") as f:
                for line in f:
                    message = json.loads(line)
                    self.control.append(message)
                    if message["type"] == "listening":
                        listening.set()

        for target, args in ((pump, (self.proc.stdout, self.stdout)), (pump, (self.proc.stderr, self.stderr)), (control, ())):
            threading.Thread(target=target, args=args, daemon=True).start()
        if not listening.wait(20):
            self.stop()
            raise RuntimeError("the app didn't listen: " + "".join(self.stderr))

    def stop(self):
        self.proc.kill()
        self.proc.wait()
        shutil.rmtree(self.dir, ignore_errors=True)


def connect(port):
    """A connection to the app; 200 at once can overflow this machine's listen backlog (128 on
    macOS, which also says EINVAL now and then), so a failed connect is tried again."""
    for attempt in range(50):
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        try:
            conn.connect()
            return conn
        except OSError:
            conn.close()
            time.sleep(0.01 * (attempt + 1))
    raise RuntimeError("couldn't connect to the app")


def ask(port, i, path):
    for attempt in range(20):
        conn = connect(port)
        body = f"body-{tid(i)}".encode()
        try:
            conn.request(
                "POST",
                f"{path}/{tid(i)}?n={i}",
                body=body,
                headers={"x-forkrail-request-id": rid(i), "x-test-id": tid(i), "cookie": f"sid={tid(i)}", "content-type": "text/plain"},
            )
            response = conn.getresponse()
            break
        except OSError:
            # Reset by this machine's backlog before the app saw it: sent again.
            conn.close()
            time.sleep(0.02 * (attempt + 1))
    else:
        raise RuntimeError("couldn't send to the app")
    if i % ABORTED_EVERY == 0:
        # The client leaves once the answer has started.
        response.close()
        conn.close()
        return None
    text = response.read().decode()
    reply = response.getheader("x-forkrail-reply-to")
    conn.close()
    return response.status, text, reply


# What each request does in an ASGI app: every way of handing work on, reading its own context
# variable after each. `seen` goes back in the answer; lines and calls are checked from outside.
ASGI = r'''
import asyncio, contextvars, json, os, random, sys, threading, urllib.request

user = contextvars.ContextVar("user", default=None)

async def work(id, path, body):
    user.set(id)
    seen = []
    look = lambda where: seen.append([where, user.get()])
    await asyncio.sleep(random.random() * 0.015); look("sleep")
    await asyncio.to_thread(lambda: print(f"thread {id} {user.get()}")); look("to_thread")
    await asyncio.to_thread(lambda: urllib.request.urlopen(os.environ["UPSTREAM"] + "/?id=" + id).read()); look("fetch")
    async def later():
        await asyncio.sleep(0.02 + random.random() * 0.04)
        print(f"task {id} {user.get()}")
    asyncio.get_running_loop().create_task(later())
    asyncio.get_running_loop().call_later(0.03 + random.random() * 0.05, lambda: print(f"late {id} {user.get()}"))
    # A plain thread has no context in 3.13: its line goes untagged (one write: see
    # test_untagged_partial_line).
    threading.Thread(target=lambda: sys.stdout.write(f"bare {id}\n")).start()
    print(f"line {id}")
    sys.stdout.write(f"half {id} ")
    await asyncio.sleep(random.random() * 0.01)
    sys.stdout.write(f"whole {id}\n")
    print(f"err {id}", file=sys.stderr)
    return seen

async def app(scope, receive, send):
    if scope["type"] == "lifespan":
        while True:
            m = await receive()
            if m["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif m["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return
    headers = dict(scope["headers"])
    id = headers[b"x-test-id"].decode()
    body = b""
    while True:
        m = await receive()
        body += m.get("body", b"")
        if not m.get("more_body"):
            break
    seen = await work(id, scope["path"], body.decode())
    await send({"type": "http.response.start", "status": 200, "headers": [(b"content-type", b"application/json")]})
    for i in range(3):
        await asyncio.sleep(random.random() * 0.01)
        seen.append([f"stream{i}", user.get()])
        await send({"type": "http.response.body", "body": b" ", "more_body": True})
    await send({"type": "http.response.body", "body": json.dumps({"id": id, "body": body.decode(), "cookie": headers.get(b"cookie", b"").decode(), "seen": seen}).encode()})
'''

# The same in FastAPI: an `async def` endpoint, and a `def` one that runs in Starlette's pool.
FASTAPI = r'''
import asyncio, contextvars, json, os, random, sys, time, urllib.request
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse

app = FastAPI()
user = contextvars.ContextVar("user", default=None)

@app.middleware("http")
async def who(request, call_next):
    user.set(request.headers["x-test-id"])
    return await call_next(request)

def work_sync(id):
    seen = [["sync", user.get()]]
    urllib.request.urlopen(os.environ["UPSTREAM"] + "/?id=" + id).read()
    time.sleep(random.random() * 0.01)
    print(f"line {id} {user.get()}")
    print(f"err {id}", file=sys.stderr)
    seen.append(["after", user.get()])
    return seen

@app.post("/sync/{name}")
def sync(name: str, request: Request):
    id = request.headers["x-test-id"]
    seen = work_sync(id)
    return JSONResponse({"id": id, "seen": seen, "cookie": request.headers.get("cookie")})

@app.post("/async/{name}")
async def asynchronous(name: str, request: Request):
    id = request.headers["x-test-id"]
    body = (await request.body()).decode()
    await asyncio.sleep(random.random() * 0.015)
    seen = await asyncio.to_thread(work_sync, id)
    seen.append(["back", user.get()])
    return JSONResponse({"id": id, "body": body, "seen": seen, "cookie": request.headers.get("cookie")})
'''

# A WSGI app (Flask's shape) through the shim's WSGI path: a thread per request.
WSGI = r'''
import json, os, random, sys, time, urllib.request

def app(environ, start_response):
    id = environ["HTTP_X_TEST_ID"]
    body = environ["wsgi.input"].read().decode()
    time.sleep(random.random() * 0.01)
    urllib.request.urlopen(os.environ["UPSTREAM"] + "/?id=" + id).read()
    print(f"line {id}")
    print(f"err {id}", file=sys.stderr)
    start_response("200 OK", [("content-type", "application/json")])
    def stream():
        yield b" "
        print(f"streamed {id}")
        yield json.dumps({"id": id, "body": body, "cookie": environ.get("HTTP_COOKIE"), "seen": []}).encode()
    return stream()
'''


class Isolation(unittest.TestCase):
    def run_app(self, files, entry, path, kinds, body=True):
        up = Upstream()
        app = App(files, entry, {"UPSTREAM": up.url})
        try:
            with ThreadPoolExecutor(IN_FLIGHT) as pool:
                answers = list(pool.map(lambda i: ask(app.port, i, path), range(IN_FLIGHT)))
            time.sleep(0.5)
            self.check(app, up, answers, kinds, body)
        finally:
            app.stop()
            up.close()

    def check(self, app, up, answers, kinds, body):
        id_of = {rid(i): tid(i) for i in range(IN_FLIGHT)}
        for i, answer in enumerate(answers):
            if i % ABORTED_EVERY == 0:
                continue
            status, text, reply = answer
            self.assertEqual(status, 200, text)
            self.assertEqual(reply, rid(i), "stamped with its own request's id")
            got = json.loads(text)
            self.assertEqual(got["id"], tid(i))
            if body:
                self.assertEqual(got["body"], f"body-{tid(i)}")
            self.assertEqual(got["cookie"], f"sid={tid(i)}")
            for where, seen in got["seen"]:
                self.assertEqual(seen, tid(i), f"{tid(i)} saw {seen} at {where}")
        counts = {}
        for stream in ("".join(app.stdout), "".join(app.stderr)):
            for line in filter(None, stream.split("\n")):
                m = TAG.match(line)
                if not m:
                    # Untagged: only a plain thread's lines, which are kept with no request.
                    self.assertFalse(re.match(r"^(line|half|whole|err|task|late|thread|streamed) ", line), repr(line))
                    continue
                request, text = m.groups()
                self.assertIn(request, id_of)
                words = text.split(" ")
                self.assertIn(words[0], kinds | {"bare"}, repr(text))
                for word in words[1:]:
                    if word.startswith("user"):
                        self.assertEqual(word, id_of[request], f"{text!r} tagged {request}")
                counts[(words[0], request)] = counts.get((words[0], request), 0) + 1
        for i in range(IN_FLIGHT):
            for kind in kinds:
                self.assertEqual(counts.get((kind, rid(i))), 1, f"{kind} of {tid(i)}")
        fetches = [m for m in app.control if m["type"] == "fetch"]
        self.assertEqual(len(fetches), IN_FLIGHT)
        for f in fetches:
            self.assertTrue(f["url"].endswith("?id=" + id_of[f["request"]]), f"{f['url']} reported with {f['request']}")
        self.assertEqual(sorted(up.seen), sorted(id_of.values()))

    def test_asgi(self):
        self.run_app({"api/main.py": ASGI}, "api/main.py", "/x", {"line", "half", "err", "thread", "task", "late"})

    def test_fastapi_async(self):
        self.run_app({"api/main.py": FASTAPI}, "api/main.py", "/async", {"line", "err"})

    def test_fastapi_sync(self):
        self.run_app({"api/main.py": FASTAPI}, "api/main.py", "/sync", {"line", "err"}, body=False)

    def test_wsgi(self):
        self.run_app({"api/main.py": WSGI}, "api/main.py", "/w", {"line", "err", "streamed"})

    # Found 2026-11-16: a line written in two parts outside any request (`print` from a plain
    # thread writes its text, then "\n") let a request's tagged line in between, and the runtime
    # then read one untagged line: the request's line lost its request (it went to no other).
    def test_untagged_partial_line(self):
        source = r'''
import sys, threading
async def app(scope, receive, send):
    if scope["type"] != "http":
        return
    started, printed = threading.Event(), threading.Event()
    def bare():
        sys.stdout.write("bare")
        started.set()
        printed.wait()
        sys.stdout.write("\n")
    threading.Thread(target=bare).start()
    started.wait()
    print("line user1")
    printed.set()
    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"ok"})
'''
        app = App({"api/main.py": source}, "api/main.py", {})
        try:
            conn = connect(app.port)
            conn.request("GET", "/", headers={"x-forkrail-request-id": rid(1)})
            self.assertEqual(conn.getresponse().read(), b"ok")
            time.sleep(0.2)
            lines = [line for line in "".join(app.stdout).split("\n") if line]
            self.assertIn(f"\x1e{rid(1)}\x1fline user1", lines)
            self.assertIn("bare", lines)
        finally:
            app.stop()

    def test_an_app_that_fails_before_answering_gets_the_shims_stamped_500(self):
        """uvicorn's own 500 would carry no stamp (the runtime would refuse it as out of step): the
        shim answers as uvicorn does, stamped, and the connection stays open for the next."""
        source = r'''
async def app(scope, receive, send):
    if scope["type"] != "http":
        return
    if scope["path"] == "/boom":
        raise RuntimeError("boom")
    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"ok"})
'''
        app = App({"api/main.py": source}, "api/main.py", {})
        try:
            conn = connect(app.port)
            conn.request("GET", "/boom", headers={"x-forkrail-request-id": rid(1)})
            r = conn.getresponse()
            self.assertEqual((r.status, r.read(), r.getheader("x-forkrail-reply-to")), (500, b"Internal Server Error", rid(1)))
            # The same connection takes the next request.
            conn.request("GET", "/", headers={"x-forkrail-request-id": rid(2)})
            r = conn.getresponse()
            self.assertEqual((r.status, r.read(), r.getheader("x-forkrail-reply-to")), (200, b"ok", rid(2)))
            self.assertIn("Exception in ASGI application", "".join(app.stderr))
        finally:
            app.stop()

    def test_an_asgi2_app_is_stamped_too(self):
        source = r'''
class app:
    def __init__(self, scope):
        self.scope = scope
    async def __call__(self, receive, send):
        if self.scope["type"] != "http":
            return
        await send({"type": "http.response.start", "status": 200, "headers": []})
        await send({"type": "http.response.body", "body": b"two"})
'''
        app = App({"api/main.py": source}, "api/main.py", {})
        try:
            conn = connect(app.port)
            conn.request("GET", "/", headers={"x-forkrail-request-id": rid(3)})
            r = conn.getresponse()
            self.assertEqual((r.status, r.read(), r.getheader("x-forkrail-reply-to")), (200, b"two", rid(3)))
        finally:
            app.stop()


class LoopMark(unittest.TestCase):
    """The loop guard's mark goes on calls to the install's own hosts (`localhost` here), from
    every client the shim patches, and on no call elsewhere (`127.0.0.1`)."""

    ASGI = r"""
import asyncio, os, urllib.request
import aiohttp, httpx, requests

async def app(scope, receive, send):
    if scope["type"] != "http":
        return
    tag = scope["query_string"].decode()
    t = os.environ["TARGET_PORT"]
    for host in ("localhost", "127.0.0.1"):
        side = "own" if host == "localhost" else "other"
        base = f"http://{host}:{t}"
        await asyncio.to_thread(requests.get, f"{base}/requests-{side}-{tag}")
        await asyncio.to_thread(urllib.request.urlopen, f"{base}/urllib-{side}-{tag}")
        await asyncio.to_thread(lambda: httpx.get(f"{base}/httpx-{side}-{tag}"))
        async with httpx.AsyncClient() as client:
            await client.get(f"{base}/httpx-async-{side}-{tag}")
        async with aiohttp.ClientSession() as session:
            async with session.get(f"{base}/aiohttp-{side}-{tag}") as r:
                await r.read()
    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"done"})
"""

    WSGI = r"""
import os, requests

def app(environ, start_response):
    tag = environ["QUERY_STRING"]
    t = os.environ["TARGET_PORT"]
    requests.get(f"http://localhost:{t}/wsgi-own-{tag}")
    requests.get(f"http://127.0.0.1:{t}/wsgi-other-{tag}")
    start_response("200 OK", [("content-type", "text/plain")])
    return [b"done"]
"""

    def run_app(self, source, clients):
        marks = {}

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                marks[self.path] = self.headers.get("x-forkrail-loop")
                self.send_response(200)
                self.send_header("content-length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

            def log_message(self, *args):
                pass

        target = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=target.serve_forever, daemon=True).start()
        hosts = tempfile.mkdtemp(prefix="forkrail-own-hosts-")
        hosts_file = os.path.join(hosts, "hosts")
        with open(hosts_file, "w") as f:
            f.write("localhost\n*.example.test\n")
        app = App({"api/main.py": source}, "api/main.py", {"TARGET_PORT": str(target.server_address[1]), "FORKRAIL_OWN_HOSTS_FILE": hosts_file})
        mark = "ab12.1." + "c" * 64
        try:
            for tag, headers in (("a", {"x-forkrail-request-id": rid(1), "x-forkrail-loop": mark}), ("b", {"x-forkrail-request-id": rid(2)})):
                conn = connect(app.port)
                conn.request("GET", f"/?{tag}", headers=headers)
                r = conn.getresponse()
                self.assertEqual((r.status, r.read()), (200, b"done"), "".join(app.stderr))
                conn.close()
            expected = {}
            for client in clients:
                for side in ("own", "other"):
                    for tag in ("a", "b"):
                        expected[f"/{client}-{side}-{tag}"] = mark if (side, tag) == ("own", "a") else None
            self.assertEqual(marks, expected)
        finally:
            app.stop()
            target.shutdown()
            shutil.rmtree(hosts, ignore_errors=True)

    def test_asgi_clients(self):
        self.run_app(self.ASGI, ("requests", "urllib", "httpx", "httpx-async", "aiohttp"))

    def test_wsgi(self):
        self.run_app(self.WSGI, ("wsgi",))


if __name__ == "__main__":
    unittest.main()
