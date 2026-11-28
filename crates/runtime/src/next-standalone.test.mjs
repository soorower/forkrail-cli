// Tests for next-standalone.cjs against a stand-in for Next's server with the two methods it
// wraps, shaped as Next's are (13.5 to 16.4): run with `node --test crates/runtime/src/next-standalone.test.mjs`.
// The real thing, on real builds of four Next versions behind a router, is
// tools/next-n4/origin-check.sh (local only).

import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const MODULE = fileURLToPath(new URL('./next-standalone.cjs', import.meta.url));

// An app folder with a `next` that has what the module asks for. `metaKey` is the name Next
// keeps the request's URL under (13.5: `__NEXT_INIT_URL`).
function app({ metaKey = 'initURL', methods = true } = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'forkrail-next-test-'));
  const server = join(dir, 'node_modules/next/dist/server');
  mkdirSync(server, { recursive: true });
  writeFileSync(join(dir, 'server.js'), '');
  writeFileSync(
    join(server, 'request-meta.js'),
    `const META = Symbol.for('NextInternalRequestMeta');
     exports.getRequestMeta = (req, key) => (req[META] || {})[key];
     exports.addRequestMeta = (req, key, value) => { (req[META] ||= {})[key] = value; };`,
  );
  writeFileSync(
    join(server, 'next-server.js'),
    `const meta = require('./request-meta');
     class NextNodeServer {
       constructor() { this.fetchHostname = 'localhost'; this.port = 41234; this.middleware = null; }
       ${methods ? `
       attachRequestMeta(req) { meta.addRequestMeta(req, ${JSON.stringify(metaKey)}, 'http://localhost:41234' + req.url); }
       async runMiddleware(params) {
         // As Next: the URL is read before the first await.
         const url = 'http://' + this.fetchHostname + ':' + this.port + params.parsed.pathname;
         await null;
         return this.middleware(url, params);
       }` : ''}
     }
     exports.default = NextNodeServer;`,
  );
  // A fresh copy of the module each time: it keeps what `prime` and `install` were told.
  const load = createRequire(join(dir, 'server.js'));
  delete load.cache[MODULE];
  const forkrail = load(MODULE);
  return { dir, forkrail, load, serverJs: join(dir, 'server.js'), done: () => rmSync(dir, { recursive: true, force: true }) };
}

const router = (host, extra = {}) => ({ host, 'x-forwarded-host': host, 'x-forwarded-proto': 'https', 'x-forwarded-port': '443', 'x-forwarded-for': '203.0.113.9', ...extra });
const META = Symbol.for('NextInternalRequestMeta');

test("request.url is the visitor's: production host, custom domain, preview host, a port", () => {
  for (const metaKey of ['initURL', '__NEXT_INIT_URL']) {
    const a = app({ metaKey });
    try {
      assert.equal(a.forkrail.install(a.serverJs), true);
      const server = new (a.load('next/dist/server/next-server').default)();
      const urlOf = (headers, url = '/echo?a=1') => {
        const req = { url, headers };
        server.attachRequestMeta(req, {});
        return req[META][metaKey];
      };
      assert.equal(urlOf(router('app.example.com')), 'https://app.example.com/echo?a=1');
      assert.equal(urlOf(router('www.custom.io')), 'https://www.custom.io/echo?a=1');
      assert.equal(urlOf(router('app-ab12cd34.example.com')), 'https://app-ab12cd34.example.com/echo?a=1');
      assert.equal(urlOf(router('app.example.com:8443')), 'https://app.example.com:8443/echo?a=1');
      assert.equal(urlOf({ ...router('127.0.0.1:18080'), 'x-forwarded-proto': 'http' }), 'http://127.0.0.1:18080/echo?a=1');
      // The router's header wins over Host (it replaces a client's own).
      assert.equal(urlOf({ ...router('app.example.com'), host: 'internal:3000' }), 'https://app.example.com/echo?a=1');
      // Not the router's request (Next asking itself in the process has no Host), or a host that
      // isn't one: Next's own URL stays.
      assert.equal(urlOf({}), 'http://localhost:41234/echo?a=1');
      assert.equal(urlOf(router('evil.example.net/@app.example.com')), 'http://localhost:41234/echo?a=1');
      assert.equal(urlOf(router('app.example.com'), 'http://absolute/'), 'http://localhost:41234http://absolute/');
    } finally {
      a.done();
    }
  }
});

test("middleware sees the visitor's origin, and its rewrites to that origin stay in the process", async () => {
  const a = app();
  try {
    a.forkrail.install(a.serverJs);
    const server = new (a.load('next/dist/server/next-server').default)();
    const params = (host, pathname = '/same') => ({ request: { headers: router(host) }, parsed: { pathname } });
    let seen;
    const answer = (headers) => ({ response: new Response(null, { headers }) });
    // It sees the visitor's URL; the server's own fields are back afterwards.
    server.middleware = (url) => ((seen = url), answer({ 'x-middleware-next': '1' }));
    const passed = await server.runMiddleware(params('www.custom.io'));
    assert.equal(seen, 'http://www.custom.io:443/same');
    assert.deepEqual([server.fetchHostname, server.port], ['localhost', 41234]);
    assert.equal(passed.response.headers.get('x-middleware-next'), '1');
    // A rewrite to the visitor's own origin goes on as a path, which Next serves itself.
    server.middleware = () => answer({ 'x-middleware-rewrite': 'https://www.custom.io/echo?b=2' });
    assert.equal((await server.runMiddleware(params('www.custom.io'))).response.headers.get('x-middleware-rewrite'), '/echo?b=2');
    // One to another site stays what it is.
    server.middleware = () => answer({ 'x-middleware-rewrite': 'https://other.example.org/x' });
    assert.equal((await server.runMiddleware(params('www.custom.io'))).response.headers.get('x-middleware-rewrite'), 'https://other.example.org/x');
    // A request that isn't the router's runs as Next has it.
    server.middleware = (url) => ((seen = url), answer({}));
    await server.runMiddleware({ request: { headers: {} }, parsed: { pathname: '/x' } });
    assert.equal(seen, 'http://localhost:41234/x');
  } finally {
    a.done();
  }
});

test("the priming request never reaches the app's middleware; nobody else can send one", async () => {
  const a = app();
  try {
    a.forkrail.prime();
    a.forkrail.install(a.serverJs);
    const prime = globalThis[Symbol.for('forkrail.prime')];
    assert.equal(typeof prime, 'function');
    const server = new (a.load('next/dist/server/next-server').default)();
    let ran = 0;
    server.middleware = () => (ran++, { response: new Response(null) });
    // What the priming request carries is seen by a server standing in for the app's.
    const { createServer } = await import('node:http');
    const asked = [];
    const http = createServer((req, res) => {
      asked.push({ url: req.url, headers: req.headers });
      res.statusCode = 404;
      res.end('Not Found');
    });
    await new Promise((resolve) => http.listen(0, '127.0.0.1', resolve));
    try {
      const did = await prime({ host: '127.0.0.1', port: http.address().port, deadline: Date.now() + 2000 });
      assert.equal(did.request, '/_next/static/forkrail-prime 404');
      assert.equal(asked.length, 1);
      const secret = asked[0].headers[a.forkrail.PRIME_HEADER];
      assert.match(secret, /^[0-9a-f]{32}$/);
      // With that header Next's middleware step answers "go on" without the app's code.
      const skipped = await server.runMiddleware({ request: { headers: asked[0].headers }, parsed: { pathname: asked[0].url } });
      assert.equal(skipped.response.headers.get('x-middleware-next'), '1');
      assert.equal(ran, 0);
      // A visitor guessing at it runs the middleware like anyone.
      await server.runMiddleware({ request: { headers: router('app.example.com', { [a.forkrail.PRIME_HEADER]: '0'.repeat(32) }) }, parsed: { pathname: '/' } });
      assert.equal(ran, 1);
    } finally {
      http.close();
    }
  } finally {
    delete globalThis[Symbol.for('forkrail.prime')];
    a.done();
  }
});

test('priming gives up at its deadline, and the config is changed only where it must be', async () => {
  const a = app();
  const before = process.env.__NEXT_PRIVATE_STANDALONE_CONFIG;
  try {
    // A build made as on Vercel trusts the Host header and preloads every page at start.
    process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify({ basePath: '/docs', experimental: { trustHostHeader: true, preloadEntriesOnStart: true, other: 1 } });
    a.forkrail.prime();
    a.forkrail.install(a.serverJs);
    assert.deepEqual(JSON.parse(process.env.__NEXT_PRIVATE_STANDALONE_CONFIG), { basePath: '/docs', experimental: { trustHostHeader: false, preloadEntriesOnStart: false, other: 1 } });
    // A server that never answers: the priming request ends at the deadline, with no status.
    const { createServer } = await import('node:net');
    const silent = createServer(() => {});
    await new Promise((resolve) => silent.listen(0, '127.0.0.1', resolve));
    try {
      const began = Date.now();
      const did = await globalThis[Symbol.for('forkrail.prime')]({ host: '127.0.0.1', port: silent.address().port, deadline: Date.now() + 150 });
      assert.ok(Date.now() - began < 1000, `${Date.now() - began} ms`);
      assert.equal(did.request, '/_next/static/forkrail-prime null');
      // Its pages' modules were its to load (Next's own pass was on): none listed here.
      assert.equal(did.modules, '0/0');
    } finally {
      silent.close();
    }
  } finally {
    if (before === undefined) delete process.env.__NEXT_PRIVATE_STANDALONE_CONFIG;
    else process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = before;
    delete globalThis[Symbol.for('forkrail.prime')];
    a.done();
  }
  // Without priming asked for, Next's own pass is left alone.
  const b = app();
  try {
    process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify({ experimental: { preloadEntriesOnStart: true } });
    b.forkrail.install(b.serverJs);
    assert.equal(JSON.parse(process.env.__NEXT_PRIVATE_STANDALONE_CONFIG).experimental.preloadEntriesOnStart, true);
  } finally {
    if (before === undefined) delete process.env.__NEXT_PRIVATE_STANDALONE_CONFIG;
    else process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = before;
    b.done();
  }
});

test('a Next without the two methods is left as it is, with a warning', () => {
  const a = app({ methods: false });
  const { warn } = console;
  const warned = [];
  console.warn = (...args) => warned.push(args.join(' '));
  try {
    assert.equal(a.forkrail.install(a.serverJs), false);
    assert.equal(warned.length, 1);
  } finally {
    console.warn = warn;
    a.done();
  }
});

test('the pages a project names for priming', () => {
  const a = app();
  try {
    assert.deepEqual(a.forkrail.primePaths('/, /pricing /docs/a?b=1'), ['/', '/pricing', '/docs/a?b=1']);
    assert.deepEqual(a.forkrail.primePaths('pricing //evil.example.net https://x/y'), []);
    assert.deepEqual(a.forkrail.primePaths(''), []);
    assert.equal(a.forkrail.primePaths(Array.from({ length: 20 }, (_, i) => `/p${i}`).join(',')).length, 8);
  } finally {
    a.done();
  }
});

// Next's file serving and filesystem check as `router-server.js` requires them (13.5 to 16.4: the
// exports are getters, `serveStatic(req, res, itemPath, { root })` after `getItem`).
function nextFiles(a, own) {
  const lib = join(a.dir, 'node_modules/next/dist/server/lib');
  mkdirSync(join(lib, 'router-utils'), { recursive: true });
  const getters = (names) =>
    `Object.defineProperty(exports, "__esModule", { value: true });
     ${names.map((n) => `Object.defineProperty(exports, ${JSON.stringify(n)}, { enumerable: true, get: function() { return ${n}; } });`).join('\n')}`;
  writeFileSync(
    join(a.dir, 'node_modules/next/dist/server/serve-static.js'),
    `${getters(['serveStatic', 'getContentType'])}
     function serveStatic(req, res, path, opts) { globalThis.__served.push({ path, root: opts.root }); res.end('from disk'); return Promise.resolve(); }
     function getContentType() { return 'text/plain'; }`,
  );
  writeFileSync(
    join(lib, 'router-utils/filesystem.js'),
    `${getters(['setupFsCheck'])}
     const own = new Set(${JSON.stringify(own)});
     async function setupFsCheck(opts) {
       return { async getItem(itemPath) { return own.has(itemPath) ? { type: 'publicFolder', fsPath: '/app/public' + itemPath, itemsRoot: '/app/public', itemPath } : null; } };
     }`,
  );
  writeFileSync(
    join(lib, 'router-server.js'),
    `const _servestatic = require("../serve-static");
     const _filesystem = require("./router-utils/filesystem");
     exports.serve = async (pathname, config) => {
       const fsChecker = await (0, _filesystem.setupFsCheck)({ dir: '/app', dev: false, minimalMode: false, config });
       const matched = await fsChecker.getItem(pathname);
       const res = { headers: {}, body: null, setHeader(k, v) { this.headers[k.toLowerCase()] = v; }, end(b) { this.body = b ?? ''; } };
       if (!(matched && matched.fsPath && matched.itemPath)) return { matched, res: null };
       await (0, _servestatic.serveStatic)({}, res, matched.itemPath, { root: matched.itemsRoot, etag: true });
       return { matched, res };
     };`,
  );
}

test("a rewrite onto a file the router serves: Next's server finds it, and tells the router to answer with it", async () => {
  const a = app();
  const Module = createRequire(import.meta.url)('node:module');
  const original = Module.prototype.require;
  globalThis.__served = [];
  try {
    nextFiles(a, ['/docs/own.txt']);
    writeFileSync(join(a.dir, a.forkrail.ROUTER_FILES), '/docs/another.txt\n/docs/_next/static/b1/_buildManifest.js\n/docs/café.txt\n');
    a.forkrail.prime();
    a.forkrail.routerFiles();
    a.forkrail.install(a.serverJs);
    const { serve } = a.load('next/dist/server/lib/router-server');
    // The hook is let go of once Next's two modules have been through it.
    assert.equal(Module.prototype.require, original);
    const config = { basePath: '/docs' };
    let { matched, res } = await serve('/docs/another.txt', config);
    assert.equal(matched.type, 'publicFolder');
    assert.deepEqual([res.headers['x-forkrail-static'], res.body], ['/docs/another.txt', '']);
    ({ matched, res } = await serve('/docs/_next/static/b1/_buildManifest.js/', config));
    assert.deepEqual([matched.type, matched.itemPath], ['nextStaticFolder', '/b1/_buildManifest.js']);
    assert.equal(res.headers['x-forkrail-static'], '/docs/_next/static/b1/_buildManifest.js');
    ({ res } = await serve('/docs/caf%C3%A9.txt', config));
    assert.equal(res.headers['x-forkrail-static'], '/docs/caf%C3%A9.txt');
    // Under the default locale, as Next finds its own files; not under another, nor outside the base path.
    const i18n = { basePath: '/docs', i18n: { locales: ['en', 'sv'], defaultLocale: 'en' } };
    ({ matched, res } = await serve('/docs/en/another.txt', i18n));
    assert.deepEqual([matched.locale, res.headers['x-forkrail-static']], ['en', '/docs/another.txt']);
    assert.equal((await serve('/docs/sv/another.txt', i18n)).matched, null);
    assert.equal((await serve('/another.txt', config)).matched, null);
    assert.equal((await serve('/docs/nope.txt', config)).matched, null);
    assert.deepEqual(globalThis.__served, []);
    // Next's own files are served by Next, as before.
    ({ res } = await serve('/docs/own.txt', config));
    assert.deepEqual([res.body, res.headers['x-forkrail-static']], ['from disk', undefined]);
    assert.deepEqual(globalThis.__served, [{ path: '/docs/own.txt', root: '/app/public' }]);
  } finally {
    Module.prototype.require = original;
    delete globalThis.__served;
    delete globalThis[Symbol.for('forkrail.prime')];
    a.done();
  }
});

test('without the list (an older build), or with nothing listed, nothing is found that Next has not', () => {
  const a = app();
  try {
    assert.equal(a.forkrail.routerItem('/another.txt', {}, new Set()), null);
    a.forkrail.install(a.serverJs);
    assert.equal(a.forkrail.routerItem('/another.txt', {}), null);
    const files = new Set(['/another.txt', '/100%.txt']);
    assert.equal(a.forkrail.routerItem('/another.txt', {}, files).itemPath, '/another.txt');
    assert.equal(a.forkrail.routerItem('/100%25.txt', {}, files).itemPath, '/100%.txt');
    assert.equal(a.forkrail.routerItem('/100%.txt', {}, files).itemPath, '/100%.txt');
    assert.equal(a.forkrail.routerItem(undefined, {}, files), null);
  } finally {
    a.done();
  }
});
