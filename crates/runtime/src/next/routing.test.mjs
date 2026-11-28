// The Next.js server's own logic, without Next: what it takes off a request, how it reads a URL,
// what it tells a handler (routing.mjs), and when it compresses (compress.mjs).
//
// Run: node --test "crates/runtime/src/next/*.test.mjs"

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { compressible, encodingFor } from './compress.mjs';
import {
  dataPagePath,
  decodedPath,
  findOutput,
  handlerQuery,
  isDataPath,
  isMiddlewareProtocol,
  location,
  paramsFromPath,
  publicOrigin,
  repeatedSlashes,
  routablePathnames,
  routeParams,
  rscHash,
  rscParam,
  rscTarget,
  stripInternalQuery,
  toNodeHeaders,
  toWebHeaders,
  untrusted,
  versionVerdict,
  withRsc,
  urlSpelling,
} from './routing.mjs';

test('headers only the platform may set are taken off a request', () => {
  const headers = untrusted(
    {
      host: 'shop.example',
      cookie: 'a=1',
      'next-resume': '1',
      'x-next-resume-state-length': '10',
      'x-matched-path': '/admin',
      'x-now-route-matches': '1=admin',
      'x-invocation-id': 'shared',
      'x-middleware-rewrite': '/admin',
      'x-middleware-set-cookie': 'session=forged',
      'x-middleware-override-headers': 'x-user',
      'x-middleware-request-x-user': 'root',
      'x-middleware-next': '1',
      'x-nextjs-data': '1',
      'x-invoke-path': '/admin',
      // Next's own client sends these two; they are the request, not routing's verdict.
      'x-middleware-prefetch': '1',
      rsc: '1',
    },
    false,
  );
  assert.deepEqual(headers, { host: 'shop.example', cookie: 'a=1', 'x-middleware-prefetch': '1', rsc: '1' });
});

test('x-nextjs-data survives only on a data URL', () => {
  assert.equal(untrusted({ 'x-nextjs-data': '1' }, true)['x-nextjs-data'], '1');
  assert.equal(untrusted({ 'x-nextjs-data': '1' }, false)['x-nextjs-data'], undefined);
  assert.ok(isDataPath('/_next/data/BUILD/blog/a.json', '', 'BUILD'));
  assert.ok(isDataPath('/docs/_next/data/BUILD/index.json', '/docs', 'BUILD'));
  assert.ok(!isDataPath('/_next/data/OTHER/blog/a.json', '', 'BUILD'));
  assert.ok(!isDataPath('/_next/data/BUILD/blog/a', '', 'BUILD'));
  assert.equal(dataPagePath('/_next/data/BUILD/blog/a.json', '', 'BUILD'), '/blog/a');
  assert.equal(dataPagePath('/_next/data/BUILD/index.json', '', 'BUILD'), '/');
});

test("the app's origin is the visitor's, from the router's headers", () => {
  assert.deepEqual(publicOrigin({ host: 'shop.example', 'x-forwarded-proto': 'https' }), { proto: 'https', host: 'shop.example', origin: 'https://shop.example' });
  // A forwarded server action arrives on our own loopback address with the visitor's host kept.
  assert.equal(publicOrigin({ host: '127.0.0.1:4123', 'x-forwarded-host': 'shop.example', 'x-forwarded-proto': 'https' }).origin, 'https://shop.example');
  assert.equal(publicOrigin({ host: 'localhost:3000' }).origin, 'http://localhost:3000');
  assert.equal(publicOrigin({ host: 'shop.example', 'x-forwarded-proto': 'javascript' }).proto, 'http');
  assert.equal(publicOrigin({ host: 'shop.example', 'x-forwarded-proto': 'HTTPS, http' }).proto, 'https');
  // A host that isn't one can't become part of a URL or a redirect.
  assert.equal(publicOrigin({ host: 'evil.example/path?x', 'x-forwarded-host': 'a b' }).host, 'localhost');
  assert.equal(publicOrigin({}).origin, 'http://localhost');
});

test('repeated slashes and backslashes redirect to the cleaned path, query kept', () => {
  assert.equal(repeatedSlashes('/a/b'), null);
  assert.equal(repeatedSlashes('/a?next=//evil'), null);
  assert.equal(repeatedSlashes('//a//b/?x=1'), '/a/b/?x=1');
  assert.equal(repeatedSlashes('/a\\b'), '/a/b');
  assert.equal(repeatedSlashes('//evil.example/x'), '/evil.example/x');
});

test('RSC path forms are the page plus the headers the client would send', () => {
  const rsc = { suffix: '.rsc', prefetchSegmentDirSuffix: '.segments', prefetchSegmentSuffix: '.segment.rsc' };
  assert.deepEqual(rscTarget('/blog/a.rsc', rsc), { pathname: '/blog/a', headers: { rsc: '1' }, segment: null });
  assert.deepEqual(rscTarget('/index.rsc', rsc).pathname, '/');
  assert.deepEqual(rscTarget('/blog/a.segments/blog/$d$slug/__PAGE__.segment.rsc', rsc), {
    pathname: '/blog/a',
    headers: { rsc: '1', 'next-router-prefetch': '1', 'next-router-segment-prefetch': '/blog/$d$slug/__PAGE__' },
    segment: '/blog/$d$slug/__PAGE__',
  });
  assert.equal(rscTarget('/index.segments/_tree.segment.rsc', rsc).pathname, '/');
  assert.equal(rscTarget('/blog/a', rsc), null);
  assert.equal(rscTarget('/.rsc', rsc)?.pathname, '/');
  assert.equal(rscTarget('/blog/a.rsc', undefined), null);
});

test("a visitor's own nxtP / nxtI query keys never reach a handler", () => {
  assert.equal(stripInternalQuery('?a=1&nxtPslug=evil&b=2'), '?a=1&b=2');
  assert.equal(stripInternalQuery('?nxtIslug=evil'), '');
  assert.equal(stripInternalQuery('?__nextLocale=fr&x=1'), '?x=1');
  // Untouched when there is nothing to take out: the app sees the query as it was sent.
  assert.equal(stripInternalQuery('?q=a+b&t=%7E'), '?q=a+b&t=%7E');
  assert.equal(stripInternalQuery(''), '');
  assert.deepEqual(handlerQuery({ a: '1', nxtPslug: 'x', nxtIid: 'y', tags: ['a', 'b'] }), { a: '1', tags: ['a', 'b'] });
});

test('an internal key is caught however it is spelled', () => {
  // `%6E` is `n`: the handler's query parser would read the key as `nxtPslug`.
  assert.equal(stripInternalQuery('?a=1&%6ExtPslug=evil'), '?a=1');
  assert.equal(stripInternalQuery('?nxt%50slug=evil&a=%20'), '?a=%20');
  assert.equal(stripInternalQuery('?%6Ext%49id=1'), '');
  // A `%` that spells nothing of ours changes nothing, empty pairs included.
  assert.equal(stripInternalQuery('?a=%E0%A4%A&&b'), '?a=%E0%A4%A&&b');
  assert.equal(stripInternalQuery('?next=1&nxt=2'), '?next=1&nxt=2');
});

test("a route's params come from routing's matches, decoded once", () => {
  assert.deepEqual(routeParams('/blog/[slug]', { nxtPslug: 'a%20b' }), { slug: 'a b' });
  assert.deepEqual(routeParams('/docs/[...path]', { nxtPpath: 'a/b%2Fc/d' }), { path: ['a', 'b/c', 'd'] });
  assert.deepEqual(routeParams('/docs/[[...path]]', { nxtPpath: 'a/b/' }), { path: ['a', 'b'] });
  // An optional catch-all that matched nothing has no value.
  assert.equal(routeParams('/docs/[[...path]]', { nxtPpath: '' }), undefined);
  assert.equal(routeParams('/docs/[[...path]]', {}), undefined);
  assert.deepEqual(routeParams('/[lang]/shop/[id]', { nxtPlang: 'en', nxtPid: '7', nxtPother: 'x', 1: 'en' }), { lang: 'en', id: '7' });
  // An intercepted route's own param name.
  assert.deepEqual(routeParams('/photo/[id]', { nxtIid: '9' }), { id: '9' });
  assert.equal(routeParams('/about', { nxtPslug: 'x' }), undefined);
  assert.equal(routeParams('/blog/[slug]', undefined), undefined);
  // Not decodable: the server answers 400.
  assert.equal(routeParams('/blog/[slug]', { nxtPslug: '%E0%A4%A' }), null);
});

test('a request path is read as the build names prerendered paths', () => {
  assert.equal(decodedPath('/sticks%20%26%20stones'), '/sticks & stones');
  assert.equal(decodedPath('/closed/docs/space%20here/100%25'), '/closed/docs/space here/100%');
  // An encoded slash is part of a name, not a separator.
  assert.equal(decodedPath('/open/with%2Fslash/x'), '/open/with%2Fslash/x');
  assert.equal(decodedPath('/plain/path'), '/plain/path');
  assert.equal(decodedPath('/%E0%A4%A'), null);
});

test("a prerendered path's params are read off the path", () => {
  assert.deepEqual(paramsFromPath('/blog/[slug]', '/blog/a%20b'), { slug: 'a b' });
  assert.deepEqual(paramsFromPath('/docs/[...path]', '/docs/a/b'), { path: ['a', 'b'] });
  assert.deepEqual(paramsFromPath('/docs/[[...path]]', '/docs/a/'), { path: ['a'] });
  assert.equal(paramsFromPath('/docs/[[...path]]', '/docs'), undefined);
  assert.deepEqual(paramsFromPath('/[lang]/shop/[id]', '/en/shop/7'), { lang: 'en', id: '7' });
  assert.equal(paramsFromPath('/blog/[slug]', '/other/x'), undefined);
  assert.equal(paramsFromPath('/about', '/about'), undefined);
  // A literal part with characters that mean something to a pattern.
  assert.deepEqual(paramsFromPath('/a.b/[x]', '/a.b/1'), { x: '1' });
  // An intercepting route, matched as its prerendered path (with trailingSlash routing finds it so).
  assert.deepEqual(paramsFromPath('/photos/(.)[id]', '/photos/(.)1'), { id: '1' });
  assert.deepEqual(paramsFromPath('/photos/(.)[id]', '/photos/(.)1/'), { id: '1' });
  assert.deepEqual(paramsFromPath('/a/(..)(..)[...rest]', '/a/(..)(..)x/y'), { rest: ['x', 'y'] });
  assert.equal(paramsFromPath('/photos/(.)[id]', '/photos/1'), undefined);
  assert.equal(paramsFromPath('/a.b/[x]', '/aXb/1'), undefined);
});

test('an output is found through a prerendered path of it, and routing knows those paths', () => {
  const manifest = {
    basePath: '',
    trailingSlash: true,
    functions: { '/blog/[slug]': { id: '/blog/[slug]' }, '/something': { id: '/something' } },
    statics: { '/index': 'pages/index.html', '/logo.svg': 'public/logo.svg' },
    prerenders: { '/blog/first': { parent: '/blog/[slug]' }, '/_next/data/B/something.json': { parent: '/something' }, '/orphan': { parent: '/gone' } },
  };
  assert.deepEqual(findOutput(manifest, '/blog/first'), { kind: 'function', pathname: '/blog/[slug]', entry: { id: '/blog/[slug]' }, prerendered: '/blog/first' });
  assert.equal(findOutput(manifest, '/_next/data/B/something.json').pathname, '/something');
  assert.equal(findOutput(manifest, '/orphan'), null);
  assert.equal(findOutput(manifest, '/').file, 'pages/index.html');
  const names = routablePathnames(manifest);
  for (const name of ['/', '/index', '/blog/first', '/blog/first/', '/something/', '/_next/data/B/something.json', '/logo.svg']) assert.ok(names.includes(name), name);
  // A file's address never ends in a slash.
  assert.ok(!names.includes('/logo.svg/'));
  assert.ok(!routablePathnames({ ...manifest, trailingSlash: false }).includes('/something/'));
});

test("a redirect on the visitor's origin is a path, elsewhere a whole URL", () => {
  assert.equal(location(new URL('https://shop.example/a?b=1#c'), 'https://shop.example'), '/a?b=1#c');
  assert.equal(location(new URL('https://other.example/a'), 'https://shop.example'), 'https://other.example/a');
  assert.equal(location(new URL('http://shop.example/a'), 'https://shop.example'), 'http://shop.example/a');
});

test('headers convert both ways without folding cookies', () => {
  const web = toWebHeaders({ host: 'a', cookie: ['a=1', 'b=2'], 'x-list': ['1', '2'], 'set-cookie': ['s=1; Path=/', 't=2'], skipped: undefined });
  assert.equal(web.get('cookie'), 'a=1; b=2');
  assert.equal(web.get('x-list'), '1, 2');
  assert.deepEqual(web.getSetCookie(), ['s=1; Path=/', 't=2']);
  const node = toNodeHeaders(web);
  assert.deepEqual(node['set-cookie'], ['s=1; Path=/', 't=2']);
  assert.equal(node.cookie, 'a=1; b=2');
  assert.ok(isMiddlewareProtocol('x-middleware-rewrite'));
  assert.ok(isMiddlewareProtocol('x-middleware-request-cookie'));
  assert.ok(isMiddlewareProtocol('x-middleware-set-cookie'));
  assert.ok(!isMiddlewareProtocol('x-middleware-cache'));
  assert.ok(!isMiddlewareProtocol('x-custom'));
});

test('an output is found by its pathname, its slash form and its index form', () => {
  const manifest = {
    functions: { '/': { id: '/' }, '/blog/[slug]': { id: 'blog' }, '/docs/index': { id: 'docs' } },
    statics: { '/logo.svg': 'public/logo.svg', '/about': '.next/server/pages/about.html' },
  };
  assert.equal(findOutput(manifest, '/').entry.id, '/');
  assert.equal(findOutput(manifest, '/blog/[slug]').kind, 'function');
  assert.equal(findOutput(manifest, '/blog/[slug]/').entry.id, 'blog');
  assert.equal(findOutput(manifest, '/logo.svg').file, 'public/logo.svg');
  assert.equal(findOutput(manifest, '/about/').kind, 'static');
  assert.equal(findOutput(manifest, '/missing'), null);
  assert.equal(findOutput(manifest, undefined), null);
  // Names from `Object.prototype` are not outputs.
  assert.equal(findOutput(manifest, 'constructor'), null);
  assert.equal(findOutput(manifest, '__proto__'), null);
});

test("the RSC cache-busting parameter is the hash Next's client computes", async () => {
  // Values from a browser on Next 16.4.6 (docs/data/n0/real-apps.md: a segment prefetch of /isr).
  const tree = await rscHash({ 'next-router-prefetch': '1', 'next-router-segment-prefetch': '/_tree' });
  assert.equal(tree.hash, '_i_aeImnuN6u1u1r');
  assert.deepEqual(await rscHash({}), { hash: '', legacy: '' });
  // A prefetch value Next doesn't know counts as none.
  assert.deepEqual(await rscHash({ 'next-router-prefetch': '9' }), { hash: '', legacy: '' });
  const a = await rscHash({ 'next-router-state-tree': 'x', 'next-url': '/a' });
  const b = await rscHash({ 'next-router-state-tree': 'x', 'next-url': '/b' });
  assert.notEqual(a.hash, b.hash);
  assert.equal(a.hash.length, 16);
  assert.match(a.legacy, /^[0-9a-z]{1,5}$/);
});

test('_rsc is read, replaced and removed without touching the rest of the query', () => {
  assert.equal(rscParam('?a=1&_rsc=abc'), 'abc');
  assert.equal(rscParam('?_rsc'), '');
  assert.equal(rscParam('?a=1'), null);
  assert.equal(rscParam(''), null);
  assert.equal(withRsc('?q=a%20b&_rsc=old', 'new'), '?q=a%20b&_rsc=new');
  assert.equal(withRsc('?q=a%20b&_rsc=old', null), '?q=a%20b');
  assert.equal(withRsc('?_rsc=old', null), '');
  assert.equal(withRsc('', ''), '?_rsc');
  assert.equal(withRsc('?x=1', ''), '?x=1&_rsc');
});

test('only verified release lines are served by this server', () => {
  const verified = ['16.4'];
  assert.deepEqual(versionVerdict('16.4.6', verified), { ok: true, minor: '16.4' });
  assert.equal(versionVerdict('16.5.0', verified).ok, false);
  assert.equal(versionVerdict('16.3.9', verified).ok, false);
  assert.equal(versionVerdict('17.0.0', verified).ok, false);
  // A prerelease of a verified line is still not verified.
  assert.equal(versionVerdict('16.4.0-canary.48', verified).ok, false);
  assert.equal(versionVerdict('16.4.7-rc.1', verified).ok, false);
  assert.equal(versionVerdict('latest', verified).ok, false);
  assert.equal(versionVerdict(undefined, verified).ok, false);
});

test('what compresses, and with what', () => {
  for (const type of ['text/html; charset=utf-8', 'text/x-component', 'application/json', 'application/javascript; charset=UTF-8', 'image/svg+xml', 'application/manifest+json', 'text/plain']) {
    assert.ok(compressible(type), type);
  }
  for (const type of ['image/png', 'font/woff2', 'application/octet-stream', 'video/mp4', undefined, 42]) {
    assert.ok(!compressible(type), String(type));
  }
  assert.equal(encodingFor('gzip, deflate, br'), 'gzip');
  assert.equal(encodingFor('br'), null);
  assert.equal(encodingFor('deflate'), 'deflate');
  assert.equal(encodingFor('gzip;q=0, deflate'), 'deflate');
  assert.equal(encodingFor('*'), 'gzip');
  assert.equal(encodingFor('identity'), null);
  assert.equal(encodingFor(''), null);
  assert.equal(encodingFor(undefined), null);
});

test("a static file's name as a URL spells it", () => {
  assert.equal(urlSpelling('/another.txt'), '/another.txt');
  assert.equal(urlSpelling('/report 2024.pdf'), '/report%202024.pdf');
  assert.equal(urlSpelling('/café.txt'), '/caf%C3%A9.txt');
  assert.equal(urlSpelling('/100%.txt'), '/100%25.txt');
  // Next's own chunk names keep their brackets, as browsers send them.
  assert.equal(urlSpelling('/_next/static/chunks/app/[slug]/page-1a2b.js'), '/_next/static/chunks/app/[slug]/page-1a2b.js');
});
