// The Next.js server (server.mjs) for an app with locales, around a made-up entrypoint that
// answers with the params it was given, routed by the real `@next/routing` with the rules Next
// 16.4.6 built for Next's own `test/e2e/i18n-support-index-rewrite` (a `fallback: false`
// catch-all at the top, prerendered under each locale, and `/` rewritten to one of its paths).
//
// Run: node --test "crates/runtime/src/next/*.test.mjs" (needs `@next/routing`, as server.test.mjs).

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { createNextServer } from './server.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const routingPath = process.env.FORKRAIL_TEST_NEXT_ROUTING ?? path.join(here, '../../../../tools/next-n8/node_modules/@next/routing');
const routing = createRequire(import.meta.url)(routingPath);

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'forkrail-next-i18n-test-'));
fs.writeFileSync(path.join(root, 'package.json'), '{}');
fs.mkdirSync(path.join(root, 'fn'));
fs.writeFileSync(
  path.join(root, 'fn/echo.js'),
  `exports.handler = async (req, res, ctx) => {
    res.setHeader('content-type', 'application/json')
    res.end(JSON.stringify({ params: ctx.requestMeta.params, locale: ctx.requestMeta.locale }))
  }`,
);

const locales = ['en', 'nl-NL', 'nl', 'fr'];
const page = (id) => ({ id, type: 'PAGES', file: 'fn/echo.js', runtime: 'nodejs', sourcePage: '[...slug]', ssg: true });
const functions = { '/[...slug]': page('/[...slug]') };
const prerenders = {};
for (const locale of locales) {
  functions[`/${locale}/[...slug]`] = page(`/${locale}/[...slug]`);
  for (const rest of ['hello', 'company/about-us']) prerenders[`/${locale}/${rest}`] = { parent: '/[...slug]', routeType: 'page' };
}
const alternatives = locales.map((l) => l.replace('-', '\\-')).join('|');
const manifest = {
  version: 1,
  nextVersion: '16.4.6',
  buildId: 'BUILD',
  deploymentId: '',
  projectDir: '.',
  distDir: '.next',
  basePath: '',
  trailingSlash: false,
  i18n: { defaultLocale: 'en', locales },
  routing: {
    beforeMiddleware: [{ headers: { Location: '/$1' }, priority: true, source: '/:path+/', sourceRegex: '^(?:\\/((?:[^\\/]+?)(?:\\/(?:[^\\/]+?))*))\\/$', status: 308 }],
    middlewareMatchers: [],
    beforeFiles: [],
    afterFiles: [
      {
        source: `/:nextInternalLocale(${alternatives})`,
        sourceRegex: `^(?:\\/(${alternatives}))(?:\\/)?$`,
        destination: '/$1/company/about-us?nextInternalLocale=$1',
      },
      // As Next 16.4 builds `/files/:path*` to `/:path*` under locales: the target keeps the locale.
      {
        source: `/:nextInternalLocale(${alternatives})/files/:path*`,
        sourceRegex: `^(?:\\/(${alternatives}))\\/files(?:\\/((?:[^\\/]+?)(?:\\/(?:[^\\/]+?))*))?(?:\\/)?$`,
        destination: '/$1/$2',
      },
    ],
    // `fallback: false`: the pattern matches only in Draft Mode; any other path is found only as
    // one of the build's prerenders.
    dynamicRoutes: [
      {
        source: '/[...slug]',
        sourceRegex: '^[/]?(?<nextLocale>[^/]{1,})/(?<nxtPslug>.+?)(?:/)?$',
        destination: '/$nextLocale/[...slug]?nxtPslug=$nxtPslug',
        has: [{ key: '__prerender_bypass', type: 'cookie', value: 'preview-id' }],
      },
    ],
    onMatch: [],
    fallback: [],
    shouldNormalizeNextData: false,
    rsc: { header: 'rsc', suffix: '.rsc', prefetchSegmentDirSuffix: '.segments', prefetchSegmentSuffix: '.segment.rsc' },
  },
  functions,
  middleware: null,
  prerenders,
  // A file in `public/` the router serves (not in the package).
  statics: { '/dot.png': null },
};

let server;
let port;
before(async () => {
  const app = createNextServer({ root, manifest, routing, log: { error: () => {} } });
  server = http.createServer((req, res) => app.handle(req, res));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  port = server.address().port;
});
after(() => {
  server.close();
  fs.rmSync(root, { recursive: true, force: true });
});

async function get(target) {
  const res = await fetch(`http://127.0.0.1:${port}${target}`, { headers: { 'x-forwarded-host': 'shop.example', 'x-forwarded-proto': 'https' } });
  const text = await res.text();
  return { status: res.status, body: res.headers.get('content-type')?.includes('json') ? JSON.parse(text) : text };
}

test("a localized page of a top-level catch-all is told its params without the locale", async () => {
  assert.deepEqual(await get('/hello'), { status: 200, body: { params: { slug: ['hello'] }, locale: 'en' } });
  assert.deepEqual(await get('/en/hello'), { status: 200, body: { params: { slug: ['hello'] }, locale: 'en' } });
  assert.deepEqual(await get('/nl-NL/hello'), { status: 200, body: { params: { slug: ['hello'] }, locale: 'nl-NL' } });
  assert.deepEqual(await get('/fr/company/about-us'), { status: 200, body: { params: { slug: ['company', 'about-us'] }, locale: 'fr' } });
});

test("a locale's home page rewritten to one of the catch-all's paths gets that path's params", async () => {
  assert.deepEqual(await get('/'), { status: 200, body: { params: { slug: ['company', 'about-us'] }, locale: 'en' } });
  assert.deepEqual(await get('/nl'), { status: 200, body: { params: { slug: ['company', 'about-us'] }, locale: 'nl' } });
});

test("a config rewrite onto a public file under locales finds the file without the locale, as Next's filesystem check does", async () => {
  for (const target of ['/files/dot.png', '/fr/files/dot.png']) {
    const res = await fetch(`http://127.0.0.1:${port}${target}`);
    assert.equal(res.status, 200, target);
    assert.equal(res.headers.get('x-forkrail-static'), '/dot.png', target);
  }
  // A path that is no file stays a 404.
  assert.equal((await get('/fr/files/nope.png')).status, 404);
});
