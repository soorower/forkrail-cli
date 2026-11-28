# forkrail shim for Python apps. The runtime runs `python3 shim.py ENTRY`, where ENTRY is an ASGI
# or WSGI app's file and attribute the way Vercel finds it: `api/main.py` for its `app`, or
# `mysite/wsgi.py:application`. The app's code is never changed. Like the Node shim, it talks to
# the runtime with JSON lines on the fd in FORKRAIL_CONTROL_FD.
#
# - Serves the app with uvicorn on 127.0.0.1:$PORT, imported the way `uvicorn api.main:app` would,
#   with the app directory (the working directory) first on the import path. It reports where it
#   listens once the app's startup (lifespan) has finished.
# - A WSGI app (Flask, Django: a callable taking `environ` and `start_response`) runs in a pool of
#   WSGI_THREADS threads through a2wsgi, which the packager adds to every Python app: as many as
#   the router sends one worker at once, so a slow request doesn't hold up the others.
# - Raises uvicorn's keep-alive timeout. The runtime is its only client and closes idle
#   connections itself after 4 s, but uvicorn's default (5 s) would close them under it at once
#   after Lambda thaws a frozen environment.
# - Exits if the runtime dies, as the Node shim does.
# - Keeps the connection open after an exception in a request that has already been answered.
#   Starlette (and FastAPI) send the 500 and then re-raise, so the server can log the error, and
#   uvicorn then closes the connection. By then the runtime may have sent the next request on it,
#   since the response was complete, and that request would fail with the connection. Such an
#   exception is logged here, with the same message and traceback uvicorn would log, and stops.
# - Tags what the app prints with the request it was serving: each line written to sys.stdout or
#   sys.stderr while a request runs (in its asyncio context, or a thread it started, which copies
#   the context) starts with the router's request id between \x1e and \x1f. The runtime takes the
#   tag off and keeps the id with the line, so a request's logs can be shown with it. Lines are
#   put together per request, so two requests printing at once don't mix their halves, and so are
#   lines written outside any request (a plain thread's `print` writes its text, then "\n"): a
#   line's start is held back until its end comes (or a second passes), so no request's line lands
#   in the middle of it and is lost to it.
# - Stamps each answer with the id of the request it answers (`x-forkrail-reply-to`), which the
#   runtime checks and takes off, and answers a request whose app failed before answering with its
#   own 500 (uvicorn's would have no stamp).
# - Reports each outgoing HTTP call the app makes (curl_cffi, requests, httpx, aiohttp, urllib),
#   with the request it was made for: method, URL, status or error, start, duration and size, as
#   a `fetch` message on the control fd. The router masks the URL's secrets before it logs it.
#   The clients are patched as they're imported, so an app that never imports one pays nothing.
# - Carries the loop guard's mark (`x-forkrail-loop`, from the request being served) on each such
#   call to one of the install's own hosts, and on no other call, so the router can stop a chain
#   of calls back to the install (crates/router/src/loops.rs). The hosts are in the file
#   FORKRAIL_OWN_HOSTS_FILE names, which the runtime rewrites when the router's list changes.
# - Adds the matched route's template (`/items/{item_id}`, from Starlette's scope) to each
#   response as `x-forkrail-route`, which the router logs and takes off.
#
# An exception in a request doesn't kill a Python app (uvicorn answers 500), so unlike the Node
# shim it has no uncaught errors to report. If the process dies anyway (out of memory, a crash in
# a native module, os._exit), every request in flight is a suspect.

import asyncio
import contextvars
import functools
import importlib.abc
import importlib.util
import inspect
import json
import logging
import os
import sys
import threading
import time

import uvicorn
from uvicorn.importer import import_from_string

control = int(os.environ.get("FORKRAIL_CONTROL_FD") or 0) or None
runtime = os.getppid()
request_id = contextvars.ContextVar("forkrail_request_id", default=None)
# The loop guard's mark of the request being served, set beside its id.
loop_mark = contextvars.ContextVar("forkrail_loop_mark", default=None)
LOOP_HEADER = "x-forkrail-loop"
TAG_START, TAG_END = "\x1e", "\x1f"


def send(message):
    if control is None:
        return
    try:
        os.write(control, (json.dumps(message) + "\n").encode())
    except OSError:
        pass  # The runtime is gone, and the environment with it.


class Tagged:
    """Stands in for sys.stdout or sys.stderr: lines written while a request runs start with its
    tag. Everything else (bytes through .buffer, fileno, isatty, ...) goes to the real stream."""

    LOOSE_WAIT = 1.0
    MAX_LOOSE = 256 * 1024

    def __init__(self, stream):
        self._stream = stream
        self._partial = {}  # request id -> the start of a line not yet ended
        self._lock = threading.Lock()
        # The start of a line written outside any request, and since when it waits.
        self._loose, self._loose_since = "", 0.0

    def write(self, text):
        rid = request_id.get()
        if not isinstance(text, str):
            return self._stream.write(text)
        if rid is None:
            with self._lock:
                *lines, rest = (self._loose + text).split("\n")
                if rest and not self._loose:
                    self._loose_since = time.monotonic()
                self._loose = rest
                # Waited long enough, or too long a line: out as a line of its own.
                if rest and (len(rest) > self.MAX_LOOSE or time.monotonic() - self._loose_since > self.LOOSE_WAIT):
                    lines.append(rest)
                    self._loose = ""
                if lines:
                    self._stream.write("".join(f"{line}\n" for line in lines))
            return len(text)
        with self._lock:
            self._flush_stale_loose()
            *lines, rest = (self._partial.pop(rid, "") + text).split("\n")
            if rest:
                self._partial[rid] = rest
            if lines:
                self._stream.write("".join(f"{TAG_START}{rid}{TAG_END}{line}\n" for line in lines))
        return len(text)

    def _flush_stale_loose(self):
        """A line's start outside any request that has waited past LOOSE_WAIT goes out as a line
        (under the lock)."""
        if self._loose and time.monotonic() - self._loose_since > self.LOOSE_WAIT:
            self._stream.write(self._loose + "\n")
            self._loose = ""

    def flush_loose(self):
        """At exit: a line's start outside any request goes out as it is."""
        with self._lock:
            if self._loose:
                self._stream.write(self._loose + "\n")
                self._loose = ""
        self._stream.flush()

    def end(self, rid):
        """A request is done: its unfinished line, if any, goes out as it is."""
        with self._lock:
            rest = self._partial.pop(rid, None)
        if rest:
            self._stream.write(f"{TAG_START}{rid}{TAG_END}{rest}\n")

    def writelines(self, lines):
        for line in lines:
            self.write(line)

    def isatty(self):
        return False

    def __getattr__(self, name):
        return getattr(self._stream, name)


def tag_output():
    import atexit

    sys.stdout = Tagged(sys.stdout)
    sys.stderr = Tagged(sys.stderr)
    for stream in (sys.stdout, sys.stderr):
        atexit.register(stream.flush_loose)


def end_request(rid):
    for stream in (sys.stdout, sys.stderr):
        if isinstance(stream, Tagged):
            stream.end(rid)


MAX_URL = 1500
MAX_ERROR = 300


def report_fetch(method, url, started, t0, status=None, error=None, size=None):
    """One outgoing call, as a control message (under PIPE_BUF, so one write each)."""
    message = {
        "type": "fetch",
        "request": request_id.get(),
        "method": str(method or "GET").upper()[:16],
        "url": str(url)[:MAX_URL],
        "start": round(started, 6),
        "ms": round((time.perf_counter() - t0) * 1000, 1),
    }
    if status is not None:
        message["status"] = int(status)
    if error is not None:
        message["error"] = f"{type(error).__name__}: {error}"[:MAX_ERROR]
    if size is not None:
        message["bytes"] = int(size)
    send(message)


def body_size(response, streamed):
    if streamed:
        return None
    content = getattr(response, "_content", None)
    if isinstance(content, (bytes, bytearray)):
        return len(content)
    try:
        content = response.content
    except Exception:
        return None
    return len(content) if isinstance(content, (bytes, bytearray)) else None


OWN_HOSTS_FILE = os.environ.get("FORKRAIL_OWN_HOSTS_FILE")
_own = {"checked": 0.0, "mtime": None, "names": frozenset(), "domains": ()}


def own_hosts():
    """The install's own hosts: looked at again at most once a second, read only when changed."""
    now = time.monotonic()
    if now - _own["checked"] < 1:
        return _own
    _own["checked"] = now
    try:
        mtime = os.stat(OWN_HOSTS_FILE).st_mtime_ns
        if mtime == _own["mtime"]:
            return _own
        with open(OWN_HOSTS_FILE, encoding="utf-8") as f:
            entries = [line.strip().lower() for line in f]
    except OSError:
        return _own
    names = frozenset(e for e in entries if e and not e.startswith("*."))
    domains = tuple(e[1:] for e in entries if e.startswith("*."))
    _own.update(mtime=mtime, names=names, domains=domains)
    return _own


def mark_for(url):
    """The mark to send with a call to `url`: the request's, when `url` is one of the install's
    own hosts (`*.<domain>` covers every name under it); else None."""
    if not OWN_HOSTS_FILE:
        return None
    mark = loop_mark.get()
    if not mark:
        return None
    from urllib.parse import urlsplit

    host = (urlsplit(str(url)).hostname or "").rstrip(".").lower()
    own = own_hosts()
    if host and (host in own["names"] or any(len(host) > len(d) and host.endswith(d) for d in own["domains"])):
        return mark
    return None


def with_header(headers, name, value):
    """`headers` (None, a dict, pairs, or a mapping type) with one more, as a copy."""
    if headers is None:
        return {name: value}
    if isinstance(headers, dict):
        return {**headers, name: value}
    if isinstance(headers, (list, tuple)):
        return [*headers, (name, value)]
    copy = type(headers)(headers)
    copy[name] = value
    return copy


def timed(fn, describe, outcome, inject=None):
    """Wraps a client's call: `describe(args, kwargs)` gives (method, url, streamed), and
    `outcome(response, streamed)` gives (status, url, size) once it returns. `inject(args, kwargs,
    mark)` gives the call's arguments with the loop guard's mark (or, with None, without one).
    Whatever goes wrong in the timing or the marking is swallowed: it must never change what the
    app's call does."""

    def marked(args, kwargs, url):
        if inject is None or not OWN_HOSTS_FILE:
            return args, kwargs
        try:
            return inject(args, kwargs, mark_for(url))
        except Exception:
            return args, kwargs

    def before(args, kwargs):
        try:
            return describe(args, kwargs)
        except Exception:
            return None, "", True

    def after(response, method, url, streamed, started, t0):
        try:
            status, final, size = outcome(response, streamed)
            report_fetch(method, final or url, started, t0, status=status, size=size)
        except Exception:
            pass

    def failed(error, method, url, started, t0):
        try:
            import urllib.error  # loaded with urllib.request; only its HTTPError is an answer

            if isinstance(error, urllib.error.HTTPError):
                report_fetch(method, url, started, t0, status=error.code)
            else:
                report_fetch(method, url, started, t0, error=error)
        except Exception:
            pass

    if inspect.iscoroutinefunction(fn):

        @functools.wraps(fn)
        async def wrapper(*args, **kwargs):
            started, t0 = time.time(), time.perf_counter()
            method, url, streamed = before(args, kwargs)
            args, kwargs = marked(args, kwargs, url)
            try:
                response = await fn(*args, **kwargs)
            except BaseException as e:
                failed(e, method, url, started, t0)
                raise
            after(response, method, url, streamed, started, t0)
            return response

    else:

        @functools.wraps(fn)
        def wrapper(*args, **kwargs):
            started, t0 = time.time(), time.perf_counter()
            method, url, streamed = before(args, kwargs)
            args, kwargs = marked(args, kwargs, url)
            try:
                response = fn(*args, **kwargs)
            except BaseException as e:
                failed(e, method, url, started, t0)
                raise
            after(response, method, url, streamed, started, t0)
            return response

    wrapper.__forkrail__ = True
    return wrapper


def arg(args, kwargs, index, name, default=None):
    return kwargs[name] if name in kwargs else args[index] if len(args) > index else default


def patch(cls, name, describe, outcome, inject=None):
    fn = getattr(cls, name, None)
    if fn is not None and not getattr(fn, "__forkrail__", False):
        setattr(cls, name, timed(fn, describe, outcome, inject))


def in_headers_kwarg(index):
    """An `inject` for a call taking `headers=` (or as positional `index`): one more header."""

    def inject(args, kwargs, mark):
        if mark is None:
            return args, kwargs
        if "headers" not in kwargs and len(args) > index:
            args = (*args[:index], with_header(args[index], LOOP_HEADER, mark), *args[index + 1 :])
        else:
            kwargs = {**kwargs, "headers": with_header(kwargs.get("headers"), LOOP_HEADER, mark)}
        return args, kwargs

    return inject


def patch_curl_cffi(module):
    # Session.request(self, method, url, params=..., ..., stream=...): get/post/... and the
    # module's own requests.get() all go through it.
    def describe(args, kwargs):
        return arg(args, kwargs, 1, "method"), arg(args, kwargs, 2, "url", ""), bool(kwargs.get("stream"))

    def outcome(r, streamed):
        return getattr(r, "status_code", None), getattr(r, "url", None), body_size(r, streamed)

    for cls in ("Session", "AsyncSession"):
        if hasattr(module, cls):
            patch(getattr(module, cls), "request", describe, outcome, in_headers_kwarg(6))


def patch_requests(module):
    # Session.send(self, prepared, **kwargs): each HTTP exchange, redirects included.
    def describe(args, kwargs):
        prepared = arg(args, kwargs, 1, "request")
        return getattr(prepared, "method", None), getattr(prepared, "url", ""), bool(kwargs.get("stream"))

    def outcome(r, streamed):
        return r.status_code, None, body_size(r, streamed)

    # Each exchange, redirects included, goes through send: a redirect to a host elsewhere drops
    # the mark the first one carried.
    def inject(args, kwargs, mark):
        prepared = arg(args, kwargs, 1, "request")
        if mark is not None:
            prepared.headers[LOOP_HEADER] = mark
        else:
            prepared.headers.pop(LOOP_HEADER, None)
        return args, kwargs

    patch(module.Session, "send", describe, outcome, inject)


def patch_httpx(module):
    def describe(args, kwargs):
        request = arg(args, kwargs, 1, "request")
        return getattr(request, "method", None), str(getattr(request, "url", "")), bool(kwargs.get("stream"))

    def outcome(r, streamed):
        return r.status_code, str(r.url), body_size(r, streamed)

    def inject(args, kwargs, mark):
        if mark is not None:
            arg(args, kwargs, 1, "request").headers[LOOP_HEADER] = mark
        return args, kwargs

    for cls in ("Client", "AsyncClient"):
        if hasattr(module, cls):
            patch(getattr(module, cls), "send", describe, outcome, inject)


def patch_aiohttp(module):
    def describe(args, kwargs):
        return arg(args, kwargs, 1, "method"), str(arg(args, kwargs, 2, "str_or_url", "")), True

    def outcome(r, streamed):
        return r.status, str(r.url), r.content_length

    def inject(args, kwargs, mark):
        if mark is None:
            return args, kwargs
        from multidict import CIMultiDict

        headers = CIMultiDict(kwargs.get("headers") or {})
        headers[LOOP_HEADER] = mark
        return args, {**kwargs, "headers": headers}

    patch(module.ClientSession, "_request", describe, outcome, inject)


def patch_urllib(module):
    def describe(args, kwargs):
        target = arg(args, kwargs, 1, "fullurl", "")
        if isinstance(target, str):
            return ("POST" if arg(args, kwargs, 2, "data") is not None else "GET"), target, True
        return target.get_method(), target.full_url, True

    def outcome(r, streamed):
        length = r.headers.get("Content-Length") if getattr(r, "headers", None) else None
        return getattr(r, "status", None), r.geturl(), int(length) if length and length.isdigit() else None

    # Unredirected: a redirect elsewhere goes without it.
    def inject(args, kwargs, mark):
        if mark is None:
            return args, kwargs
        target = arg(args, kwargs, 1, "fullurl", "")
        if isinstance(target, str):
            target = module.Request(target)
        target.add_unredirected_header(LOOP_HEADER, mark)
        if "fullurl" in kwargs:
            return args, {**kwargs, "fullurl": target}
        return (args[0], target, *args[2:]), kwargs

    patch(module.OpenerDirector, "open", describe, outcome, inject)


# Module → patch, applied when the module has been loaded.
PATCHES = {
    "curl_cffi.requests.session": patch_curl_cffi,
    "requests.sessions": patch_requests,
    "httpx._client": patch_httpx,
    "aiohttp.client": patch_aiohttp,
    "urllib.request": patch_urllib,
}


def apply_patch(module):
    try:
        PATCHES[module.__name__](module)
    except Exception as e:  # A client's internals changed: leave it be, and say so.
        print(f"forkrail: not timing {module.__name__}'s calls: {type(e).__name__}: {e}", file=sys.stderr)


class PatchingLoader(importlib.abc.Loader):
    def __init__(self, loader):
        self.loader = loader

    def create_module(self, spec):
        return self.loader.create_module(spec)

    def exec_module(self, module):
        self.loader.exec_module(module)
        apply_patch(module)

    def __getattr__(self, name):
        return getattr(self.loader, name)


class PatchOnImport(importlib.abc.MetaPathFinder):
    def find_spec(self, name, path, target=None):
        if name not in PATCHES:
            return None
        for finder in sys.meta_path:
            if finder is self or not hasattr(finder, "find_spec"):
                continue
            spec = finder.find_spec(name, path, target)
            if spec is not None:
                if spec.loader is not None and hasattr(spec.loader, "exec_module"):
                    spec.loader = PatchingLoader(spec.loader)
                return spec
        return None


def time_outgoing_calls():
    sys.meta_path.insert(0, PatchOnImport())
    for name in PATCHES:
        if name in sys.modules:
            apply_patch(sys.modules[name])


def route_header(scope):
    """The matched route's template, as a response header, when Starlette found one."""
    path = getattr(scope.get("route"), "path", None)
    if not isinstance(path, str) or not path:
        return None
    value = path.replace("\r", "").replace("\n", "")[:256]
    return (b"x-forkrail-route", value.encode("latin-1", "replace"))


class Server(uvicorn.Server):
    async def startup(self, sockets=None):
        await super().startup(sockets=sockets)
        if self.started:
            # `replies`: its answers carry REPLY_HEADER (KeepConnection).
            send({"type": "listening", "host": self.config.host, "port": self.config.port, "replies": True})


def watch_runtime():
    while True:
        time.sleep(1)
        if os.getppid() != runtime:
            os._exit(1)


# On every answer: the id of the request it answers (see the top).
REPLY_HEADER = b"x-forkrail-reply-to"


class KeepConnection:
    """Wraps the app: an exception raised after the response went out in full is logged, not
    passed to uvicorn, which would close the connection under the runtime's next request."""

    logger = logging.getLogger("uvicorn.error")

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        rid = next((v.decode("latin-1") for k, v in scope.get("headers", ()) if k == b"x-forkrail-request-id"), None)
        mark = next((v.decode("latin-1") for k, v in scope.get("headers", ()) if k == b"x-forkrail-loop"), None)
        token, marked = request_id.set(rid), loop_mark.set(mark)
        try:
            await self.handle(scope, receive, send, rid)
        finally:
            request_id.reset(token)
            loop_mark.reset(marked)
            if rid:
                end_request(rid)

    async def handle(self, scope, receive, send, rid=None):
        complete = started = False

        async def tracked(message):
            nonlocal complete, started
            if message["type"] == "http.response.start":
                started = True
                extra = [header for header in (route_header(scope),) if header]
                if rid:
                    extra.append((REPLY_HEADER, rid.encode("latin-1")))
                if extra:
                    message = {**message, "headers": [*message.get("headers", ()), *extra]}
            await send(message)
            if message["type"] == "http.response.body" and not message.get("more_body", False):
                complete = True

        try:
            await self.app(scope, receive, tracked)
        except Exception as exc:
            if started and not complete:
                raise
            # Logged as uvicorn logs it. Not answered yet: answered here as uvicorn would (500,
            # "Internal Server Error"), but stamped, and on a connection that stays open.
            self.logger.error("Exception in ASGI application\n", exc_info=exc)
            if not started:
                body = b"Internal Server Error"
                await tracked({"type": "http.response.start", "status": 500, "headers": [(b"content-type", b"text/plain; charset=utf-8"), (b"content-length", str(len(body)).encode())]})
                await tracked({"type": "http.response.body", "body": body})


# The router sends a worker at most this many requests at once.
WSGI_THREADS = 10


def wsgi(app):
    """Whether the app is a WSGI callable: a plain (not async) function, method or object called
    with exactly two arguments, `environ` and `start_response`. An ASGI 2 app is called with one."""
    if asgi3(app) or inspect.isclass(app) or not callable(app):
        return False
    if inspect.iscoroutinefunction(app) or inspect.iscoroutinefunction(getattr(app, "__call__", None)):
        return False
    try:
        parameters = inspect.signature(app).parameters.values()
    except (TypeError, ValueError):
        return False
    positional = (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
    required = [p for p in parameters if p.kind in positional and p.default is inspect.Parameter.empty]
    return len(required) == 2


def tagged_wsgi(app):
    """Wraps a WSGI app so that what it prints in its threads, and the calls it makes, are kept
    with the request: its id comes in the environ, since a thread doesn't get the event loop's
    context. The response is iterated in the request's context too, for apps that stream."""

    def call(environ, start_response):
        rid, mark = environ.get("HTTP_X_FORKRAIL_REQUEST_ID"), environ.get("HTTP_X_FORKRAIL_LOOP")
        token, marked = request_id.set(rid), loop_mark.set(mark)
        try:
            body = app(environ, start_response)
        finally:
            request_id.reset(token)
            loop_mark.reset(marked)
        return Body(body, rid, mark)

    return call


class Body:
    """A WSGI response, iterated (and closed) in its request's context."""

    def __init__(self, body, rid, mark=None):
        self.body, self.rid, self.mark = body, rid, mark
        self.iterator = None

    def __iter__(self):
        return self

    def __next__(self):
        token, marked = request_id.set(self.rid), loop_mark.set(self.mark)
        try:
            if self.iterator is None:
                self.iterator = iter(self.body)
            return next(self.iterator)
        finally:
            request_id.reset(token)
            loop_mark.reset(marked)

    def close(self):
        close = getattr(self.body, "close", None)
        if close is not None:
            token, marked = request_id.set(self.rid), loop_mark.set(self.mark)
            try:
                close()
            finally:
                request_id.reset(token)
                loop_mark.reset(marked)


def served(app):
    """What uvicorn serves: an ASGI 3 app inside KeepConnection, a WSGI app through a2wsgi (inside
    KeepConnection too), a BaseHTTPRequestHandler class through an HTTP server of its own (as
    Vercel serves a `handler`), anything else as ASGI 2 (as uvicorn would take it), through
    uvicorn's adapter inside KeepConnection."""
    from http.server import BaseHTTPRequestHandler

    if inspect.isclass(app) and issubclass(app, BaseHTTPRequestHandler):
        return KeepConnection(HandlerClass(app))
    if asgi3(app):
        return KeepConnection(app)
    if wsgi(app):
        try:
            from a2wsgi import WSGIMiddleware
        except ImportError:  # Packaged before a2wsgi was added: uvicorn's own still works.
            from uvicorn.middleware.wsgi import WSGIMiddleware
        return KeepConnection(WSGIMiddleware(tagged_wsgi(app), workers=WSGI_THREADS))
    # Anything else is what uvicorn would take for ASGI 2: served through its own adapter, inside
    # KeepConnection like the rest, so its answers are stamped too.
    from uvicorn.middleware.asgi2 import ASGI2Middleware

    return KeepConnection(ASGI2Middleware(app))


def asgi3(app):
    """Whether uvicorn's interface detection would take the app for ASGI 3."""
    if inspect.isclass(app):
        return hasattr(app, "__await__")
    if inspect.isfunction(app):
        return inspect.iscoroutinefunction(app)
    return inspect.iscoroutinefunction(getattr(app, "__call__", None))


# ---- Per-file functions (Vercel's api/*.py) ----
#
# A deployment of them is one Python app: the build writes FUNCTIONS_ENTRY (crates/cli/src/
# pyfns.rs), whose FUNCTIONS lists them, each with its file and URL paths. One ASGI app takes every
# request and runs the function the router's routes reached (`x-forkrail-function`), else the one
# the path is. A function's file is imported on its first request, so one that fails to import
# fails only its own requests. As Vercel's Python runtime finds it, a file serves:
# - a `handler` (or `Handler`) class, a subclass of http.server.BaseHTTPRequestHandler: run in an
#   HTTP server of its own on a loopback port, which each request is passed to and answered from;
# - an `app` (or `application`): ASGI (its lifespan started when it's loaded), else WSGI.

FUNCTIONS_ENTRY = ".forkrail/functions.py"
FUNCTION_HEADER = b"x-forkrail-function"
NOT_FOUND = b"The page could not be found.\n\nNOT_FOUND\n"
FAILED = b"A server error has occurred\n\nFUNCTION_INVOCATION_FAILED\n"
# Headers about one connection, never passed from one to another.
HOP_BY_HOP = {b"connection", b"keep-alive", b"proxy-connection", b"transfer-encoding", b"te", b"trailer", b"upgrade"}


def segments_of(file):
    """A function file's path segments without its extension; an `index` file is its folder."""
    parts = file.rsplit(".", 1)[0].split("/")
    if len(parts) > 1 and parts[-1] == "index":
        parts.pop()
    return parts


def trailing_dynamic(segments):
    count = 0
    for s in segments:
        count = count + 1 if dynamic_name(s) else 0
    return count


def dynamic_name(segment):
    if len(segment) > 2 and segment.startswith("[") and segment.endswith("]"):
        return segment[1:-1]
    return None


async def plain(send, status, body, error):
    await send({"type": "http.response.start", "status": status, "headers": [(b"content-type", b"text/plain; charset=utf-8"), (b"x-forkrail-error", error)]})
    await send({"type": "http.response.body", "body": body})


class Functions:
    """The ASGI app that serves a deployment's Python functions."""

    def __init__(self, functions):
        self.exact = {}
        self.dynamic = []
        for fn in functions:
            for path in fn["paths"]:
                self.exact.setdefault(path, fn["file"])
            segments = segments_of(fn["file"])
            if any(dynamic_name(s) for s in segments):
                self.dynamic.append((segments, fn["file"]))
        # As Vercel orders its routes: more segments first, then fewer dynamic ones at the end.
        self.dynamic.sort(key=lambda d: (-len(d[0]), trailing_dynamic(d[0])))
        self.files = [fn["file"] for fn in functions]
        self.by_file = {fn["file"]: fn for fn in functions}
        self.loaded = {}  # file -> Loaded
        self.loading = {}  # file -> asyncio.Lock

    def pick(self, named, path):
        """The function's file, and the dynamic segments' values when it was found by its path
        alone (the router's routes put them in the query when they name the function)."""
        if named and named in self.exact:
            return self.exact[named], []
        if path in self.exact:
            return self.exact[path], []
        got = path.strip("/").split("/")
        for segments, file in self.dynamic:
            if len(segments) != len(got):
                continue
            params = []
            for s, g in zip(segments, got):
                name = dynamic_name(s)
                if name:
                    params.append((name, g))
                elif s != g:
                    break
            else:
                return file, params
        # A router from before the header sends a rewritten request with its own path: with one
        # function, it can only be that one's.
        if not named and len(self.files) == 1:
            return self.files[0], []
        return None

    async def __call__(self, scope, receive, send):
        if scope["type"] == "lifespan":
            # Each function's own lifespan starts when it's loaded.
            while True:
                message = await receive()
                if message["type"] == "lifespan.startup":
                    await send({"type": "lifespan.startup.complete"})
                elif message["type"] == "lifespan.shutdown":
                    await send({"type": "lifespan.shutdown.complete"})
                    return
        if scope["type"] != "http":
            return
        named = None
        headers = []
        for key, value in scope["headers"]:
            if key == FUNCTION_HEADER:
                named = value.decode("latin-1")
            else:
                headers.append((key, value))
        # In place: the scope is this request's, and what the app sets in it (Starlette's route)
        # is read after it.
        scope["headers"] = headers
        picked = self.pick(named, scope["path"])
        if picked is None:
            return await plain(send, 404, NOT_FOUND, b"NOT_FOUND")
        file, params = picked
        if params:
            from urllib.parse import quote

            extra = "&".join(f"{quote(k, safe='')}={quote(v, safe='')}" for k, v in params).encode()
            query = scope.get("query_string") or b""
            scope["query_string"] = query + b"&" + extra if query else extra
        try:
            loaded = await self.load(file)
        except Exception:
            logging.getLogger("uvicorn.error").exception(f"Error importing {file}")
            return await plain(send, 500, FAILED, b"FUNCTION_INVOCATION_FAILED")
        await loaded(scope, receive, send)

    async def load(self, file):
        if file in self.loaded:
            return self.loaded[file]
        lock = self.loading.setdefault(file, asyncio.Lock())
        async with lock:
            if file not in self.loaded:
                self.loaded[file] = await Loaded.of(self.by_file[file])
        return self.loaded[file]


class Loaded:
    """A function's file, imported, and what it serves."""

    @staticmethod
    async def of(fn):
        file = fn["file"]
        name = "__forkrail_function__" + "".join(c if c.isalnum() else "_" for c in file)
        spec = importlib.util.spec_from_file_location(name, os.path.join(os.getcwd(), file))
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        try:
            spec.loader.exec_module(module)
        except BaseException:
            del sys.modules[name]
            raise
        variable = fn.get("variable") or "app"
        if not hasattr(module, variable):
            raise TypeError(f"{file} has no `{variable}` to serve")
        app = getattr(module, variable)
        # As Vercel's runtime: a `handler` class is served by an HTTP server of its own; anything
        # else is ASGI or WSGI by its shape.
        from http.server import BaseHTTPRequestHandler

        if variable.lower() == "handler" and isinstance(app, type) and issubclass(app, BaseHTTPRequestHandler):
            return HandlerClass(app)
        kind, app = app_type(app, file)
        if kind == "wsgi":
            try:
                from a2wsgi import WSGIMiddleware
            except ImportError:
                from uvicorn.middleware.wsgi import WSGIMiddleware
            return Asgi(WSGIMiddleware(tagged_wsgi(app), workers=WSGI_THREADS), None)
        state = {}
        await lifespan(app, state)
        return Asgi(app, state)


def app_type(app, file):
    """ASGI or WSGI, as Vercel's runtime tells them apart (`resolver.detect_app_type`): an `asgi`
    attribute that can be called is the ASGI app; an async callable with three required positional
    parameters is ASGI, a callable with two is WSGI."""
    asgi = getattr(app, "asgi", None)
    if asgi is not None and callable(asgi):
        return "asgi", asgi
    is_async = inspect.iscoroutinefunction(app)
    if not is_async and callable(app):
        is_async = inspect.iscoroutinefunction(app.__call__)
    count = positional_count(app)
    if is_async and count == 3:
        return "asgi", app
    if count == 2:
        return "wsgi", app
    raise RuntimeError(f"{file}: could not determine the application interface (ASGI or WSGI) of what it serves")


def positional_count(app):
    """The required positional parameters of a callable (a class's __call__ for an instance)."""
    try:
        target = app if inspect.isfunction(app) or inspect.ismethod(app) else app.__call__
        parameters = inspect.signature(target).parameters.values()
    except (TypeError, ValueError, AttributeError):
        return -1
    positional = (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
    return sum(1 for p in parameters if p.kind in positional and p.default is inspect.Parameter.empty)


class Asgi:
    def __init__(self, app, state):
        self.app, self.state = app, state

    async def __call__(self, scope, receive, send):
        if self.state is not None:
            scope["state"] = dict(self.state)
        await self.app(scope, receive, send)


async def lifespan(app, state):
    """Start an ASGI app's lifespan, as uvicorn's "auto" does: an app that doesn't take lifespan
    messages is served without one. Runs on in the background (it's never shut down)."""
    inbox = asyncio.Queue()
    started = asyncio.get_running_loop().create_future()

    async def receive():
        return await inbox.get()

    async def send(message):
        if message["type"] in ("lifespan.startup.complete", "lifespan.startup.failed") and not started.done():
            started.set_result(message)

    async def run():
        try:
            await app({"type": "lifespan", "asgi": {"version": "3.0", "spec_version": "2.0"}, "state": state}, receive, send)
        except BaseException as e:  # An app without lifespan raises on the message; that's fine.
            if not started.done():
                started.set_result({"type": "unsupported", "error": e})
            return
        if not started.done():
            started.set_result({"type": "unsupported"})

    await inbox.put({"type": "lifespan.startup"})
    task = asyncio.get_running_loop().create_task(run())
    LIFESPANS.append(task)
    message = await started
    if message["type"] == "lifespan.startup.failed":
        raise RuntimeError(f"the app's startup failed: {message.get('message', '')}")


# Kept so the lifespans' tasks aren't collected while they run.
LIFESPANS = []


class HandlerClass:
    """A BaseHTTPRequestHandler subclass, served as Vercel serves it: by an HTTP server of its own
    on a loopback port, each request passed to it with http.client and the answer passed back."""

    def __init__(self, cls):
        from http.server import ThreadingHTTPServer

        class Tagged(cls):
            # The handler runs in a thread of the server's, which doesn't have the request's
            # context: what it prints is kept with the request by the id in its headers.
            def parse_request(self):
                ok = super().parse_request()
                if ok:
                    request_id.set(self.headers.get("x-forkrail-request-id"))
                    loop_mark.set(self.headers.get("x-forkrail-loop"))
                return ok

            def finish(self):
                try:
                    super().finish()
                finally:
                    rid = request_id.get()
                    if rid:
                        end_request(rid)

        Tagged.__name__, Tagged.__qualname__, Tagged.__module__ = cls.__name__, cls.__qualname__, cls.__module__
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Tagged)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    async def __call__(self, scope, receive, send):
        if scope["type"] == "lifespan":
            # Served as the whole app: it has no startup or shutdown of its own.
            while True:
                message = await receive()
                if message["type"] == "lifespan.startup":
                    await send({"type": "lifespan.startup.complete"})
                elif message["type"] == "lifespan.shutdown":
                    await send({"type": "lifespan.shutdown.complete"})
                    return
        if scope["type"] != "http":
            return
        body = bytearray()
        while True:
            message = await receive()
            if message["type"] == "http.disconnect":
                return
            body += message.get("body", b"")
            if not message.get("more_body", False):
                break
        loop = asyncio.get_running_loop()
        target = scope.get("raw_path") or scope["path"].encode()
        if scope.get("query_string"):
            target = target + b"?" + scope["query_string"]

        def relay(message):
            asyncio.run_coroutine_threadsafe(send(message), loop).result()

        def exchange():
            import http.client

            conn = http.client.HTTPConnection("127.0.0.1", self.port)
            try:
                conn.putrequest(scope["method"], target.decode("latin-1"), skip_host=True, skip_accept_encoding=True)
                for key, value in scope["headers"]:
                    if key not in HOP_BY_HOP and key != b"content-length":
                        conn.putheader(key.decode("latin-1"), value.decode("latin-1"))
                conn.putheader("Content-Length", str(len(body)))
                conn.putheader("Connection", "close")
                conn.endheaders(bytes(body) if body else None)
                response = conn.getresponse()
                headers = [(k.lower().encode("latin-1"), v.encode("latin-1")) for k, v in response.getheaders()]
                headers = [(k, v) for k, v in headers if k not in HOP_BY_HOP and k not in (b"server", b"date")]
                relay({"type": "http.response.start", "status": response.status, "headers": headers})
                while True:
                    chunk = response.read1(65536) if scope["method"] != "HEAD" else b""
                    if not chunk:
                        break
                    relay({"type": "http.response.body", "body": chunk, "more_body": True})
                relay({"type": "http.response.body", "body": b""})
            finally:
                conn.close()

        await loop.run_in_executor(None, exchange)


def load_functions(path):
    """FUNCTIONS from the build's FUNCTIONS_ENTRY."""
    spec = importlib.util.spec_from_file_location("__forkrail_functions__", os.path.join(os.getcwd(), path))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.FUNCTIONS


def main():
    path, _, attr = sys.argv[1].partition(":")
    module = path.removesuffix(".py").replace("/", ".")
    sys.path.insert(0, os.getcwd())
    threading.Thread(target=watch_runtime, daemon=True).start()
    # Before the app is imported and uvicorn sets up logging, so their handlers write through it.
    tag_output()
    time_outgoing_calls()
    if path == FUNCTIONS_ENTRY:
        app = Functions(load_functions(path))
    else:
        app = import_from_string(f"{module}:{attr or 'app'}")
    config = uvicorn.Config(
        served(app),
        host="127.0.0.1",
        port=int(os.environ["PORT"]),
        timeout_keep_alive=24 * 3600,
        # h11's own limit is 16 KiB of headers, under what the router lets through (about 128 KB,
        # as on Vercel): past it uvicorn would answer 400 itself, outside the app.
        h11_max_incomplete_event_size=int(os.environ.get("FORKRAIL_MAX_HEADER_BYTES") or 0) or None,
        # The router writes the access log.
        access_log=False,
        log_level="warning",
    )
    Server(config).run()


main()
