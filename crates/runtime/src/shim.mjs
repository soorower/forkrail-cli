// forkrail shim, preloaded into the user's Node app with `node --import`. The app's code is
// never changed. It talks to the runtime with JSON lines on the fd in FORKRAIL_CONTROL_FD,
// written synchronously so nothing is lost when the process dies right after.
//
// - Reports where the app's HTTP server listens. That tells the runtime the app is ready and
//   where to send requests, whatever port the app chose.
// - Turns off that server's keep-alive timeout. The runtime is its only client, and Node's
//   default (5 s) would close pooled connections under it, at once after Lambda thaws a frozen
//   environment.
// - Runs each request in an AsyncLocalStorage context holding the router's request id.
// - Stamps each answer with the id of the request it answers (`x-forkrail-reply-to`), which the
//   runtime checks and takes off: bytes an app writes outside an answer (to its socket, after the
//   answer ended) would otherwise be read as the next request's answer on that connection, which
//   may be another visitor's.
// - Reports uncaught errors with that request id, without changing how the app handles them.
// - Tags what the app prints with that request: each line written to stdout or stderr while a
//   request runs starts with its id between \x1e and \x1f, which the runtime takes off and keeps
//   with the line. Lines are put together per request, so requests printing at once don't mix,
//   and so are lines written outside any request: a line's start held back until its end comes
//   (or a second passes), so no request's line lands in the middle of it and is lost to it.
// - Reports each outgoing HTTP call (fetch and undici, node:http and node:https) with the request
//   it was made for, as a `fetch` message: method, URL, status or error, start and duration. It
//   listens on Node's diagnostics channels, so nothing is patched. The router masks the URL.
// - Carries the loop guard's mark (`x-forkrail-loop`, from the request being served) on each
//   outgoing call to one of the install's own hosts, and on no other call, so the router can stop
//   a chain of calls back to the install (crates/router/src/loops.rs). The hosts are in the file
//   FORKRAIL_OWN_HOSTS_FILE names, which the runtime rewrites when the router's list changes.
// - Exits if the runtime dies. On Lambda the environment goes with it anyway, but a local
//   router kills workers with SIGKILL, which would leave the app running and holding its port.
// - Puts Vercel's cache purges and `addCacheTag` in the request context (`@vercel/functions`'
//   `invalidateByTag`, `dangerouslyDeleteByTag`, by source image, and `addCacheTag`): purges go to
//   the runtime (`purge.rs`), tags onto the answer's `Vercel-Cache-Tag` as its head goes out.
// - Lets a server prime itself before the runtime is told it's ready (`forkrail.prime`, below).
// - Gives Next.js its platform's `waitUntil` (`@next/request-context`), tracked like Vercel's.
// - Serves an app its entry exports, for a framework preset's launcher (crates/cli/src/nodeapp.rs),
//   when the entry didn't listen itself: Vercel takes an Express (or Hono, Fastify, Koa, Elysia,
//   H3) app either way. An app with `fetch` (Hono, Elysia, H3 2, `{ fetch }`) is served through
//   Web requests and responses; Fastify with its own `listen`; one with `listen` (Express, Koa)
//   with that; a plain `(req, res)` function with node:http.

import { AsyncLocalStorage } from 'node:async_hooks';
import { createHash } from 'node:crypto';
import diagnostics from 'node:diagnostics_channel';
import { readFileSync, statSync, writeSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import querystring from 'node:querystring';
import { PassThrough, Readable } from 'node:stream';

const control = Number(process.env.FORKRAIL_CONTROL_FD) || null;
const runtime = process.ppid;
const requests = new AsyncLocalStorage();
let server = null;

function send(message) {
  if (control === null) return;
  try {
    writeSync(control, JSON.stringify(message) + '\n');
  } catch {
    // The runtime is gone, and the environment with it. Don't fail the app over it.
  }
}

// Vercel's per-request context, which its packages read from this global (`@vercel/functions`'
// `waitUntil`, `attachDatabasePool`, `@vercel/analytics/server`, the AI SDK's gateway, flags,
// cache purges and tags): the request's `headers` (lowercase names), its `url`, `waitUntil`,
// `purge` and `addCacheTag`. Outside a request `get()` gives nothing, as Vercel's does: those
// packages read it as `get?.() ?? {}`, and `@vercel/otel` checks for none before it reads
// `headers.host` (an empty object there crashed every app that registers it, at its first span).
const vercelContext = new AsyncLocalStorage();
globalThis[Symbol.for('@vercel/request-context')] ??= { get: () => vercelContext.getStore() };
// `waitUntil` is below, with the Edge functions.

// Next.js asks its platform for `waitUntil` through a global of its own (15.0 on): `after()`,
// and what its server does behind an answer (regenerating a stale page, writing its cache). With
// it Next's own server hands that work here, and the runtime keeps the invocation open (and the
// router the worker) until it settles; without it Next would track the work only in its own
// process, which Lambda freezes as soon as the invocation returns. A server that calls Next's
// handlers itself (crates/runtime/src/next) passes the same function as `ctx.waitUntil`, from
// `globalThis[Symbol.for('forkrail.waitUntil')]`.
globalThis[Symbol.for('@next/request-context')] ??= { get: () => ({ waitUntil }) };
globalThis[Symbol.for('forkrail.waitUntil')] = waitUntil;

// Purges by tag go to the runtime on the loopback port it names in FORKRAIL_PURGE, with its
// secret (`127.0.0.1:<port>:<secret>`, taken out of the app's environment), which sends them to
// the router as this invocation's worker: they purge this deployment's project and environment,
// as Vercel's do. One line of JSON each way; the promise settles when the router has it (and, on a
// router that isn't home, when home has it). A raw socket, so it isn't reported as the app's own
// outgoing call. Without FORKRAIL_PURGE (the app run some other way) purges do nothing, as off
// Vercel.
const PURGE_AT = process.env.FORKRAIL_PURGE ?? '';
delete process.env.FORKRAIL_PURGE;
const PURGE_TIMEOUT_MS = 30_000;

function purgeCall(op, value, { src = false, options } = {}) {
  const [host, port, secret] = PURGE_AT.split(':');
  if (!secret) return Promise.resolve();
  const given = (Array.isArray(value) ? value : [value]).map(String);
  // A comma separates two tags; a source image may have commas of its own.
  const tags = src ? given : given.flatMap((tag) => tag.split(','));
  const line = { secret, op, tags };
  if (src) line.src_images = true;
  const deadline = Number(options?.revalidationDeadlineSeconds);
  if (op === 'delete' && Number.isFinite(deadline) && deadline > 0) line.deadline_s = Math.floor(deadline);
  return new Promise((resolve, reject) => {
    const socket = net.connect({ host, port: Number(port) });
    let answer = '';
    socket.setEncoding('utf8');
    socket.setTimeout(PURGE_TIMEOUT_MS, () => socket.destroy(new Error('the purge timed out')));
    socket.on('connect', () => socket.end(JSON.stringify(line) + '\n'));
    socket.on('data', (chunk) => { answer += chunk; });
    socket.on('error', (error) => reject(new Error(`purge failed: ${error.message}`)));
    socket.on('close', () => {
      let reply = null;
      try {
        reply = JSON.parse(answer.split('\n')[0]);
      } catch {
        // No answer: below.
      }
      if (reply?.ok) resolve();
      else reject(new Error(`purge failed: ${reply?.error ?? 'no answer from the runtime'}`));
    });
  });
}

const purge = {
  invalidateByTag: (tag) => purgeCall('invalidate', tag),
  dangerouslyDeleteByTag: (tag, options) => purgeCall('delete', tag, { options }),
  invalidateBySrcImage: (src) => purgeCall('invalidate', src, { src: true }),
  dangerouslyDeleteBySrcImage: (src, options) => purgeCall('delete', src, { src: true, options }),
};

// `addCacheTag`: tags for the answer to this request, added to its `Vercel-Cache-Tag` as its head
// goes out (`writeHead`, which Node calls for an implicit head too), whichever way the app sets
// that header. Tags added after the head has gone can't reach it, as a header can't.
const CACHE_TAG = 'vercel-cache-tag';

function cacheTagging(res) {
  const tags = new Set();
  const { writeHead } = res;
  res.writeHead = function (status, ...rest) {
    if (tags.size > 0) {
      // `writeHead(status, [reason], [headers])`: headers given there win over `setHeader`'s.
      const at = rest.findIndex((r) => r !== null && typeof r === 'object');
      const given = at === -1 ? null : rest[at];
      if (Array.isArray(given)) {
        given.push(CACHE_TAG, [...tags].join(','));
      } else {
        const key = given ? Object.keys(given).find((k) => k.toLowerCase() === CACHE_TAG) : undefined;
        const before = key !== undefined ? given[key] : this.getHeader(CACHE_TAG);
        const all = [...new Set([...String(before ?? '').split(',').map((t) => t.trim()).filter(Boolean), ...tags])].join(',');
        if (key !== undefined) given[key] = all;
        else this.setHeader(CACHE_TAG, all);
      }
    }
    return writeHead.call(this, status, ...rest);
  };
  return (tag) => {
    for (const one of (Array.isArray(tag) ? tag : [tag]).map(String).flatMap((t) => t.split(','))) {
      const trimmed = one.trim();
      if (trimmed) tags.add(trimmed);
    }
    return Promise.resolve();
  };
}

const REPLY_HEADER = 'x-forkrail-reply-to';
const { emit } = http.Server.prototype;
http.Server.prototype.emit = function (event, ...args) {
  if (event === 'request') {
    const [req, res] = args;
    const id = req.headers['x-forkrail-request-id'];
    if (typeof id === 'string' && !res.headersSent) res.setHeader(REPLY_HEADER, id);
    const proto = req.headers['x-forwarded-proto'] === 'http' ? 'http' : 'https';
    const addCacheTag = args[1] instanceof http.ServerResponse ? cacheTagging(args[1]) : () => Promise.resolve();
    const context = { waitUntil, purge, addCacheTag, headers: req.headers, url: `${proto}://${req.headers.host ?? 'localhost'}${req.url ?? '/'}` };
    return requests.run(id, () => vercelContext.run(context, () => emit.call(this, event, ...args)));
  }
  // The first HTTP server to listen is the app. Any other server listens as it asked.
  if (event === 'listening' && server === null) {
    server = this;
    this.keepAliveTimeout = 0;
    const address = this.address();
    // `replies`: its answers carry REPLY_HEADER.
    const listening = typeof address === 'string'
      ? { type: 'listening', path: address, replies: true }
      : { type: 'listening', host: address.address, port: address.port, replies: true };
    primed(address).then((prime) => send(prime ? { ...listening, prime } : listening));
  }
  return emit.call(this, event, ...args);
};

// Priming: work a server wants done before the runtime is told the app is ready, because it would
// otherwise fall to the first request. On Lambda the start runs at full speed whatever the
// function's size, and a request at the size's share of a core (0.36 at 640 MB), so what a
// framework loads and compiles for its first render costs a visitor two to three times what it
// costs here. A server entry sets `globalThis[Symbol.for('forkrail.prime')]` to a function before
// it listens; it's called once the server listens, with `{ host, port, origin, deadline }` (the
// server's own loopback address; `deadline` in ms since the epoch), and may ask its own server for
// pages or call its handlers. The runtime hears "ready" when the function settles, throws, or
// reaches the deadline, whichever comes first: priming can make a start slower, never fail it.
// The deadline keeps the whole start inside what the runtime allows an app (9 s; Lambda's init
// ends at 10): PRIME_END_MS after the process began, and never more than PRIME_MAX_MS of priming.
// A function that returns an object has it kept with the outcome, for the runtime's log.
// FORKRAIL_PRIME=0 in the project's env vars turns it off.
const PRIME_END_MS = 5000;
const PRIME_MAX_MS = Math.min(Number(process.env.FORKRAIL_PRIME_MAX_MS) || 3000, 4000);

async function primed(address) {
  const prime = globalThis[Symbol.for('forkrail.prime')];
  if (typeof prime !== 'function' || process.env.FORKRAIL_PRIME === '0' || typeof address === 'string') return null;
  const began = performance.now();
  const budget = Math.min(PRIME_MAX_MS, PRIME_END_MS - process.uptime() * 1000);
  if (budget < 50) return { ms: 0, outcome: 'skipped' };
  const host = address.family === 'IPv6' ? (address.address === '::' ? '127.0.0.1' : `[${address.address}]`) : address.address === '0.0.0.0' ? '127.0.0.1' : address.address;
  let timer;
  const late = new Promise((resolve) => {
    timer = setTimeout(() => resolve('timeout'), budget);
  });
  let outcome;
  try {
    const work = Promise.resolve(prime({ host, port: address.port, origin: `http://${host}:${address.port}`, deadline: Date.now() + budget }));
    // Still running past the deadline, it goes on behind the first requests; its failure then
    // is its own.
    work.catch(() => {});
    outcome = await Promise.race([work.then((did) => (did && typeof did === 'object' ? `ok ${JSON.stringify(did)}`.slice(0, 300) : 'ok')), late]);
  } catch (error) {
    outcome = 'failed';
    console.error('forkrail: priming failed (the app starts without it):', error);
  }
  clearTimeout(timer);
  return { ms: Math.round(performance.now() - began), outcome };
}

// Whether anything asked to listen yet: the entry did itself, or the launcher shouldn't serve it.
let listenAsked = false;
const serverListen = http.Server.prototype.listen;
http.Server.prototype.listen = function (...args) {
  // A per-file function's module loading (below): captured, not listening.
  if (capturing !== null) {
    capturing.push(this);
    const callback = args.find((a) => typeof a === 'function');
    if (callback) process.nextTick(callback);
    return this;
  }
  listenAsked = true;
  return serverListen.apply(this, args);
};

const PORT = Number(process.env.PORT) || 3000;
const HOST = '127.0.0.1';

// The app a module exports: its default (a CommonJS module's exports), or an `app` or `handler`.
function exported(module) {
  for (const candidate of [module?.default?.default, module?.default, module?.app, module?.handler]) {
    if (candidate && (typeof candidate === 'function' || typeof candidate === 'object')) {
      if (typeof candidate.fetch === 'function' || typeof candidate.listen === 'function' || typeof candidate === 'function') return candidate;
    }
  }
  return null;
}

// A Node request as a Web Request, and a Web Response written back.
async function viaFetch(app, req, res) {
  const url = new URL(req.url ?? '/', `http://${req.headers.host ?? HOST}`);
  const headers = new Headers();
  for (const [name, value] of Object.entries(req.headers)) {
    if (Array.isArray(value)) for (const v of value) headers.append(name, v);
    else if (value !== undefined) headers.set(name, value);
  }
  const body = req.method === 'GET' || req.method === 'HEAD' ? undefined : Readable.toWeb(req);
  let response;
  try {
    response = await app.fetch(new Request(url, { method: req.method, headers, body, duplex: body ? 'half' : undefined }), {}, { waitUntil, passThroughOnException() {} });
  } catch (error) {
    console.error(error);
    if (!res.headersSent) res.writeHead(500, { 'content-type': 'text/plain' });
    res.end('Internal Server Error');
    return;
  }
  const cookies = response.headers.getSetCookie?.() ?? [];
  for (const [name, value] of response.headers) {
    if (name !== 'set-cookie') res.setHeader(name, value);
  }
  if (cookies.length) res.setHeader('set-cookie', cookies);
  res.statusCode = response.status;
  if (!response.body || req.method === 'HEAD') {
    res.end();
    return;
  }
  const stream = Readable.fromWeb(response.body);
  stream.on('error', (error) => {
    console.error(error);
    res.destroy(error);
  });
  stream.pipe(res);
}

globalThis[Symbol.for('forkrail.serve')] = (module, entry) => {
  // After the entry's own start-up: an app that listened, or asked to, is left alone.
  setImmediate(() => {
    if (listenAsked || server !== null) return;
    const app = exported(module);
    if (app === null) {
      console.error(`${entry} neither listened on PORT nor exported an app (export default app, or call app.listen(process.env.PORT))`);
      process.exit(1);
    }
    if (typeof app.fetch === 'function' && typeof app.listen !== 'function') {
      http.createServer((req, res) => void viaFetch(app, req, res)).listen(PORT, HOST);
    } else if (typeof app.ready === 'function' && typeof app.listen === 'function' && app.server instanceof http.Server) {
      // Fastify: listen({ port, host }) after its plugins are ready.
      app.listen({ port: PORT, host: HOST }).catch((error) => {
        console.error(error);
        process.exit(1);
      });
    } else if (typeof app.listen === 'function') {
      app.listen(PORT, HOST);
    } else if (typeof app.fetch === 'function') {
      http.createServer((req, res) => void viaFetch(app, req, res)).listen(PORT, HOST);
    } else {
      http.createServer(app).listen(PORT, HOST);
    }
  });
};

// ---- Per-file functions (Vercel's `api/*.js`, `api/*.ts`) ----
//
// A deployment of them is one Node app: the build writes `.forkrail/functions.mjs`
// (crates/cli/src/nodefns.rs), which hands this its functions, each with the file it came from,
// its URL paths and a loader. One server takes every request and runs the function the router's
// routes reached (`x-forkrail-function`), else the one the path is. A function's module is loaded
// on its first request, so one that fails to load fails only its own requests.
//
// Each runs as on Vercel (`@vercel/node`, Apache-2.0, whose shapes and helpers this follows):
// - Web handlers: a module exporting `GET`, `POST`, ... (a method it doesn't export is 405), or
//   `export default { fetch }`. They get a Web `Request` and return a `Response`.
// - Anything else exported by default is a Node `(req, res)` handler, with Vercel's helpers:
//   `req.query`, `req.cookies`, `req.body`, `res.status()`, `res.send()`, `res.json()` and
//   `res.redirect()`. An app with its own `listen` (Express) is called as it is.

const FUNCTION_HEADER = 'x-forkrail-function';
const HTTP_METHODS = ['GET', 'HEAD', 'OPTIONS', 'POST', 'PUT', 'DELETE', 'PATCH'];
const NOT_FOUND = 'The page could not be found.\n\nNOT_FOUND\n';
const FAILED = 'A server error has occurred\n\nFUNCTION_INVOCATION_FAILED\n';

// A function file's path segments without its extension; an `index` file is its folder.
function segmentsOf(file) {
  const parts = file.replace(/\.[^/.]+$/, '').split('/');
  if (parts.length > 1 && parts[parts.length - 1] === 'index') parts.pop();
  return parts;
}

const dynamicName = (segment) => (segment.startsWith('[') && segment.endsWith(']') && segment.length > 2 ? segment.slice(1, -1) : null);

globalThis[Symbol.for('forkrail.functions')] = (functions, { middleware = [] } = {}) => {
  const exact = new Map();
  const dynamic = [];
  // The project's Routing Middleware: named by the router only, never a path anyone can ask for.
  const named = new Map(middleware.map((fn) => [fn.name, { ...fn, file: fn.file ?? fn.name, edge: true }]));
  for (const fn of functions) {
    for (const path of fn.paths) if (!exact.has(path)) exact.set(path, fn);
    const segments = segmentsOf(fn.file);
    if (segments.some((s) => dynamicName(s) !== null)) dynamic.push({ segments, fn });
  }
  // As Vercel orders its routes: more segments first, then fewer dynamic ones at the end.
  const dynamicCount = (segments) => segments.reduce((n, s) => (dynamicName(s) !== null ? n + 1 : 0), 0);
  dynamic.sort((a, b) => b.segments.length - a.segments.length || dynamicCount(a.segments) - dynamicCount(b.segments));
  const distinct = new Set(functions);

  // The function for a request, and the dynamic segments' values when it was found by its path
  // alone (the router's routes put them in the query when they name the function).
  function pick(asked, path) {
    if (asked && named.has(asked)) return { fn: named.get(asked), params: [] };
    if (asked && exact.has(asked)) return { fn: exact.get(asked), params: [] };
    let decoded = path;
    try {
      decoded = decodeURIComponent(path);
    } catch {}
    for (const candidate of [path, decoded]) if (exact.has(candidate)) return { fn: exact.get(candidate), params: [] };
    const got = path.replace(/^\/+|\/+$/g, '').split('/');
    for (const { segments, fn } of dynamic) {
      if (segments.length !== got.length) continue;
      const params = [];
      if (segments.every((s, i) => (dynamicName(s) !== null ? params.push([dynamicName(s), got[i]]) : s === got[i]))) return { fn, params };
    }
    // A router from before the header sends a rewritten request with its own path: with one
    // function, it can only be that one's.
    if (!asked && distinct.size === 1) return { fn: functions[0], params: [] };
    return null;
  }

  http.createServer((req, res) => void serveFunction(req, res, pick)).listen(PORT, HOST);
};

// ---- A Build Output's functions (Vercel's `.vercel/output/functions`) ----
//
// The build writes `.forkrail/functions.mjs` (crates/cli/src/build_output.rs): each real function
// with the paths it answers at (its `.func` names, symlinked ones included) and a loader for its
// handler. The router's routes name the function in `x-forkrail-function`. Each handler runs as
// Vercel's Node launcher runs it: a Web `fetch` or method export, or a Node `(req, res)`, with
// Vercel's helpers only when its `.vc-config.json` asks (`shouldAddHelpers`). One process runs them
// all, in the working directory of the function most routes reach, with their own env vars.
globalThis[Symbol.for('forkrail.buildOutput')] = ({ cwd, env, functions }) => {
  // NODE_ENV is production already unless the project set it (the runtime starts every Node app
  // so, as Vercel runs its functions); a function's own `environment` may still set it.
  for (const [key, value] of Object.entries(env ?? {})) process.env[key] = value;
  if (cwd) process.chdir(cwd);
  const exact = new Map();
  const byName = new Map();
  for (const fn of functions) {
    fn.file = fn.name;
    byName.set(fn.name, fn);
    for (const path of fn.paths) if (!exact.has(path)) exact.set(path, fn);
  }
  // Libraries tell Vercel's Edge runtime by this global; only an app that is all Edge functions
  // gets it, so a Node function beside them still takes its Node paths.
  if (functions.length && functions.every((fn) => fn.edge)) globalThis.EdgeRuntime ??= 'edge-runtime';
  // A function only a route's `middlewarePath` names has no paths: the router names it.
  const pick = (named, path) => {
    const fn = (named && (exact.get(named) || byName.get(named))) || exact.get(path);
    return fn ? { fn, params: [] } : null;
  };
  http.createServer((req, res) => void serveFunction(req, res, pick)).listen(PORT, HOST);
};

// Take a header the router sent for the shim off the request, as the app never sees it.
function takeHeader(req, name) {
  const value = req.headers[name];
  delete req.headers[name];
  for (let i = req.rawHeaders.length - 2; i >= 0; i -= 2) {
    if (req.rawHeaders[i].toLowerCase() === name) req.rawHeaders.splice(i, 2);
  }
  return value;
}

async function serveFunction(req, res, pick) {
  const named = takeHeader(req, FUNCTION_HEADER);
  // The router's call of a Routing Middleware (crates/router/src/proxy/middleware.rs).
  const middleware = takeHeader(req, MIDDLEWARE_HEADER) === '1';
  const url = req.url ?? '/';
  const at = url.indexOf('?');
  const path = at === -1 ? url : url.slice(0, at);
  const found = pick(typeof named === 'string' ? named : null, path);
  if (found === null) {
    res.writeHead(404, { 'content-type': 'text/plain; charset=utf-8', 'x-forkrail-error': 'NOT_FOUND' });
    res.end(NOT_FOUND);
    return;
  }
  if (found.params.length) {
    const extra = found.params.map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`).join('&');
    req.url = at === -1 ? `${path}?${extra}` : `${url}&${extra}`;
  }
  const code = middleware ? 'MIDDLEWARE_INVOCATION_FAILED' : found.fn.edge ? 'EDGE_FUNCTION_INVOCATION_FAILED' : 'FUNCTION_INVOCATION_FAILED';
  let handler;
  try {
    if (middleware || found.fn.edge) {
      found.fn.edgeHandler ??= loadEdge(found.fn);
      handler = await found.fn.edgeHandler;
    } else {
      found.fn.handler ??= loadFunction(found.fn);
      handler = await found.fn.handler;
    }
  } catch (error) {
    // Tried again on the next request.
    found.fn.handler = undefined;
    found.fn.edgeHandler = undefined;
    console.error(`Error loading ${found.fn.file}:`, error);
    return failed(res, code);
  }
  try {
    // The request's Vercel context is already set (the server's `request` event, above).
    await (middleware || found.fn.edge ? viaEdge(handler, req, res, { middleware, waitUntil }) : handler(req, res));
  } catch (error) {
    if (error instanceof ApiError) return sendError(res, error.statusCode, error.message);
    console.error(error);
    failed(res, code);
  }
}

function failed(res, code = 'FUNCTION_INVOCATION_FAILED') {
  if (res.headersSent) return res.destroy();
  res.writeHead(500, { 'content-type': 'text/plain; charset=utf-8', 'x-forkrail-error': code });
  res.end(`A server error has occurred\n\n${code}\n`);
}

// ---- Edge functions and Routing Middleware ----
//
// Vercel's Edge runtime functions (`.vc-config.json` `runtime: "edge"`) and Routing Middleware
// (Edge or Node) run here on Node, as Vercel's own middleware now does by default. Each is an ES
// module whose default export takes a Web `Request` and a context with `waitUntil`, or one with
// method exports (`GET`, ...), as Vercel's edge handler template reads it. A middleware that
// returns nothing goes on (`x-middleware-next: 1`); an Edge function must return a Response.
// WebAssembly the function's config binds by name (`wasm`) is a global `WebAssembly.Module` of
// that name, as Vercel's Edge runtime gives it.

const MIDDLEWARE_HEADER = 'x-forkrail-middleware';

// Work a handler hands `waitUntil` (its context's, or `@vercel/functions`' through the request
// context above) goes on after its response, and the runtime keeps the invocation open until it
// settles; its failure is reported, as Vercel's runtime does, rather than left an unhandled
// rejection that ends the process (and every request with it).
function waitUntil(promise) {
  send({ type: 'wait', delta: 1 });
  Promise.resolve(promise)
    .catch((error) => console.error('waitUntil:', error))
    .finally(() => send({ type: 'wait', delta: -1 }));
}


async function loadEdge(fn) {
  for (const { name, url } of fn.wasm ?? []) {
    const { readFile } = await import('node:fs/promises');
    globalThis[name] ??= new WebAssembly.Module(await readFile(new URL(url)));
  }
  const module = await fn.load();
  let entry = module;
  for (let i = 0; i < 5 && typeof entry?.default === 'object' && entry.default !== null; i++) entry = entry.default;
  if (typeof module.default === 'function') return module.default;
  if (typeof entry?.fetch === 'function') return entry.fetch.bind(entry);
  if (HTTP_METHODS.some((m) => typeof module[m] === 'function')) {
    return (request, context) => (typeof module[request.method] === 'function' ? module[request.method](request, context) : new Response(null, { status: 405 }));
  }
  throw new Error(`No default or HTTP-named export was found in ${fn.file}. Add one to handle requests.`);
}

// An Edge handler or middleware: the request as a Web `Request` (its URL as the visitor asked
// for it), and its `Response` written back, streamed.
async function viaEdge(fn, req, res, { middleware, waitUntil }) {
  await viaWeb(async (request, context) => {
    const response = await fn(request, context);
    if (response === undefined || response === null) {
      if (middleware) return new Response(null, { headers: { 'x-middleware-next': '1' } });
      throw new Error('Edge Function did not return a response.');
    }
    return response;
  }, req, res, waitUntil);
}

// Servers a function's module asks to listen while it loads, which are captured instead (as on
// Vercel, the port is ignored) and handed its requests. Loads go one at a time, so each capture is
// the right module's.
let capturing = null;
let loads = Promise.resolve();
// How long to wait for a module that exports nothing to call `listen`.
const LISTEN_WAIT_MS = 1000;

function loadFunction(fn) {
  const loaded = loads.then(async () => {
    capturing = [];
    try {
      const module = await fn.load();
      const handler = handlerOf(module, fn.helpers, fn.web);
      if (handler !== null) return handler;
      const deadline = Date.now() + LISTEN_WAIT_MS;
      while (capturing.length === 0 && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 10));
      if (capturing.length === 0) throw new Error("Can't detect way to handle request: export a default function (req, res), GET, POST, ... (Web handlers) or { fetch }");
      const server = capturing[0];
      return (req, res) =>
        new Promise((resolve) => {
          res.on('close', resolve);
          server.emit('request', req, res);
        });
    } finally {
      capturing = null;
    }
  });
  loads = loaded.catch(() => {});
  return loaded;
}

// The handler a function's module exports, as `@vercel/node` reads it; null when it exports none.
// `helpers`: whether a Node handler gets Vercel's helpers (a Build Output says per function; `api/`
// functions get them unless NODEJS_HELPERS=0, as on Vercel). `webApi`: a Build Output's `useWebApi`,
// a default export that takes a Web `Request` (React Router's functions).
function handlerOf(module, helpers, webApi = false) {
  const config = module?.config ?? module?.default?.config;
  let listener = module;
  // TypeScript compiled to CommonJS nests its default export.
  for (let i = 0; i < 5; i++) if (listener?.default) listener = listener.default;
  const web = (fns) => {
    const byMethod = {};
    for (const method of HTTP_METHODS) byMethod[method] = typeof fns[method] === 'function' ? (req, res) => viaWeb(fns[method], req, res) : methodNotAllowed;
    // A method outside these fails, as on Vercel.
    return (req, res) => byMethod[req.method ?? 'GET'](req, res);
  };
  if (listener && typeof listener.fetch === 'function') {
    const fetch = listener.fetch.bind(listener);
    return web(Object.fromEntries(HTTP_METHODS.map((m) => [m, fetch])));
  }
  if (listener && HTTP_METHODS.some((m) => typeof listener[m] === 'function')) return web(listener);
  if (typeof listener !== 'function') return null;
  // A default export the file's `config` says takes a Web Request: an Edge function's, or
  // `useWebApi`. Edge functions run in Node here.
  if (webApi || config?.useWebApi === true || config?.runtime === 'edge' || config?.runtime === 'experimental-edge') {
    return web(Object.fromEntries(HTTP_METHODS.map((m) => [m, listener])));
  }
  return async (req, res) => {
    if (typeof listener.listen !== 'function' && (helpers ?? process.env.NODEJS_HELPERS !== '0')) await addHelpers(req, res);
    return listener(req, res);
  };
}

function methodNotAllowed(_req, res) {
  res.statusCode = 405;
  res.end();
}

// A Web handler: the request as a Web `Request` (its URL as the visitor asked for it), the
// `Response` it returns written back, streamed.
async function viaWeb(fn, req, res, given) {
  const proto = req.headers['x-forwarded-proto'] === 'http' ? 'http' : 'https';
  const url = new URL(req.url ?? '/', `${proto}://${req.headers.host ?? HOST}`);
  const headers = new Headers();
  for (const [name, value] of Object.entries(req.headers)) {
    if (Array.isArray(value)) for (const v of value) headers.append(name, v);
    else if (value !== undefined) headers.set(name, value);
  }
  const body = req.method === 'GET' || req.method === 'HEAD' ? undefined : Readable.toWeb(req);
  const controller = new AbortController();
  res.on('close', () => {
    if (!res.writableFinished) controller.abort();
  });
  const request = new Request(url, { method: req.method, headers, body, duplex: body ? 'half' : undefined, signal: controller.signal });
  // Work a handler hands `waitUntil` goes on after its response; its failure is reported, as
  // Vercel's runtime does, rather than left an unhandled rejection that ends the process.
  const response = await fn(request, { waitUntil: given ?? waitUntil });
  if (!(response instanceof Response)) throw new Error('a Web handler must return a Response');
  const cookies = response.headers.getSetCookie?.() ?? [];
  for (const [name, value] of response.headers) {
    if (name !== 'set-cookie') res.setHeader(name, value);
  }
  if (cookies.length) res.setHeader('set-cookie', cookies);
  res.statusCode = response.status;
  if (response.statusText) res.statusMessage = response.statusText;
  if (!response.body || req.method === 'HEAD') {
    res.end();
    return;
  }
  await new Promise((resolve) => {
    const stream = Readable.fromWeb(response.body);
    stream.on('error', (error) => {
      console.error(error);
      res.destroy(error);
      resolve();
    });
    res.on('close', resolve);
    stream.pipe(res);
  });
}

// ---- Vercel's helpers for Node handlers (`@vercel/node`'s serverless-functions/helpers.ts) ----

class ApiError extends Error {
  constructor(statusCode, message) {
    super(message);
    this.statusCode = statusCode;
  }
}

function sendError(res, statusCode, message) {
  res.statusCode = statusCode;
  res.statusMessage = message;
  res.end();
}

// A property computed the first time it's read, and settable.
function setLazyProp(req, prop, getter) {
  const opts = { configurable: true, enumerable: true };
  const optsReset = { ...opts, writable: true };
  Object.defineProperty(req, prop, {
    ...opts,
    get: () => {
      const value = getter();
      Object.defineProperty(req, prop, { ...optsReset, value });
      return value;
    },
    set: (value) => {
      Object.defineProperty(req, prop, { ...optsReset, value });
    },
  });
}

// `content-type`'s parse, as far as the helpers use it: the type, lowercased, and parameters.
function parseContentType(header) {
  const [type, ...params] = String(header).split(';');
  const parameters = {};
  for (const param of params) {
    const eq = param.indexOf('=');
    if (eq === -1) continue;
    let value = param.slice(eq + 1).trim();
    if (value.startsWith('"') && value.endsWith('"')) value = value.slice(1, -1).replace(/\\(.)/g, '$1');
    parameters[param.slice(0, eq).trim().toLowerCase()] = value;
  }
  return { type: type.trim().toLowerCase(), parameters };
}

function formatContentType({ type, parameters }) {
  let text = type;
  for (const name of Object.keys(parameters).sort()) {
    const value = parameters[name];
    text += `; ${name}=${/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(value) ? value : JSON.stringify(value)}`;
  }
  return text;
}

function setCharset(type, charset) {
  const parsed = parseContentType(type);
  parsed.parameters.charset = charset;
  return formatContentType(parsed);
}

// A request with no content type is read as text.
function normalizeContentType(contentType) {
  if (!contentType) return 'text/plain';
  return parseContentType(contentType).type;
}

function getBodyParser(body, contentType) {
  return function parseBody() {
    const type = normalizeContentType(contentType);
    if (type === 'application/json') {
      try {
        const str = body.toString();
        return str ? JSON.parse(str) : {};
      } catch {
        throw new ApiError(400, 'Invalid JSON');
      }
    }
    if (type === 'application/octet-stream') return body;
    if (type === 'application/x-www-form-urlencoded') return querystring.parse(body.toString());
    if (type === 'text/plain') return body.toString();
    return undefined;
  };
}

// The query string's parameters; a name given more than once has a list.
function getQueryParser({ url = '/' }) {
  return () => {
    const query = {};
    new URL(url, 'http://localhost').searchParams.forEach((value, key) => {
      const existing = query[key];
      if (existing !== undefined) query[key] = Array.isArray(existing) ? [...existing, value] : [existing, value];
      else query[key] = value;
    });
    return query;
  };
}

// `cookie`'s parse: `name=value` pairs split on `;`, the first of a name kept, values
// percent-decoded when they can be, quotes taken off.
function parseCookies(header) {
  const out = {};
  for (const part of header.split(';')) {
    const eq = part.indexOf('=');
    if (eq === -1) continue;
    const name = part.slice(0, eq).trim();
    if (!name || Object.prototype.hasOwnProperty.call(out, name)) continue;
    let value = part.slice(eq + 1).trim();
    if (value.startsWith('"') && value.endsWith('"') && value.length > 1) value = value.slice(1, -1);
    try {
      out[name] = value.includes('%') ? decodeURIComponent(value) : value;
    } catch {
      out[name] = value;
    }
  }
  return out;
}

function getCookieParser(req) {
  return () => {
    const header = req.headers.cookie;
    if (!header) return {};
    return parseCookies(Array.isArray(header) ? header.join(';') : header);
  };
}

function setStatus(res, statusCode) {
  res.statusCode = statusCode;
  return res;
}

function sendRedirect(res, statusOrUrl, url) {
  if (typeof statusOrUrl === 'string') {
    url = statusOrUrl;
    statusOrUrl = 307;
  }
  if (typeof statusOrUrl !== 'number' || typeof url !== 'string') {
    throw new Error(`Invalid redirect arguments. Please use a single argument URL, e.g. res.redirect('/destination') or use a status code and URL, e.g. res.redirect(307, '/destination').`);
  }
  res.writeHead(statusOrUrl, { Location: url }).end();
  return res;
}

// `etag`'s weak ETag of a body: its length in hex and the start of its SHA-1 in base64.
function createETag(body, encoding) {
  const buf = !Buffer.isBuffer(body) ? Buffer.from(body, encoding) : body;
  if (buf.length === 0) return 'W/"0-2jmj7l5rSw0yVb/vlWAYkK/YBwk"';
  const hash = createHash('sha1').update(buf).digest('base64').substring(0, 27);
  return `W/"${buf.length.toString(16)}-${hash}"`;
}

function sendJson(req, res, jsonBody) {
  const body = JSON.stringify(jsonBody);
  if (!res.getHeader('content-type')) res.setHeader('content-type', 'application/json; charset=utf-8');
  return sendBody(req, res, body);
}

function sendBody(req, res, body) {
  let chunk = body;
  let encoding;
  switch (typeof chunk) {
    case 'string':
      if (!res.getHeader('content-type')) res.setHeader('content-type', 'text/html');
      break;
    case 'boolean':
    case 'number':
    case 'object':
      if (chunk === null) {
        chunk = '';
      } else if (Buffer.isBuffer(chunk)) {
        if (!res.getHeader('content-type')) res.setHeader('content-type', 'application/octet-stream');
      } else {
        return sendJson(req, res, chunk);
      }
      break;
  }
  if (typeof chunk === 'string') {
    encoding = 'utf8';
    const type = res.getHeader('content-type');
    if (typeof type === 'string') res.setHeader('content-type', setCharset(type, 'utf-8'));
  }
  let len;
  if (chunk !== undefined) {
    if (Buffer.isBuffer(chunk)) {
      len = chunk.length;
    } else if (typeof chunk === 'string') {
      if (chunk.length < 1000) {
        len = Buffer.byteLength(chunk, encoding);
      } else {
        const buf = Buffer.from(chunk, encoding);
        len = buf.length;
        chunk = buf;
        encoding = undefined;
      }
    } else {
      throw new Error('`body` is not a valid string, object, boolean, number, Stream, or Buffer');
    }
    if (len !== undefined) res.setHeader('content-length', len);
  }
  let etag;
  if (!res.getHeader('etag') && len !== undefined && (etag = createETag(chunk, encoding))) res.setHeader('etag', etag);
  if (204 === res.statusCode || 304 === res.statusCode) {
    res.removeHeader('Content-Type');
    res.removeHeader('Content-Length');
    res.removeHeader('Transfer-Encoding');
    chunk = '';
  }
  if (req.method === 'HEAD') res.end();
  else if (encoding) res.end(chunk, encoding);
  else res.end(chunk);
  return res;
}

// The body read whole (none for GET and HEAD), and given back to the request for code that reads
// it as a stream with `data` and `end` events or `read()`.
async function readBody(req) {
  let body = Buffer.from('');
  if (req.method !== 'GET' && req.method !== 'HEAD') {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    body = Buffer.concat(chunks);
  }
  restoreBody(req, body);
  return body;
}

function restoreBody(req, body) {
  const replicateBody = new PassThrough();
  const on = replicateBody.on.bind(replicateBody);
  const originalOn = req.on.bind(req);
  req.read = replicateBody.read.bind(replicateBody);
  req.on = req.addListener = (name, cb) => (name === 'data' || name === 'end' ? on(name, cb) : originalOn(name, cb));
  replicateBody.write(body);
  replicateBody.end();
}

async function addHelpers(req, res) {
  setLazyProp(req, 'cookies', getCookieParser(req));
  setLazyProp(req, 'query', getQueryParser(req));
  const contentType = req.headers['content-type'];
  const body = contentType === undefined ? Buffer.from('') : await readBody(req);
  setLazyProp(req, 'body', getBodyParser(body, contentType));
  res.status = (statusCode) => setStatus(res, statusCode);
  res.redirect = (statusOrUrl, url) => sendRedirect(res, statusOrUrl, url);
  res.send = (body) => sendBody(req, res, body);
  res.json = (jsonBody) => sendJson(req, res, jsonBody);
}

// A monitor only observes: the app's own handlers, or Node's default of exiting, still apply.
process.on('uncaughtExceptionMonitor', (error, origin) => {
  send({
    type: 'uncaught',
    origin,
    request_id: requests.getStore() ?? null,
    error: String(error?.stack ?? error),
  });
});

const TAG_START = '\x1e';
const TAG_END = '\x1f';
// Unfinished lines kept per request; a request that never ends its line can't hold more than this.
const MAX_PARTIAL = 1000;
// A line's start written outside any request waits this long for its end, and holds this much.
const LOOSE_WAIT_MS = 1000;
const MAX_LOOSE = 256 * 1024;
for (const stream of [process.stdout, process.stderr]) {
  const write = stream.write;
  const partial = new Map();
  // The start of a line written outside any request.
  let loose = '';
  let looseTimer = null;
  const flushLoose = () => {
    if (looseTimer) clearTimeout(looseTimer);
    looseTimer = null;
    if (!loose) return;
    const text = loose + '\n';
    loose = '';
    write.call(stream, text);
  };
  process.on('exit', flushLoose);
  stream.write = function (chunk, encoding, callback) {
    const id = requests.getStore();
    if (typeof encoding === 'function') {
      callback = encoding;
      encoding = undefined;
    }
    if (id === undefined || id === null) {
      if (typeof chunk !== 'string' && !(chunk instanceof Uint8Array)) return write.call(this, chunk, encoding, callback);
      const text = typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8');
      const lines = (loose + text).split('\n');
      loose = lines.pop();
      if (loose.length > MAX_LOOSE) {
        lines.push(loose);
        loose = '';
      }
      if (loose && !looseTimer) {
        looseTimer = setTimeout(flushLoose, LOOSE_WAIT_MS);
        looseTimer.unref();
      } else if (!loose && looseTimer) {
        clearTimeout(looseTimer);
        looseTimer = null;
      }
      if (lines.length === 0) {
        if (callback) process.nextTick(callback);
        return true;
      }
      return write.call(this, lines.map((line) => line + '\n').join(''), 'utf8', callback);
    }
    const text = typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8');
    const lines = ((partial.get(id) ?? '') + text).split('\n');
    const rest = lines.pop();
    partial.delete(id);
    if (rest) {
      if (partial.size >= MAX_PARTIAL) partial.clear();
      partial.set(id, rest);
    }
    if (lines.length === 0) {
      if (callback) process.nextTick(callback);
      return true;
    }
    return write.call(this, lines.map((line) => `${TAG_START}${id}${TAG_END}${line}\n`).join(''), 'utf8', callback);
  };
}

const MAX_URL = 1500;
const MAX_ERROR = 300;
// Calls in progress, by the request object the channels pass around.
const calls = new WeakMap();

function begin(key, method, url) {
  calls.set(key, {
    request: requests.getStore() ?? null,
    method: String(method || 'GET').toUpperCase().slice(0, 16),
    url: String(url).slice(0, MAX_URL),
    start: Date.now() / 1000,
    t0: performance.now(),
  });
}

function end(key, status, error) {
  const call = calls.get(key);
  if (call === undefined) return;
  calls.delete(key);
  const message = {
    type: 'fetch',
    request: call.request,
    method: call.method,
    url: call.url,
    start: call.start,
    ms: Math.round((performance.now() - call.t0) * 10) / 10,
  };
  if (status !== undefined && status !== null) message.status = status;
  if (error) message.error = `${error.name ?? 'Error'}: ${error.message ?? error}`.slice(0, MAX_ERROR);
  send(message);
}

// A subscriber that throws would crash the app (Node rethrows it on the next tick), so each one
// swallows its own errors: timing must never change what the app's call does.
function listen(channel, handler) {
  diagnostics.subscribe(channel, (message) => {
    try {
      handler(message);
    } catch {
      // Leave the call alone.
    }
  });
}

// The loop guard: the mark of the request being served, for a call to one of the install's own
// hosts (`*.<domain>` entries cover every name under it). The file is looked at again at most once
// a second, and read only when it changed; without it no call is marked.
const LOOP_HEADER = 'x-forkrail-loop';
const OWN_HOSTS_FILE = process.env.FORKRAIL_OWN_HOSTS_FILE;
const own = { checked: 0, mtime: null, names: new Set(), domains: [] };

function ownHosts() {
  const now = Date.now();
  if (now - own.checked < 1000) return own;
  own.checked = now;
  let mtime;
  try {
    mtime = statSync(OWN_HOSTS_FILE).mtimeMs;
  } catch {
    return own;
  }
  if (mtime === own.mtime) return own;
  const names = new Set();
  const domains = [];
  for (const line of readFileSync(OWN_HOSTS_FILE, 'utf8').split('\n')) {
    const entry = line.trim().toLowerCase();
    if (entry.startsWith('*.')) domains.push(entry.slice(1));
    else if (entry) names.add(entry);
  }
  Object.assign(own, { mtime, names, domains });
  return own;
}

function loopMark(host) {
  if (!OWN_HOSTS_FILE || !host) return null;
  const mark = vercelContext.getStore()?.headers?.[LOOP_HEADER];
  if (typeof mark !== 'string' || mark === '') return null;
  const name = String(host).toLowerCase().replace(/\.$/, '');
  const { names, domains } = ownHosts();
  return names.has(name) || domains.some((domain) => name.length > domain.length && name.endsWith(domain)) ? mark : null;
}

// fetch() and undici: from the request to the end of the response body.
listen('undici:request:create', ({ request }) => {
  const mark = loopMark(new URL(request.origin).hostname);
  if (mark) request.addHeader(LOOP_HEADER, mark);
});
listen('undici:request:create', ({ request }) => begin(request, request.method, `${request.origin}${request.path}`));
listen('undici:request:headers', ({ request, response }) => {
  const call = calls.get(request);
  if (call) call.status = response.statusCode;
});
listen('undici:request:trailers', ({ request }) => end(request, calls.get(request)?.status));
listen('undici:request:error', ({ request, error }) => end(request, calls.get(request)?.status, error));
// node:http and node:https: marked when the request is made (Node 22.12 and later), before its
// headers can have gone; timed from the request to the response's headers.
listen('http.client.request.created', ({ request }) => {
  const mark = loopMark(request.host);
  if (mark) request.setHeader(LOOP_HEADER, mark);
});
listen('http.client.request.start', ({ request }) => {
  // The Host header has the port when it isn't the default; `request.host` never does.
  const host = request.getHeader?.('host') ?? request.host;
  begin(request, request.method, `${request.protocol}//${host}${request.path}`);
});
listen('http.client.response.finish', ({ request, response }) => end(request, response.statusCode));
listen('http.client.request.error', ({ request, error }) => end(request, null, error));

setInterval(() => {
  if (process.ppid !== runtime) process.exit(1);
}, 1000).unref();
