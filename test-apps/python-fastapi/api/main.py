# Test app for the Python runtime: FastAPI laid out the way Vercel deploys it (`api/main.py`
# exporting `app`, no server of its own). It knows nothing about forkrail. Routes match
# test-apps/node-http:
#
# - GET /sleep?ms=200          waits, then answers, like an I/O-bound request
# - GET /cpu?ms=50             keeps the CPU (and the event loop) busy
# - GET /work/cpu:5,io:100,cpu:20  runs the steps in order: cpu:N uses N ms of CPU time (however
#                              long that takes where CPU is throttled), io:N waits N ms, like a call
#                              to a database or an API. In the path, so each mix is its own route.
# - GET /alloc?mb=300&ms=1000  holds 300 MB for a second
# - GET /error                 raises, which uvicorn answers with a 500; the process lives on
# - GET /error?after_ms=50     the same, but the exception reaches uvicorn 50 ms after the 500 went
#                              out, as when logging a long traceback takes a while
# - GET /crash?ms=500          exits the process, like a crash in a native module
# - GET /stream?chunks=10&ms=100  sends a line every ms
# - GET /bytes?mb=5            answers with 5 MB of one repeated byte, which compresses to almost nothing
# - GET /random?mb=1           answers with 1 MB of random bytes, which don't compress
# - GET /log?n=3&msg=hi        prints n lines to stdout and one to stderr, for checking app logs
# - GET /ip?ms=1000            the address the app's outbound calls leave from (as
#                              checkip.amazonaws.com sees it), then waits ms, so concurrent requests
#                              overlap: for checking which requests share an IP
# - GET /fetch?url=URL&n=2     calls URL n times one after another (urllib), and prints a line
#                              each time: for checking outgoing calls and their log lines
# - GET /items/{item_id}       a route with a path parameter, for checking route templates
# - any other GET or HEAD      a JSON summary of the process and the request
# - other methods              send the request body back

import asyncio
import os
import platform
import sys
import time
import urllib.error
import urllib.request

from fastapi import FastAPI, Request
from fastapi.responses import Response, StreamingResponse

app = FastAPI()
started = time.monotonic()
print(f"test app imported, Python {platform.python_version()}")
served = 0
inflight = 0


@app.middleware("http")
async def count(request: Request, call_next):
    global served, inflight
    served += 1
    inflight += 1
    try:
        return await call_next(request)
    finally:
        inflight -= 1


def num(request: Request, key: str) -> int:
    return int(request.query_params.get(key, 0))


def wait(request: Request) -> float:
    return min(num(request, "ms"), 60_000) / 1000


def rss_mb() -> int:
    try:
        with open("/proc/self/status") as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) // 1024
    except OSError:
        pass
    return 0


@app.get("/sleep")
async def sleep(request: Request):
    await asyncio.sleep(wait(request))
    return Response()


@app.get("/cpu")
async def cpu(request: Request):
    until = time.monotonic() + wait(request)
    while time.monotonic() < until:
        pass
    return Response()


@app.get("/work/{steps}")
async def work(steps: str):
    for step in steps.split(","):
        kind, _, value = step.partition(":")
        seconds = min(float(value or 0), 60_000) / 1000
        if kind == "cpu":
            until = time.thread_time() + seconds
            while time.thread_time() < until:
                pass
        elif kind == "io":
            await asyncio.sleep(seconds)
    return Response()


@app.get("/alloc")
async def alloc(request: Request):
    # Filled, so the pages are resident: a zeroed bytearray would only reserve them.
    held = bytearray(b"\x01") * (num(request, "mb") << 20)
    await asyncio.sleep(wait(request))
    return Response(str(len(held)))


@app.get("/error")
async def error():
    raise RuntimeError("error requested")


@app.get("/crash")
async def crash(request: Request):
    asyncio.get_running_loop().call_later(wait(request), os._exit, 1)
    await asyncio.sleep(3600)


@app.get("/stream")
async def stream(request: Request):
    async def lines():
        for line in range(1, num(request, "chunks") + 1):
            yield f"line {line}\n"
            await asyncio.sleep(wait(request))

    return StreamingResponse(lines(), media_type="text/plain")


@app.get("/log")
async def log(request: Request):
    msg = request.query_params.get("msg", "hello")
    n = min(num(request, "n") or 1, 10_000)
    for i in range(1, n + 1):
        print(f"{msg} {i}/{n}")
    print(f"{msg} on stderr", file=sys.stderr)
    return Response(f"logged {n + 1} lines\n", media_type="text/plain")


@app.get("/env-check")
async def env_check(name: str):
    # Whether an env var reached the app, and its length: never its value.
    value = os.environ.get(name)
    return {"name": name, "set": value is not None, "length": len(value or "")}


@app.get("/ip")
async def ip(request: Request):
    def fetch() -> str:
        with urllib.request.urlopen("https://checkip.amazonaws.com", timeout=10) as answer:
            return answer.read().decode().strip()

    address = await asyncio.to_thread(fetch)
    await asyncio.sleep(wait(request))
    # Lambda names each environment's log stream, so it tells environments apart.
    return {"ip": address, "env": os.environ.get("AWS_LAMBDA_LOG_STREAM_NAME", ""), "pid": os.getpid(), "inflight": inflight}


@app.get("/bytes")
async def bytes_(request: Request):
    return Response(b"x" * (num(request, "mb") << 20), media_type="text/plain")


# Made once, so a request costs no CPU for it: transfer tests shouldn't measure os.urandom.
RANDOM = os.urandom(4 << 20)


@app.get("/random")
async def random_bytes(request: Request):
    n = num(request, "mb") << 20
    return Response(RANDOM[:n] if n <= len(RANDOM) else os.urandom(n), media_type="application/octet-stream")


@app.get("/fetch")
async def fetch_url(request: Request):
    url = request.query_params.get("url", "https://checkip.amazonaws.com")
    statuses = []
    for i in range(max(1, min(num(request, "n"), 20))):
        def call() -> int:
            try:
                with urllib.request.urlopen(url, timeout=10) as answer:
                    answer.read()
                    return answer.status
            except urllib.error.HTTPError as e:
                return e.code

        status = await asyncio.to_thread(call)
        print(f"call {i + 1}: {status}")
        statuses.append(status)
    return {"url": url, "statuses": statuses}


@app.get("/items/{item_id}")
async def item(item_id: str):
    return {"item": item_id}


@app.api_route("/{path:path}", methods=["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"])
async def summary(request: Request, path: str):
    if request.method not in ("GET", "HEAD"):
        return Response(await request.body(), media_type=request.headers.get("content-type"))
    return {
        "pid": os.getpid(),
        "python": platform.python_version(),
        "uptime_ms": round((time.monotonic() - started) * 1000),
        "request_no": served,
        "inflight": inflight,
        "method": request.method,
        "path": "/" + path,
        "host": request.headers.get("host"),
        "client": request.client.host if request.client else None,
        "forwarded_for": request.headers.get("x-forwarded-for"),
        "real_ip": request.headers.get("x-real-ip"),
        "geo": {k: v for k, v in request.headers.items() if k.startswith("x-vercel-ip-")},
        "forwarded_proto": request.headers.get("x-forwarded-proto"),
        "scheme": request.url.scheme,
        "request_id": request.headers.get("x-forkrail-request-id"),
        "rss_mb": rss_mb(),
        "sys_path": sys.path,
    }


class RaiseLater:
    """For `/error?after_ms=N`. Starlette sends the 500 and then re-raises, so the exception reaches
    the server after the response. uvicorn then closes the connection, which by then may carry the
    next request. Here it gets there N ms later, as in one real API, where formatting a long traceback
    took about 60 ms."""

    def __init__(self, inner):
        self.inner = inner

    async def __call__(self, scope, receive, send):
        try:
            await self.inner(scope, receive, send)
        except Exception:
            if scope.get("path") == "/error":
                query = dict(p.split("=", 1) for p in scope.get("query_string", b"").decode().split("&") if "=" in p)
                await asyncio.sleep(min(int(query.get("after_ms", 0) or 0), 60_000) / 1000)
            raise


app = RaiseLater(app)
