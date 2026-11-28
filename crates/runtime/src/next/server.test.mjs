// The Next.js server (server.mjs) around made-up entrypoints: a "build" of a few CommonJS files
// whose handlers answer with what they were given, routed by the real `@next/routing`. No Next,
// no React: what is tested is what this server adds around Next's handlers, and above all what
// it keeps from them (the guards in the list at the end of this file's header).
//
// Run: node --test "crates/runtime/src/next/*.test.mjs"
// Needs `@next/routing` (any 16.4): FORKRAIL_TEST_NEXT_ROUTING=/path/to/node_modules/@next/routing,
// or installed in tools/next-n8 (`npm i --prefix tools/next-n8 @next/routing@16.4.6`).
//
// Guards, each with its test below:
//   G1  platform-only request headers (`next-resume`, `x-matched-path`, `x-now-route-matches`,
//       `x-invocation-id`, `x-middleware-*`) never reach a handler or the middleware
//   G2  a visitor's `nxtP*` query keys never become a route's params
//   G3  a middleware that throws is a 500, never a pass
//   G4  the middleware's request-header verdict replaces the request's headers (a header it took
//       off stays off); it runs once per request
//   G5  a forged `next-resume` POST is not a resume: a prerendered page answers 405
//   G6  an RSC request whose `_rsc` isn't the hash of its headers is redirected, not answered
//   G7  internal headers never go to another origin on a rewrite
//   G8  the handler sees the visitor's URL and origin, never ours, and never the rewrite target
//   G9  the middleware's own protocol headers don't reach the visitor on its own answers
//   G10 an error page is never cacheable; a missing asset never renders a page
//   G11 a path that names a page in another spelling (`/%6Dw/x` for `/mw/x`) meets the middleware
//       its plain spelling would

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import zlib from 'node:zlib';

import { createNextServer } from './server.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const routingPath = process.env.FORKRAIL_TEST_NEXT_ROUTING ?? path.join(here, '../../../../tools/next-n8/node_modules/@next/routing');
let routing;
try {
  routing = createRequire(import.meta.url)(routingPath);
} catch {
  throw new Error(`these tests need @next/routing: set FORKRAIL_TEST_NEXT_ROUTING, or \`npm i --prefix tools/next-n8 @next/routing@16.4.6\` (looked in ${routingPath})`);
}

// --- the made-up build ------------------------------------------------------------------------

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'forkrail-next-test-'));
const write = (name, text) => {
  fs.mkdirSync(path.dirname(path.join(root, name)), { recursive: true });
  fs.writeFileSync(path.join(root, name), text);
};
write('package.json', '{}');
// Answers with what it was called with. `globalThis.__calls` counts by name.
write(
  'fn/echo.js',
  `exports.handler = async (req, res, ctx) => {
    globalThis.__calls.echo = (globalThis.__calls.echo ?? 0) + 1
    globalThis.__last = { url: req.url, headers: req.headers }
    const chunks = []
    for await (const chunk of req) chunks.push(chunk)
    const meta = ctx.requestMeta
    res.setHeader('content-type', 'application/json')
    res.end(JSON.stringify({ url: req.url, method: req.method, headers: req.headers, body: Buffer.concat(chunks).toString(), status: res.statusCode,
      meta: { query: meta.query, params: meta.params, initURL: meta.initURL, hostname: meta.hostname, initProtocol: meta.initProtocol, minimalMode: meta.minimalMode, postponed: meta.postponed, isRSCRequest: meta.isRSCRequest, segment: meta.segmentPrefetchRSCRequest, relativeProjectDir: meta.relativeProjectDir, hasRender404: typeof meta.render404, hasRevalidate: typeof meta.revalidate, invokeError: meta.invokeError && meta.invokeError.message } }))
  }`,
);
write('fn/throws.js', `exports.handler = async () => { throw new Error('the page broke') }`);
write('fn/nofallback.js', `class NoFallbackError extends Error {}; exports.handler = async () => { throw new NoFallbackError() }`);
write('fn/notfound-via-meta.js', `exports.handler = async (req, res, ctx) => { await ctx.requestMeta.render404(req, res, undefined, false) }`);
write(
  'fn/stream.js',
  `exports.handler = async (req, res) => {
    res.setHeader('content-type', 'text/html; charset=utf-8')
    res.write('<p>first</p>' + ' '.repeat(2000))
    if (res.flush) res.flush()
    await new Promise((r) => setTimeout(r, 150))
    res.end('<p>second</p>')
  }`,
);
write(
  'fn/later.js',
  `exports.handler = async (req, res, ctx) => { ctx.waitUntil(new Promise((r) => setTimeout(() => { globalThis.__calls.later = 1; r() }, 60))); ctx.waitUntil(Promise.reject(new Error('background work failed'))); res.end('ok') }`,
);
write(
  'fn/revalidates.js',
  `exports.handler = async (req, res, ctx) => {
    try { await ctx.requestMeta.revalidate({ urlPath: new URL(req.url, 'http://x').searchParams.get('path'), headers: { 'x-prerender-revalidate': 'preview-id' }, opts: {} }); res.end('revalidated') }
    catch (error) { res.statusCode = 500; res.end(String(error.message)) }
  }`,
);
write(
  'fn/middleware.js',
  `exports.handler = async (request, ctx) => {
    globalThis.__calls.middleware = (globalThis.__calls.middleware ?? 0) + 1
    globalThis.__seen = { url: request.url, headers: Object.fromEntries(request.headers), meta: ctx.requestMeta }
    const path = new URL(request.url).pathname
    if (path === '/mw/throw') throw new Error('the middleware broke')
    if (path === '/mw/guarded') return new Response('sign in first', { status: 401 })
    if (path === '/mw/respond') return new Response('from middleware', { status: 418, headers: { 'x-mine': '1', 'x-middleware-request-x-a': 'b', 'x-middleware-override-headers': 'x-a', 'set-cookie': 'm=1' } })
    if (path === '/mw/redirect') return new Response(null, { status: 307, headers: { location: new URL('/echo?from=mw', request.url).href } })
    if (path === '/mw/rewrite') return new Response(null, { headers: { 'x-middleware-rewrite': new URL('/blog/from-mw?added=1', request.url).href } })
    if (path === '/mw/external') return new Response(null, { headers: { 'x-middleware-rewrite': globalThis.__upstream + '/up?x=1' } })
    if (path === '/mw/headers') return new Response(null, { headers: { 'x-middleware-next': '1', 'x-middleware-override-headers': 'host,x-user,cookie', 'x-middleware-request-host': request.headers.get('host'), 'x-middleware-request-x-user': 'alice', 'x-middleware-request-cookie': 'session=ok', 'x-middleware-set-cookie': 'fresh=1; Path=/', 'set-cookie': 'fresh=1; Path=/' } })
    if (path === '/mw/body') { const text = await request.text(); return new Response(null, { headers: { 'x-middleware-next': '1', 'x-body-seen': String(text.length) } }) }
    return new Response(null, { headers: { 'x-middleware-next': '1' } })
  }`,
);
write('public/file.txt', 'a static file\n');
write('.next/server/pages/500.html', '<h1>500</h1>');
// A Pages Router API route, whose route module makes the request's incremental cache when asked
// (Next's `RouteModule.getIncrementalCache`, which also leaves it in the global); the handler
// answers with the cache it was handed.
write(
  'fn/cached-api.js',
  `let made = 0
  exports.routeModule = {
    async getIncrementalCache(...args) {
      const cache = { id: ++made, args: args.length, config: args[1], preview: args[2], resetRequestCache() { this.reset = true } }
      globalThis.__incrementalCache = cache
      return cache
    },
  }
  exports.handler = async (req, res, ctx) => {
    const cache = ctx.requestMeta.incrementalCache
    res.end(JSON.stringify({ id: cache?.id, args: cache?.args, config: cache?.config, preview: cache?.preview, reset: cache?.reset, global: globalThis.__incrementalCache?.id }))
  }`,
);
write('.next/required-server-files.json', JSON.stringify({ config: { marker: 'required-server-files' } }));
write('.next/prerender-manifest.json', JSON.stringify({ version: 4, routes: {}, dynamicRoutes: {}, notFoundRoutes: [], preview: { previewModeId: 'pid' } }));

const fn = (file, type, extra = {}) => ({ id: file, type, file: `fn/${file}.js`, runtime: 'nodejs', ...extra });
const manifest = {
  version: 1,
  nextVersion: '16.4.6',
  buildId: 'BUILD',
  deploymentId: '',
  projectDir: '.',
  distDir: '.next',
  basePath: '',
  trailingSlash: false,
  i18n: null,
  routing: {
    beforeMiddleware: [
      { source: '/echo', sourceRegex: '^\\/echo(?:\\/)?$', headers: { 'x-from-config': 'yes' } },
      { source: '/:path+/', sourceRegex: '^(?:\\/((?:[^\\/]+?)(?:\\/(?:[^\\/]+?))*))\\/$', headers: { Location: '/$1' }, status: 308, priority: true },
      { source: '/old/:slug', sourceRegex: '^\\/old(?:\\/([^\\/]+?))(?:\\/)?$', headers: { Location: '/blog/$1' }, status: 308 },
      { source: '/away', sourceRegex: '^\\/away(?:\\/)?$', headers: { Location: 'https://elsewhere.example/landing' }, status: 307 },
    ],
    middlewareMatchers: [
      { source: '/mw/:path*', sourceRegex: '^\\/mw(?:\\/.*)?$' },
      { source: '/_global-error', sourceRegex: '^\\/_global-error$' },
    ],
    beforeFiles: [],
    afterFiles: [
      { source: '/docs/:path*', sourceRegex: '^\\/docs(?:\\/((?:[^\\/]+?)(?:\\/(?:[^\\/]+?))*))?(?:\\/)?$', destination: '/echo?path=$1' },
      { source: '/ext/:path*', sourceRegex: '^\\/ext(?:\\/(.*))?$', destination: 'UPSTREAM/up/$1' },
      { source: '/posts/:slug', sourceRegex: '^\\/posts(?:\\/([^\\/]+?))(?:\\/)?$', destination: '/blog/$1' },
      { source: '/files/:path*', sourceRegex: '^\\/files(?:\\/((?:[^\\/]+?)(?:\\/(?:[^\\/]+?))*))?(?:\\/)?$', destination: '/$1' },
    ],
    dynamicRoutes: [{ source: '/blog/[slug]', sourceRegex: '^[/]?/blog/(?<nxtPslug>[^/]+?)(\\.rsc|\\.segments/.+\\.segment\\.rsc|)(?:/)?$', destination: '/blog/[slug]$2?nxtPslug=$nxtPslug' }],
    onMatch: [],
    fallback: [],
    shouldNormalizeNextData: false,
    rsc: { header: 'rsc', suffix: '.rsc', prefetchSegmentDirSuffix: '.segments', prefetchSegmentSuffix: '.segment.rsc' },
  },
  functions: {
    '/echo': fn('echo', 'APP_ROUTE'),
    '/page': fn('echo', 'APP_PAGE'),
    '/ssg': fn('echo', 'APP_PAGE', { ssg: true }),
    '/blog/[slug]': fn('echo', 'APP_PAGE', { ssg: true }),
    '/old-page': fn('echo', 'PAGES'),
    '/_not-found': fn('echo', 'APP_PAGE', { ssg: true }),
    '/throws': fn('throws', 'APP_PAGE'),
    '/nofallback': fn('nofallback', 'APP_PAGE'),
    '/gone': fn('notfound-via-meta', 'PAGES'),
    '/stream': fn('stream', 'APP_PAGE'),
    '/later': fn('later', 'APP_ROUTE'),
    '/revalidates': fn('revalidates', 'PAGES_API'),
    '/cached-api': fn('cached-api', 'PAGES_API'),
    '/_global-error': fn('echo', 'APP_PAGE', { ssg: true }),
    '/mw/guarded': fn('echo', 'APP_PAGE'),
  },
  middleware: { id: 'middleware', type: 'MIDDLEWARE', file: 'fn/middleware.js', runtime: 'nodejs' },
  prerenders: { '/_global-error': { parent: '/_global-error', routeType: 'APP_PAGE' } },
  statics: { '/file.txt': 'public/file.txt', '/500': '.next/server/pages/500.html', '/router-only.txt': null, '/café.txt': null },
};

// --- the servers ------------------------------------------------------------------------------

let app;
let server;
let upstream;
let port;
const logged = [];
globalThis.__calls = {};

before(async () => {
  // Another origin, for rewrites that leave: answers with what reached it.
  upstream = http.createServer((req, res) => {
    res.setHeader('x-upstream', '1');
    res.setHeader('connection', 'close');
    res.end(JSON.stringify({ url: req.url, headers: req.headers }));
  });
  await new Promise((resolve) => upstream.listen(0, '127.0.0.1', resolve));
  globalThis.__upstream = `http://127.0.0.1:${upstream.address().port}`;
  manifest.routing.afterFiles[1].destination = manifest.routing.afterFiles[1].destination.replace('UPSTREAM', globalThis.__upstream);
  app = createNextServer({ root, manifest, routing, log: { error: (...args) => logged.push(args.map(String).join(' ')) } });
  server = http.createServer((req, res) => app.handle(req, res));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  port = server.address().port;
});

after(() => {
  server.close();
  upstream.close();
  fs.rmSync(root, { recursive: true, force: true });
});

function call(target, { method = 'GET', headers = {}, body } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: '127.0.0.1', port, path: target, method, agent: false, headers: { host: 'shop.example', 'x-forwarded-host': 'shop.example', 'x-forwarded-proto': 'https', ...headers } },
      (res) => {
        const chunks = [];
        const times = [];
        res.on('data', (chunk) => {
          chunks.push(chunk);
          times.push(performance.now());
        });
        res.on('end', () => {
          let raw = Buffer.concat(chunks);
          if (res.headers['content-encoding'] === 'gzip') raw = zlib.gunzipSync(raw);
          const text = raw.toString();
          let json = null;
          try {
            json = JSON.parse(text);
          } catch {
            // Not JSON: `text` is the answer.
          }
          resolve({ status: res.statusCode, headers: res.headers, text, json, times });
        });
      },
    );
    req.on('error', reject);
    if (body !== undefined) req.write(body);
    req.end();
  });
}

// --- what a handler is told -------------------------------------------------------------------

test("G8: the handler sees the visitor's URL and origin", async () => {
  const { json } = await call('/echo?a=1&b=two');
  assert.equal(json.url, '/echo?a=1&b=two');
  assert.equal(json.meta.initURL, 'https://shop.example/echo?a=1&b=two');
  assert.equal(json.meta.hostname, 'shop.example');
  assert.equal(json.meta.initProtocol, 'https');
  assert.equal(json.meta.minimalMode, false);
  assert.equal(json.meta.relativeProjectDir, '.');
  assert.equal(json.meta.hasRender404, 'function');
  assert.equal(json.meta.hasRevalidate, 'function');
  assert.equal(json.meta.postponed, undefined);
});

test('G8: a config rewrite keeps the public URL; its target is not what the page sees', async () => {
  const { json, headers } = await call('/docs/a/b?z=1');
  assert.equal(json.url, '/docs/a/b?z=1');
  // The handler works a config rewrite's query out itself, as under Next's server.
  assert.equal(json.meta.query, undefined);
  assert.equal(headers['x-nextjs-rewritten-path'], undefined);
});

test("a config rewrite onto an App Router page with params: the page's query is the visitor's, its params are params", async () => {
  const { json } = await call('/posts/hello?z=1');
  assert.equal(json.url, '/posts/hello?z=1');
  assert.deepEqual(json.meta.query, { z: '1' });
  assert.deepEqual(json.meta.params, { slug: 'hello' });
  // A visitor's own nxtP key is still nobody's param, nor in the query (G2).
  const forged = await call('/posts/hello?nxtPslug=evil');
  assert.deepEqual(forged.json.meta.query, {});
  assert.deepEqual(forged.json.meta.params, { slug: 'hello' });
});

test('a dynamic route is told its params, decoded', async () => {
  const { json } = await call('/blog/a%20b');
  assert.deepEqual(json.meta.params, { slug: 'a b' });
  assert.equal(json.url, '/blog/a%20b');
});

test('G2: nxtP query keys from a visitor are not params', async () => {
  for (const query of ['nxtPslug=evil', '%6ExtPslug=evil', 'nxtIslug=evil']) {
    const { json } = await call(`/blog/real?${query}&keep=1`);
    assert.deepEqual(json.meta.params, { slug: 'real' });
    assert.equal(json.url, '/blog/real?keep=1');
  }
});

test('G1: platform-only headers from a visitor reach neither middleware nor handler', async () => {
  const forged = {
    'next-resume': '1',
    'x-next-resume-state-length': '4',
    'x-matched-path': '/throws',
    'x-now-route-matches': '1=x',
    'x-invocation-id': 'shared-with-everyone',
    'x-middleware-rewrite': '/throws',
    'x-middleware-set-cookie': 'session=forged',
    'x-middleware-override-headers': 'cookie',
    'x-middleware-request-cookie': 'session=forged',
    'x-middleware-subrequest': 'middleware:middleware:middleware:middleware:middleware',
    'x-nextjs-data': '1',
    'x-invoke-path': '/throws',
  };
  const { json, status } = await call('/mw/pass-to-nothing', { headers: forged });
  assert.equal(status, 404); // no such page: but the middleware ran and saw the request
  for (const name of Object.keys(forged)) assert.equal(globalThis.__seen.headers[name], undefined, `middleware saw ${name}`);
  const direct = await call('/echo', { headers: forged });
  for (const name of Object.keys(forged)) assert.equal(direct.json.headers[name], undefined, `handler saw ${name}`);
  assert.equal(direct.json.url, '/echo');
  assert.ok(json === null || json.url !== '/throws');
});

// --- middleware -------------------------------------------------------------------------------

test('G11: a path spelled with percent-encoded letters still runs the middleware that guards it', async () => {
  const before = globalThis.__calls.echo ?? 0;
  for (const path of ['/mw/guarded', '/%6Dw/guarded', '/mw/%67uarded', '/%6dw/%67%75arded']) {
    const { status } = await call(path);
    assert.equal(status, 401, path);
  }
  assert.equal(globalThis.__calls.echo ?? 0, before);
});

test('G3: a middleware that throws is a 500, and the page never runs', async () => {
  const before = globalThis.__calls.echo ?? 0;
  const { status, headers } = await call('/mw/throw');
  assert.equal(status, 500);
  assert.match(headers['cache-control'], /no-store/);
  assert.equal(globalThis.__calls.echo ?? 0, before);
});

test("G4: the middleware's request headers replace the request's", async () => {
  manifest.functions['/mw/headers'] = fn('echo', 'APP_ROUTE');
  const fresh = createNextServer({ root, manifest, routing, log: { error() {} } });
  const local = http.createServer((req, res) => fresh.handle(req, res));
  await new Promise((resolve) => local.listen(0, '127.0.0.1', resolve));
  try {
    const json = await new Promise((resolve) => {
      http.get(
        {
          host: '127.0.0.1',
          port: local.address().port,
          path: '/mw/headers',
          agent: false,
          headers: { host: 'shop.example', 'x-forwarded-proto': 'https', 'x-user': 'mallory', 'x-secret': 'dropped-by-middleware', cookie: 'session=stolen' },
        },
        (res) => {
          let text = '';
          res.on('data', (c) => (text += c));
          res.on('end', () => resolve({ body: JSON.parse(text), headers: res.headers }));
        },
      );
    });
    assert.equal(json.body.headers['x-user'], 'alice');
    assert.equal(json.body.headers.cookie, 'session=ok');
    // Not in the middleware's list, so gone: a merge would have brought it back.
    assert.equal(json.body.headers['x-secret'], undefined);
    // The cookie the middleware set travels to the page as Next expects, and to the visitor.
    assert.equal(json.body.headers['x-middleware-set-cookie'], 'fresh=1; Path=/');
    assert.deepEqual(json.headers['set-cookie'], ['fresh=1; Path=/']);
    assert.equal(json.headers['x-middleware-set-cookie'], undefined);
  } finally {
    local.close();
    delete manifest.functions['/mw/headers'];
  }
});

test('G4: the middleware runs once, and both it and the page read a POST body', async () => {
  manifest.functions['/mw/body'] = fn('echo', 'APP_ROUTE');
  const fresh = createNextServer({ root, manifest, routing, log: { error() {} } });
  const local = http.createServer((req, res) => fresh.handle(req, res));
  await new Promise((resolve) => local.listen(0, '127.0.0.1', resolve));
  try {
    globalThis.__calls.middleware = 0;
    const result = await new Promise((resolve) => {
      const req = http.request({ host: '127.0.0.1', port: local.address().port, path: '/mw/body', method: 'POST', agent: false, headers: { host: 'shop.example' } }, (res) => {
        let text = '';
        res.on('data', (c) => (text += c));
        res.on('end', () => resolve({ body: JSON.parse(text), headers: res.headers }));
      });
      req.write('hello ');
      setTimeout(() => req.end('world'), 30);
    });
    assert.equal(globalThis.__calls.middleware, 1);
    assert.equal(result.headers['x-body-seen'], '11');
    assert.equal(result.body.body, 'hello world');
  } finally {
    local.close();
    delete manifest.functions['/mw/body'];
  }
});

test("G9: a middleware's own answer goes out without its protocol headers", async () => {
  const { status, text, headers } = await call('/mw/respond');
  assert.equal(status, 418);
  assert.equal(text, 'from middleware');
  assert.equal(headers['x-mine'], '1');
  assert.deepEqual(headers['set-cookie'], ['m=1']);
  for (const name of Object.keys(headers)) assert.ok(!name.startsWith('x-middleware-'), name);
});

test("a middleware's redirect on the same origin is a path", async () => {
  const { status, headers } = await call('/mw/redirect');
  assert.equal(status, 307);
  assert.equal(headers.location, '/echo?from=mw');
});

test("a middleware's rewrite: the page is told the query; the URL keeps the visitor's path with the rewrite's query", async () => {
  const { json, headers } = await call('/mw/rewrite?own=1');
  // As Vercel's proxy passes it: the rewrite (`/blog/from-mw?added=1`) names the whole query.
  assert.equal(json.url, '/mw/rewrite?added=1');
  assert.deepEqual(json.meta.params, { slug: 'from-mw' });
  assert.equal(json.meta.query.added, '1');
  assert.equal(json.meta.query.nxtPslug, undefined);
  assert.equal(headers['x-middleware-rewrite'], '/blog/from-mw?added=1');
});

// --- redirects and rewrites -------------------------------------------------------------------

test('config redirects: a path on our origin, a URL elsewhere, Refresh on 308', async () => {
  const kept = await call('/old/x?keep=1');
  assert.equal(kept.status, 308);
  assert.equal(kept.headers.location, '/blog/x?keep=1');
  assert.equal(kept.headers.refresh, '0;url=/blog/x?keep=1');
  assert.equal(kept.text, '/blog/x?keep=1');
  const away = await call('/away');
  assert.equal(away.status, 307);
  assert.equal(away.headers.location, 'https://elsewhere.example/landing');
  assert.equal(away.headers.refresh, undefined);
});

test("Next's own trailing-slash redirect comes before config headers and the middleware", async () => {
  globalThis.__calls.middleware = 0;
  const { status, headers } = await call('/mw/x/?a=1');
  assert.equal(status, 308);
  assert.equal(headers.location, '/mw/x?a=1');
  assert.equal(globalThis.__calls.middleware, 0);
  const echo = await call('/echo/');
  assert.equal(echo.headers['x-from-config'], undefined);
});

test('repeated slashes redirect, and can never name another host', async () => {
  const { status, headers } = await call('//elsewhere.example//x?q=1');
  assert.equal(status, 308);
  assert.equal(headers.location, '/elsewhere.example/x?q=1');
});

test('G7: a rewrite to another origin is proxied without our internal headers', async () => {
  const { status, json, headers } = await call('/ext/a?x=1', { headers: { 'x-forkrail-visitor': 'secret', 'x-forkrail-request-id': 'r1', connection: 'keep-alive', cookie: 'c=1' } });
  assert.equal(status, 200);
  assert.equal(headers['x-upstream'], '1');
  assert.equal(json.url, '/up/a?x=1');
  assert.equal(json.headers.cookie, 'c=1');
  assert.equal(json.headers['x-forwarded-host'], 'shop.example');
  assert.equal(json.headers.host, new URL(globalThis.__upstream).host);
  for (const name of Object.keys(json.headers)) assert.ok(!name.startsWith('x-forkrail-'), name);
  // A middleware's rewrite names its whole target: the visitor's query is not added to it.
  const viaMiddleware = await call('/mw/external?own=1');
  assert.equal(viaMiddleware.json.url, '/up?x=1');
});

// --- RSC ----------------------------------------------------------------------------------------

test('G6: an RSC request with the wrong _rsc is redirected to the right one', async () => {
  const wrong = await call('/page?q=a%20b&_rsc=wrong', { headers: { rsc: '1', 'next-router-prefetch': '1', 'next-router-segment-prefetch': '/_tree' } });
  assert.equal(wrong.status, 307);
  assert.equal(wrong.headers.location, '/page?q=a%20b&_rsc=_i_aeImnuN6u1u1r');
  const none = await call('/page', { headers: { rsc: '1' } });
  assert.equal(none.status, 307);
  assert.equal(none.headers.location, '/page?_rsc');
  const right = await call('/page?q=1&_rsc=_i_aeImnuN6u1u1r', { headers: { rsc: '1', 'next-router-prefetch': '1', 'next-router-segment-prefetch': '/_tree' } });
  assert.equal(right.status, 200);
  // The URL is the visitor's, parameter and all (as under Next's own server).
  assert.equal(right.json.url, '/page?q=1&_rsc=_i_aeImnuN6u1u1r');
});

test('an App Router answer varies by the RSC headers; next-url only where it matters', async () => {
  const { headers, json } = await call('/echo', { headers: { 'next-url': '/from' } });
  assert.equal(headers.vary, 'rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding');
  assert.equal(json.headers['next-url'], undefined);
  const pages = await call('/old-page');
  assert.equal(pages.headers.vary, 'Accept-Encoding');
});

test('RSC path forms are served as the page', async () => {
  const { json } = await call('/page.segments/_tree.segment.rsc?_rsc=_i_aeImnuN6u1u1r');
  assert.equal(json.url, '/page?_rsc=_i_aeImnuN6u1u1r');
  assert.equal(json.headers.rsc, '1');
  assert.equal(json.headers['next-router-segment-prefetch'], '/_tree');
  assert.equal(json.meta.isRSCRequest, true);
  assert.equal(json.meta.segment, '/_tree');
});

// --- what Next's server settles before a page ---------------------------------------------------

test('G5: a POST to a prerendered page is 405, forged resume or not; a server action goes through', async () => {
  for (const headers of [{}, { 'next-resume': '1' }]) {
    const { status, headers: answer, text } = await call('/ssg', { method: 'POST', headers, body: 'postponed state a visitor made up' });
    assert.equal(status, 405);
    assert.equal(answer.allow, 'GET, HEAD');
    assert.equal(text, 'Method Not Allowed');
  }
  const action = await call('/ssg', { method: 'POST', headers: { 'next-action': 'abc' }, body: '[]' });
  assert.equal(action.status, 200);
  assert.equal(action.json.body, '[]');
  assert.equal(action.json.meta.postponed, undefined);
  const dynamic = await call('/page', { method: 'POST', headers: { 'next-resume': '1' }, body: 'x' });
  assert.equal(dynamic.json.headers['next-resume'], undefined);
});

test('OPTIONS is for route handlers only', async () => {
  assert.equal((await call('/page', { method: 'OPTIONS' })).status, 400);
  assert.equal((await call('/echo', { method: 'OPTIONS' })).status, 200);
});

test("the Pages Router's speculative data request doesn't run a page that would have to render", async () => {
  const before = globalThis.__calls.echo;
  const { status, text, headers } = await call('/old-page', { headers: { 'x-middleware-prefetch': '1' } });
  assert.equal(status, 200);
  assert.equal(text, '{}');
  assert.equal(headers['x-middleware-skip'], '1');
  assert.match(headers['cache-control'], /no-store/);
  assert.equal(globalThis.__calls.echo, before);
});

// --- 404 and errors -----------------------------------------------------------------------------

test("G10: a path nothing answers gets the app's not-found page, uncacheable", async () => {
  const { status, json, headers } = await call('/nothing/here');
  assert.equal(status, 404);
  assert.equal(json.status, 404);
  assert.equal(json.url, '/nothing/here');
  assert.equal(headers['cache-control'], 'private, no-cache, no-store, max-age=0, must-revalidate');
});

test('G10: a missing asset or subresource is three words, never a page', async () => {
  const asset = await call('/_next/static/chunks/gone.js');
  assert.equal(asset.status, 404);
  assert.equal(asset.text, 'Not Found');
  assert.match(asset.headers['cache-control'], /no-store/);
  const image = await call('/gone.png', { headers: { 'sec-fetch-dest': 'image' } });
  assert.equal(image.status, 404);
  assert.equal(image.text, 'Not Found');
});

test('a page that says "no such path" (fallback: false) is a 404', async () => {
  const { status, json } = await call('/nofallback');
  assert.equal(status, 404);
  assert.equal(json.status, 404);
});

test("a page asking for the 404 itself gets it, with the page's own caching left alone", async () => {
  const { status, json, headers } = await call('/gone');
  assert.equal(status, 404);
  assert.equal(json.status, 404);
  assert.equal(headers['cache-control'], undefined);
});

test('G10: a page that throws is the 500 page, uncacheable, and the error is logged', async () => {
  logged.length = 0;
  const { status, text, headers } = await call('/throws');
  assert.equal(status, 500);
  assert.equal(text, '<h1>500</h1>');
  assert.match(headers['cache-control'], /no-store/);
  assert.ok(logged.some((line) => line.includes('the page broke')));
});

test('a path that is not valid percent-encoding is a bad request', async () => {
  assert.equal((await call('/%E0%A4%A')).status, 400);
  assert.equal((await call('/blog/%E0%A4%A')).status, 400);
  // An encoded slash inside a param is a fine request.
  assert.deepEqual((await call('/blog/a%2Fb')).json.meta.params, { slug: 'a/b' });
});

// --- statics, streaming, background work ----------------------------------------------------------

test('a static file is served; writing to one is refused', async () => {
  const { status, text, headers } = await call('/file.txt');
  assert.equal(status, 200);
  assert.equal(text, 'a static file\n');
  assert.match(headers['content-type'], /^text\/plain/);
  assert.equal(headers['accept-ranges'], 'bytes');
  const part = await call('/file.txt', { headers: { range: 'bytes=2-7' } });
  assert.equal(part.status, 206);
  assert.equal(part.text, 'static');
  assert.equal(part.headers['content-range'], 'bytes 2-7/14');
  assert.equal((await call('/file.txt', { headers: { range: 'bytes=-5' } })).text, 'file\n');
  assert.equal((await call('/file.txt', { headers: { range: 'bytes=99-' } })).status, 416);
  assert.equal((await call('/file.txt', { headers: { 'if-none-match': headers.etag } })).status, 304);
  const post = await call('/file.txt', { method: 'POST', body: 'x' });
  assert.equal(post.status, 405);
  assert.equal(post.headers.allow, 'GET, HEAD');
});

test('a streamed page arrives as it is made, compressed or not', async () => {
  for (const encoding of ['identity', 'gzip']) {
    const started = performance.now();
    const { text, times, headers } = await call('/stream', { headers: { 'accept-encoding': encoding } });
    assert.ok(text.startsWith('<p>first</p>') && text.endsWith('<p>second</p>'));
    assert.equal(headers['content-encoding'], encoding === 'gzip' ? 'gzip' : undefined);
    assert.match(headers.vary, /Accept-Encoding/);
    // The first part came well before the second was written (150 ms later).
    assert.ok(times[0] - started < 100, `first bytes after ${times[0] - started} ms with ${encoding}`);
    assert.ok(times.at(-1) - times[0] > 100);
  }
});

test('background work outlives the response and its failure is only reported', async () => {
  logged.length = 0;
  globalThis.__calls.later = 0;
  const { text } = await call('/later');
  assert.equal(text, 'ok');
  assert.equal(globalThis.__calls.later, 0);
  await app.idle();
  assert.equal(globalThis.__calls.later, 1);
  assert.ok(logged.some((line) => line.includes('background work failed')));
});

test("a Pages API route is handed an incremental cache made for its request, as Next's own server makes one", async () => {
  const first = await call('/cached-api');
  const second = await call('/cached-api');
  assert.equal(first.status, 200);
  // 16.4's form: (req, config, preview keys, prerender manifest, minimal mode).
  assert.deepEqual(first.json, { id: first.json.id, args: 5, config: { marker: 'required-server-files' }, preview: { previewModeId: 'pid' }, reset: true, global: first.json.id });
  assert.equal(second.json.id, first.json.id + 1);
});

test('res.revalidate() renders the page again inside this process', async () => {
  const before = globalThis.__calls.echo;
  const ok = await call('/revalidates?path=/blog/again');
  assert.equal(ok.text, 'revalidated');
  assert.equal(globalThis.__calls.echo, before + 1);
  const bad = await call('/revalidates?path=/throws');
  assert.equal(bad.status, 500);
  assert.match(bad.text, /Invalid response 500/);
});

// --- the start ------------------------------------------------------------------------------------

test('the start loads one entrypoint of each kind, then others only while its budget lasts', () => {
  const loads = [];
  const fresh = createNextServer({ root, manifest, routing, log: { error: (...args) => loads.push(args.join(' ')) } });
  const shared = fresh.preload({ budgetMs: 0 });
  // The middleware, the not-found page (the page runtime), a route handler, a Pages page, an API route.
  assert.equal(shared.loaded, 3); // middleware.js, echo.js (not-found, route and page share it), revalidates.js
  assert.ok(shared.of > shared.loaded);
  const all = fresh.preload({ budgetMs: Infinity });
  assert.equal(all.loaded, all.of);
  assert.deepEqual(loads, []);
  // A module that fails to load is reported and left for its first request.
  manifest.functions['/broken'] = fn('does-not-exist', 'APP_PAGE');
  try {
    const again = createNextServer({ root, manifest, routing, log: { error: (...args) => loads.push(args.join(' ')) } });
    const result = again.preload({ budgetMs: Infinity });
    assert.equal(result.loaded, result.of - 1);
    assert.ok(loads.some((line) => line.includes('does-not-exist')));
  } finally {
    delete manifest.functions['/broken'];
  }
});

test("priming loads the shared entrypoints, renders Next's error page without the middleware, then the named pages", async () => {
  const fresh = createNextServer({ root, manifest, routing, log: { error: (...args) => logged.push(args.map(String).join(' ')) } });
  globalThis.__calls.middleware = 0;
  globalThis.__last = null;
  const before = globalThis.__calls.echo;
  const done = await fresh.prime({
    deadline: Date.now() + 5000,
    previewId: 'preview-id',
    paths: ['/page', '/mw/nothing'],
    visitor: { host: 'site.example', 'x-forwarded-host': 'site.example', 'x-forwarded-proto': 'https' },
  });
  // `/_global-error` once, as an on-demand revalidation, its matching middleware not run; then
  // each named page as a visitor, middleware included.
  assert.equal(done.request, '/_global-error 200');
  assert.equal(done['/page'], 200);
  assert.equal(done['/mw/nothing'], 404);
  assert.equal(globalThis.__calls.middleware, 1);
  // The error page, `/page`, and the not-found page `/mw/nothing` landed on.
  assert.equal(globalThis.__calls.echo, before + 3);
  assert.equal(globalThis.__last.url, '/mw/nothing');
  assert.equal(globalThis.__last.headers['x-forwarded-host'], 'site.example');
  // Every entrypoint loaded within the deadline.
  assert.match(done.entries, /^(\d+)\/\1$/);
  // A visitor's own `x-prerender-revalidate` on the error page goes through the middleware.
  await call('/_global-error', { headers: { 'x-prerender-revalidate': 'preview-id' } });
  assert.equal(globalThis.__calls.middleware, 2);
});

test('priming stops at its deadline and asks for nothing without a preview id', async () => {
  const fresh = createNextServer({ root, manifest, routing, log: { error() {} } });
  const before = globalThis.__calls.echo;
  const late = await fresh.prime({ deadline: Date.now(), previewId: 'preview-id', paths: ['/page'] });
  assert.equal(late.request, undefined);
  assert.equal(late['/page'], undefined);
  assert.equal(late.entries, `0/${late.entries.split('/')[1]}`);
  const none = await fresh.prime({ deadline: Date.now() + 5000 });
  assert.equal(none.request, undefined);
  assert.equal(globalThis.__calls.echo, before);
});

test("background work is handed to the runtime's waitUntil when there is one", async () => {
  const handed = [];
  globalThis[Symbol.for('forkrail.waitUntil')] = (promise) => handed.push(promise);
  try {
    const { text } = await call('/later');
    assert.equal(text, 'ok');
    assert.equal(handed.length, 2);
    // What is handed over never rejects: a failure is reported here.
    await Promise.all(handed);
    await app.idle();
  } finally {
    delete globalThis[Symbol.for('forkrail.waitUntil')];
  }
});

test('a rewrite onto a static file the router serves and the package lacks: the router is told to answer with it', async () => {
  // `/files/:path*` to `/:path*`: Next's routing lands on a file the router serves.
  for (const [asked, named] of [
    ['/files/router-only.txt', '/router-only.txt'],
    ['/router-only.txt', '/router-only.txt'],
    ['/files/caf%C3%A9.txt', '/caf%C3%A9.txt'],
  ]) {
    const { status, headers, text } = await call(asked);
    assert.equal(status, 200, asked);
    assert.equal(headers['x-forkrail-static'], named, asked);
    assert.equal(text, '', asked);
  }
  // A HEAD alike; a write is refused here, as for any file.
  assert.equal((await call('/files/router-only.txt', { method: 'HEAD' })).headers['x-forkrail-static'], '/router-only.txt');
  const post = await call('/files/router-only.txt', { method: 'POST', body: 'x' });
  assert.equal(post.status, 405);
  assert.equal(post.headers['x-forkrail-static'], undefined);
  // A file the package holds is served here, as before.
  const held = await call('/files/file.txt');
  assert.equal(held.headers['x-forkrail-static'], undefined);
  assert.equal(held.status, 200);
});
