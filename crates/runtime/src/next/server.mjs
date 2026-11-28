// forkrail's Next.js server ("H"), for Next 16.2 and later: every request is resolved with Next's
// own `@next/routing` and answered by calling the build's entrypoint `handler(req, res, ctx)`
// directly (Next's Adapters API). There is no Next server in the process: no `next start`, no
// router-server, no standalone `server.js`. About half the cold start and a sixth of the package
// of Next's own server (docs: N0's findings).
//
// The handlers are called the way Next's own server calls them (not in "minimal mode"), so they
// do what `next start` does: read and write the configured `cacheHandler` / `cacheHandlers`,
// serve prerendered and ISR pages with HIT / STALE / MISS, and do partial prerendering in one
// pass (the shell from the cache, then the rest resumed in the same response).
//
// What this file adds is what Next's server did around the handlers:
//   - the visitor's URL as the app's URL (`request.url`, redirects), from the router's headers;
//   - headers only the platform may set taken off the request (routing.mjs, `UNTRUSTED`);
//   - routing: redirects, rewrites (to another origin: proxied), config headers, i18n,
//     `_next/data`, middleware (`proxy.ts` / `middleware.ts`, Node or Edge);
//   - the request as the handler wants it: the public URL as `req.url`, the rewrite target, the
//     route's params and the resolved query in `requestMeta`;
//   - 404 and error pages, `res.revalidate()`, `waitUntil`.
//
// Static files are the router's on forkrail; this server answers them only when it has the file
// (a local run, or a rewrite that ends on one).

import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import querystring from 'node:querystring';
import { createRequire } from 'node:module';
import { PassThrough, Readable, Writable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import {
  dataPagePath,
  decodedPath,
  findOutput,
  handlerQuery,
  isDataPath,
  isHopByHop,
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
  withRsc,
  toNodeHeaders,
  toWebHeaders,
  untrusted,
  urlSpelling,
} from './routing.mjs';
import { compress } from './compress.mjs';
import { createEdge } from './edge.mjs';
import { createImages } from './image.mjs';
import { serveStatic } from './statics.mjs';

const builtin = createRequire(import.meta.url);
const REDIRECTS = new Set([301, 302, 303, 307, 308]);
const NO_STORE = 'private, no-cache, no-store, max-age=0, must-revalidate';
// The worker's word to the router that the answer is one of the deployment's static files.
const STATIC_HEADER = 'x-forkrail-static';
const RSC_VARY = 'rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch';
// On priming's own request for Next's error page (`prime`): no visitor's request can carry it.
const PRIMING = Symbol('forkrail.next.priming');
// `Sec-Fetch-Dest` values of requests that can never show HTML (an image, a script, a font): a
// 404 for one is three words, not the app's not-found page (Next's
// `server/lib/is-non-html-sec-fetch-dest.ts`).
const NON_HTML_DESTS = new Set([
  'audio',
  'audioworklet',
  'font',
  'image',
  'json',
  'manifest',
  'paintworklet',
  'report',
  'script',
  'serviceworker',
  'sharedworker',
  'style',
  'track',
  'video',
  'webidentity',
  'worker',
  'xslt',
]);

/// `routing` is the app's own `@next/routing` (the same version as its Next); left out, it is
/// the one packaged beside this file.
export function createNextServer({ root, manifest, routing = createRequire(import.meta.url)('@next/routing'), log = console }) {
  const projectDir = path.join(root, manifest.projectDir);
  const distDir = path.join(projectDir, manifest.distDir);
  const require = createRequire(path.join(projectDir, 'package.json'));
  const { basePath, buildId, i18n } = manifest;
  const imagePath = `${basePath}/_next/image`;
  // A static file whose name a URL spells another way (`/report 2024.pdf` as
  // `/report%202024.pdf`, `/café.txt` as `/caf%C3%A9.txt`): routing compares names as given, so
  // that spelling is one it may land on too (a rewrite's target keeps the URL's), and stands for
  // the file, as Next's own server finds a file by either.
  const urlSpellings = new Map();
  for (const name of Object.keys(manifest.statics)) {
    const spelledAs = urlSpelling(name);
    if (spelledAs !== name) urlSpellings.set(spelledAs, name);
  }
  const pathnames = [...routablePathnames(manifest), ...urlSpellings.keys(), imagePath, `${imagePath}/`];
  // Under locales a config rewrite keeps the locale in its target (`/files/:path*` to `/:path*`
  // lands on `/fr/dot.png`), and `@next/routing` (16.4) matches a target only as named, where
  // Next's own filesystem check finds a `public/` file without the locale. So where the app has
  // rewrites, each `public/` file is also named under each locale (found as the file in `route`).
  const rewrites = manifest.routing.beforeFiles.length + manifest.routing.afterFiles.length + manifest.routing.fallback.length;
  if (i18n && rewrites) {
    for (const name of Object.keys(manifest.statics)) {
      const inner = name.slice(basePath.length);
      if (!name.startsWith(basePath) || inner.startsWith('/_next/')) continue;
      for (const locale of i18n.locales) pathnames.push(`${basePath}/${locale}${inner}`);
    }
  }
  const known = new Set(pathnames);
  // Dynamic Pages Router API routes, the most specific first (more fixed parts, then fewer
  // catch-alls), for the locale case in `route`.
  const fixed = (pathname) => pathname.split('/').filter((part) => part && !part.startsWith('[')).length;
  const apiRoutes = Object.entries(manifest.functions)
    .filter(([pathname, entry]) => entry.type === 'PAGES_API' && pathname.includes('['))
    .map(([pathname, entry]) => ({ pathname, entry }))
    .sort((a, b) => fixed(b.pathname) - fixed(a.pathname) || (a.pathname.match(/\.\.\./g) ?? []).length - (b.pathname.match(/\.\.\./g) ?? []).length);
  const edge = createEdge({ projectDir, distDir, manifest, require });
  const modules = new Map();
  const background = new Set();
  // Next's own redirects that come before everything else (the trailing-slash rule): answered
  // before config headers and the middleware, as Next's server does.
  const priorityRedirects = manifest.routing.beforeMiddleware
    .filter((rule) => rule.priority && REDIRECTS.has(rule.status) && rule.headers?.Location)
    .map((rule) => ({ regex: new RegExp(rule.sourceRegex), to: rule.headers.Location, status: rule.status }));
  // Paths an intercepting route can answer (`app/@modal/(.)photo/[id]`): their answer depends on
  // where the visitor is navigating from (`next-url`).
  const interceptable = manifest.routing.beforeFiles.filter((rule) => rule.has?.[0]?.key === 'next-url').map((rule) => new RegExp(rule.sourceRegex));

  // A module is loaded on its first request and kept. Turbopack and webpack both write CommonJS
  // entries; `require` is also the fast path (no async module graph).
  function load(file) {
    let mod = modules.get(file);
    if (mod === undefined) {
      mod = require(path.join(root, file));
      modules.set(file, mod);
    }
    return mod;
  }

  // Work handed to `waitUntil` (Next's `after()`, a revalidation behind a stale answer) goes on
  // after the response. Under forkrail's runtime it is also handed to the runtime's own
  // (`forkrail.waitUntil`, shim.mjs), which keeps the worker's invocation open until it settles;
  // a failure is reported, never an unhandled rejection.
  function waitUntil(promise) {
    const settled = Promise.resolve(promise).catch((error) => log.error('waitUntil:', error));
    background.add(settled);
    settled.finally(() => background.delete(settled));
    const runtime = globalThis[Symbol.for('forkrail.waitUntil')];
    if (typeof runtime === 'function') runtime(settled);
  }

  function baseMeta(state) {
    return {
      relativeProjectDir: '.',
      distDir,
      minimalMode: false,
      hostname: state.hostname,
      initURL: state.initURL,
      initProtocol: state.proto,
      revalidate,
      render404: (req, res, parsedUrl, setHeaders) => render404(state, req, res, setHeaders !== false),
    };
  }

  // --- middleware ---------------------------------------------------------------------------

  async function invokeMiddleware(state, ctx) {
    const mw = manifest.middleware;
    const { req } = state;
    const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
    // The middleware may read the body; the page reads it after. Next's own server does the
    // same: one copy streamed to the middleware, one kept for the handler.
    if (hasBody) state.tee = teeBody(req);
    const requestMeta = { relativeProjectDir: '.', distDir, initURL: state.initURL, initProtocol: state.proto, hostname: state.hostname };
    let response;
    try {
      if (mw.runtime === 'edge') {
        response = await edge.run(mw, { url: ctx.url.href, method: req.method, headers: toNodeHeaders(ctx.headers), body: state.tee?.copy, page: {}, waitUntil, requestMeta });
      } else {
        const mod = await load(mw.file);
        const handler = mod.handler ?? mod.default?.handler;
        if (typeof handler !== 'function') throw new Error(`the middleware entry ${mw.file} exports no handler`);
        const body = state.tee ? Readable.toWeb(state.tee.copy) : undefined;
        response = await handler(new Request(ctx.url, { method: req.method, headers: ctx.headers, body, duplex: body ? 'half' : undefined }), { waitUntil, requestMeta });
      }
    } catch (error) {
      // A middleware that throws is a 500, never "the middleware let it through": it may be the
      // app's authentication.
      state.middlewareError = error;
      return { bodySent: true };
    } finally {
      if (state.tee) await state.tee.settle();
    }
    state.middlewareResponse = response;
    if (response.headers.has('x-middleware-rewrite') && response.status !== 200) state.rewriteStatus = response.status;
    const headers = new Headers(ctx.headers);
    const result = routing.responseToMiddlewareResult(response, headers, ctx.url);
    state.requestHeaders = headers;
    state.rewriteTo = result.rewrite ?? null;
    return result;
  }

  // --- one request --------------------------------------------------------------------------

  async function handle(req, res) {
    const state = { req, res };
    if (manifest.compress !== false) compress(req, res);
    try {
      await route(state);
    } catch (error) {
      await fail(state, error);
    }
  }

  async function route(state) {
    const { req, res } = state;
    const { proto, host, origin } = publicOrigin(req.headers);
    state.proto = proto;
    state.origin = origin;
    state.hostname = host.replace(/:\d+$/, '');
    const raw = req.url || '/';
    if (!raw.startsWith('/')) return plain(res, 400, 'Bad Request');
    state.initURL = `${origin}${raw}`;

    const slashes = repeatedSlashes(raw);
    if (slashes) return redirect(state, new URL(slashes, origin), 308);

    let url;
    try {
      url = new URL(raw, origin);
    } catch {
      return plain(res, 400, 'Bad Request');
    }
    for (const rule of priorityRedirects) {
      const match = rule.regex.exec(url.pathname);
      if (match) {
        const to = rule.to.replace(/\$(\d+)/g, (_, n) => match[Number(n)] ?? '');
        // The query goes along, written again the way Next's server writes it (`/` as `%2F`).
        const query = url.search.length > 1 ? `?${querystring.stringify(querystring.parse(url.search.slice(1)))}` : '';
        return redirect(state, new URL(`${to}${query}`, origin), rule.status);
      }
    }
    state.url = url;
    try {
      decodeURIComponent(url.pathname);
    } catch {
      // A path that isn't valid percent-encoding is no request for a page. (Next's own server
      // answers these unevenly: its 404 page for `/%E0%A4%A`, a bare 500 for the same thing in
      // a route's param.)
      return plain(res, 400, 'Bad Request');
    }
    const isData = isDataPath(url.pathname, basePath, buildId);
    untrusted(req.headers, false);
    // `x-nextjs-data` says "this is a data request" to a middleware and, behind one, to the
    // page (Next's own server sets it itself, and only when the build has a middleware).
    if (isData && manifest.middleware) req.headers['x-nextjs-data'] = '1';
    state.isData = isData;

    // `/page.rsc` and segment prefetch paths are the page with the headers Next's client sends.
    const rsc = rscTarget(url.pathname, manifest.routing.rsc);
    if (rsc && !findOutput(manifest, url.pathname)?.entry?.type?.startsWith('PAGES')) {
      Object.assign(req.headers, rsc.headers);
      url.pathname = rsc.pathname === '/' && basePath ? basePath : rsc.pathname;
      state.rscMeta = { isRSCRequest: true, ...(rsc.segment !== null ? { isPrefetchRSCRequest: true, segmentPrefetchRSCRequest: rsc.segment } : {}) };
    }
    url.search = stripInternalQuery(url.search);
    if (i18n) state.locale = localeOf(isData ? `${basePath}${dataPagePath(url.pathname, basePath, buildId)}` : url.pathname).locale ?? i18n.defaultLocale;
    // What the handler sees as the request's URL: the visitor's own.
    state.publicUrl = `${url.pathname}${url.search}`;
    const isRsc = req.headers.rsc === '1';

    // Behind a middleware, a Pages Router data request (`/_next/data/<id>/x.json`) is routed as
    // its page (`/x`, or `/x/` with `trailingSlash`), which is the URL Next's own server shows
    // the middleware; the handler still gets the data URL and answers JSON.
    let routed = url;
    if (isData && manifest.routing.shouldNormalizeNextData) {
      routed = new URL(url);
      const page = dataPagePath(url.pathname, basePath, buildId);
      routed.pathname = `${basePath}${page === '/' ? '' : page}${manifest.trailingSlash && page !== '/' ? '/' : ''}` || '/';
    }
    // The build names a prerendered path as it reads (`/sticks & stones`); the request names it
    // encoded (`/sticks%20%26%20stones`). Routing compares names as given, the middleware's
    // matchers too, so such a request is routed in its plainest spelling: `/%6Dw/x` as `/mw/x`,
    // or a matcher for `/mw/:path*` would never see the page it lands on. What a URL can't hold
    // as it reads (a space) stays encoded: that spelling is added to what routing may land on,
    // and remembered as that path's.
    let names = pathnames;
    let spelled = null;
    if (routed.pathname.includes('%')) {
      const decoded = decodedPath(routed.pathname);
      if (decoded && decoded !== routed.pathname && known.has(decoded)) {
        routed = new URL(routed);
        routed.pathname = decoded;
        if (routed.pathname !== decoded) {
          names = [...pathnames, routed.pathname];
          spelled = { as: routed.pathname, is: decoded };
        }
      }
    }
    const output = (name) => findOutput(manifest, spelled && name === spelled.as ? spelled.is : (urlSpellings.get(name) ?? name));
    const resolve = (invokeMiddleware) =>
      routing.resolveRoutes({
        url: routed,
        buildId,
        basePath,
        i18n: i18n ?? undefined,
        headers: toWebHeaders(req.headers),
        requestBody: emptyBody(),
        pathnames: names,
        routes: manifest.routing,
        invokeMiddleware,
      });
    let result;
    let asked = false;
    const noted = async () => {
      asked = true;
      return {};
    };
    result = await resolve(noted);
    // With locales, the home page is routed as `/<default locale>/`, which Next's own
    // trailing-slash rule then "redirects" to `/<locale>`: an artefact of the prefixing, not a
    // redirect anyone asked for. Routed again as the locale's home page.
    if (i18n && !result.redirect && result.status === 308 && routed.pathname === (basePath || '/')) {
      const to = result.resolvedHeaders?.get('location');
      const target = to ? new URL(to, routed) : null;
      if (target && localeOf(target.pathname).locale && localeOf(target.pathname).rest === (basePath || '/')) {
        routed = new URL(routed);
        routed.pathname = target.pathname;
        asked = false;
        result = await resolve(noted);
      }
    }
    // Priming's own request for Next's error page (`prime`) never runs the app's middleware.
    if (manifest.middleware && !req[PRIMING]) {
      // A config redirect answers before the middleware runs, as under Next's own server.
      // `resolveRoutes` asks the middleware first and applies the redirect after, so it is asked
      // once with a middleware that only notes it was called: if routing then ends on a
      // redirect that no middleware made, that is the answer. Otherwise the real pass.
      const early = !result.redirect && !result.resolvedPathname && REDIRECTS.has(result.status) && result.resolvedHeaders?.has('location');
      if (asked && !early) result = await resolve((ctx) => invokeMiddleware(state, ctx));
    }
    if (state.middlewareError) throw state.middlewareError;
    if (res.destroyed) return;

    // The middleware's verdict on the request's own headers (`NextResponse.next({ request })`):
    // the whole set replaces the request's, so a header it took off stays off.
    if (state.requestHeaders) {
      const replaced = toNodeHeaders(state.requestHeaders);
      for (const name of Object.keys(req.headers)) delete req.headers[name];
      Object.assign(req.headers, replaced);
    }

    if (result.middlewareResponded) return sendMiddlewareResponse(state);

    apply(res, result.resolvedHeaders);

    if (result.redirect) return redirect(state, result.redirect.url, result.redirect.status);
    const located = result.resolvedHeaders?.get('location');
    if (located && REDIRECTS.has(result.status)) return redirect(state, new URL(located, url), result.status);

    // With locales, the build's pattern for a dynamic API route expects a locale in front
    // (`/<locale>/api/blog/[slug]`), and API paths never get one: routing finds nothing for
    // `/api/blog/first`. Next's own server matches API routes without locales, so the same is
    // done here before anything further down (a fallback rewrite, the 404) takes the request.
    if (i18n && !output(result.resolvedPathname)) {
      // The path asked for, or the one a middleware rewrote to (which does carry a locale).
      const rewritten = state.rewriteTo && state.rewriteTo.origin === origin ? state.rewriteTo : null;
      const wanted = localeOf((rewritten ?? url).pathname).rest;
      if (wanted.startsWith(`${basePath}/api/`)) {
        const api = apiRoutes.map((route) => ({ ...route, params: paramsFromPath(route.pathname, wanted) })).find((route) => route.params);
        if (api) {
          const query = rewritten ? handlerQuery({ ...Object.fromEntries(url.searchParams), ...Object.fromEntries(rewritten.searchParams) }) : undefined;
          return invoke(state, { kind: 'function', pathname: api.pathname, entry: api.entry }, { params: api.params, query });
        }
      }
    }

    if (result.externalRewrite) {
      const target = result.externalRewrite;
      // A config rewrite to another origin takes the visitor's query along (under the
      // destination's own keys); a middleware's rewrite names its whole target itself.
      if (!state.middlewareResponse?.headers.has('x-middleware-rewrite')) {
        for (const [key, value] of url.searchParams) if (!target.searchParams.has(key)) target.searchParams.append(key, value);
      }
      return proxy(state, target);
    }

    if (result.status && !result.resolvedPathname) {
      // A routing rule that ends the request with a status and no destination.
      res.statusCode = result.status;
      return res.end();
    }

    // An image to optimize: the router's on forkrail, this server's where it has Next's optimizer.
    if (result.resolvedPathname === imagePath || result.resolvedPathname === `${imagePath}/`) {
      if (await images.serve(state)) return;
      return plain(res, 404, 'Not Found');
    }

    // Where routing landed: the exact path first (a page named `/ssr-page` answers
    // `/ssr-page`, and a rewrite to it, even when a `/[id]` pattern also fits), then the
    // pattern routing chose.
    let found = output(result.invocationTarget?.pathname) ?? output(result.resolvedPathname);
    // With locales, a middleware's rewrite carries one even for what has none (`/en/_next/static/x.js`,
    // a file in `public/`): looked up again without it, as Next's own router does.
    if (!found && i18n) {
      const to = state.rewriteTo && state.rewriteTo.origin === origin ? state.rewriteTo.pathname : routed.pathname;
      if (localeOf(to).locale) found = output(localeOf(to).rest);
      // A config rewrite under locales carries one into its target too (`/files/:path*` to
      // `/:path*` lands on `/en/dot.png`): a file there is the one without it, as Next's
      // filesystem check finds a `public/` file. Only a file: a page keeps its locale.
      for (const target of [result.invocationTarget?.pathname, result.resolvedPathname]) {
        if (found || !target || !localeOf(target).locale) continue;
        const file = output(localeOf(target).rest);
        if (file?.kind === 'static') found = file;
      }
    }
    // Its params are read off the request's own (encoded) spelling.
    if (found?.prerendered && spelled?.is === found.prerendered) found.prerendered = spelled.as;
    if (!found) {
      // A hashed asset that isn't there: a plain 404, never the app's not-found page.
      if (url.pathname.startsWith(`${basePath}/_next/static/`)) {
        res.setHeader('cache-control', NO_STORE);
        return plain(res, 404, 'Not Found');
      }
      return notFound(state);
    }

    if (isRsc && result.invocationTarget) {
      // A rewrite's target, for the client router: it asked for one path and got another's
      // payload (Next's own router-server sets these for config rewrites, the middleware
      // adapter for a middleware's).
      const target = result.invocationTarget.pathname;
      if (target !== url.pathname && !isDataPath(target, basePath, buildId)) res.setHeader('x-nextjs-rewritten-path', target);
    }

    // `/404` and `/500` asked for by name answer with their own status, as under Next's server.
    const named = localeOf(found.pathname).rest.slice(basePath.length);
    const own = named === '/404' || named === '/_not-found' ? 404 : named === '/500' ? 500 : undefined;
    if (own && !result.status) result.status = own;
    if (process.env.FORKRAIL_NEXT_TRACE)
      log.error(
        'trace',
        req.method,
        raw,
        '->',
        JSON.stringify({
          routed: routed.pathname,
          resolved: result.resolvedPathname,
          target: result.invocationTarget,
          matches: result.routeMatches,
          query: result.resolvedQuery,
          found: found.pathname,
          prerendered: found.prerendered,
          status: result.status,
        }),
      );
    if (found.kind === 'static') return staticFile(state, found, result.status);

    if (isRsc) {
      // The request's `_rsc` must be the hash of its RSC headers (routing.mjs): a mismatch is
      // sent to the right URL, as Next's own server does. The URL the handler gets keeps it
      // (Next's server leaves it there too, and a Pages Router page can read it in
      // `resolvedUrl`); the query a handler is told does not have it.
      if (manifest.validateRsc !== false && (req.method === 'GET' || req.method === 'HEAD')) {
        const { hash, legacy } = await rscHash(req.headers);
        const given = rscParam(state.url.search);
        if (given !== hash && (given === null || given !== legacy)) {
          const init = new URL(state.initURL);
          res.statusCode = 307;
          res.setHeader('location', `${init.pathname}${withRsc(init.search, hash)}`);
          return res.end();
        }
      }
      if (result.resolvedQuery) delete result.resolvedQuery._rsc;
    }

    state.result = result;
    // The locale routing ended on (a middleware may rewrite `/about` to `/fr/about`).
    const targetLocale = localeOf(result.invocationTarget?.pathname ?? '').locale ?? localeOf(result.resolvedPathname ?? '').locale;
    if (targetLocale) state.locale = targetLocale;
    // A config rewrite's query is worked out by the handler itself from the visitor's URL, as
    // under Next's server (`:path*` becomes an array there). A middleware's rewrite is known
    // only here. And a config rewrite onto an App Router page with params (`/:path*` to
    // `/[domain]/[...section]`): applying the rewrite again, the handler adds the route's params
    // to the query, so the page's `searchParams` would show `domain` and `section`, which
    // neither Vercel nor `next start` do. There routing's own query is given: the visitor's and
    // the destination's, without the route's (`handlerQuery`). The Pages Router's `query` holds
    // a route's params by design, so it stays the handler's.
    const middlewareRewrite = state.middlewareResponse?.headers.has('x-middleware-rewrite');
    const app = found.entry.type === 'APP_PAGE' || found.entry.type === 'APP_ROUTE';
    const rewroteOntoParams = app && found.pathname.includes('[') && result.invocationTarget && result.invocationTarget.pathname !== routed.pathname;
    return invoke(state, found, {
      status: state.rewriteStatus ?? result.status,
      query: middlewareRewrite || rewroteOntoParams ? handlerQuery(result.resolvedQuery) : undefined,
      params: paramsFor(found, result),
    });
  }

  // The locale a path starts with (`/fr/about` → `fr`), and the path without it. Null where the
  // app has no locales or the path names none.
  function localeOf(pathname) {
    if (!i18n) return { locale: null, rest: pathname };
    const inner = pathname.startsWith(basePath) ? pathname.slice(basePath.length) : pathname;
    const first = inner.split('/', 2)[1] ?? '';
    const locale = i18n.locales.find((one) => one.toLowerCase() === first.toLowerCase());
    return locale ? { locale, rest: `${basePath}${inner.slice(first.length + 1) || '/'}` } : { locale: null, rest: pathname };
  }

  // A route's params. Routing's own matches give them for a plain match of the route's pattern.
  // Two cases give fewer: a prerendered path matched as itself (no matches at all), and Next
  // 16.4's partly filled shells (`/en/default/products/[slug]` for `/[locale]/[channel]/products/
  // [slug]`), whose pattern has the filled parts as plain text. So they are also read off the
  // concrete path routing ended on, and the fuller set is used. Null: not decodable (a 400).
  function paramsFor(found, result) {
    const matched = routeParams(found.pathname, result.routeMatches);
    const literal = (name) => (name && !name.includes('[') ? name : undefined);
    const concrete = literal(result.invocationTarget?.pathname) ?? literal(found.prerendered);
    const read = concrete ? paramsOf(found.pathname, concrete) : undefined;
    if (matched === null || read === null) return null;
    return read && Object.keys(read).length >= Object.keys(matched ?? {}).length ? read : matched;
  }

  // Params read off a concrete path for a route (`/_next/data/<id>/docs/a.json` and
  // `/docs/a.rsc` are `/docs/a`; a localized page is one route under each locale).
  function paramsOf(route, pathname) {
    const page = (name) => {
      const path = isDataPath(name, basePath, buildId) ? `${basePath}${dataPagePath(name, basePath, buildId)}` : name;
      return rscTarget(path, manifest.routing.rsc)?.pathname ?? path;
    };
    const template = page(route);
    const concrete = page(pathname);
    // With locales, a localized path is the route under its locale (`/en/hello` is `hello` of
    // `/[...slug]`, and of `/en/[...slug]`): both read without their locale. Read as they are, a
    // catch-all at the top would take the locale as its first part (`['en', 'hello']`), which
    // a `fallback: false` page then answers with a 404.
    const t = localeOf(template);
    const c = localeOf(concrete);
    if (t.locale || c.locale) return paramsFromPath(t.rest, c.rest);
    return paramsFromPath(template, concrete);
  }

  // Calls a function's entrypoint. `req.url` stays the visitor's URL (so the page's own idea of
  // its address, its canonical links and `usePathname` are the public ones, as under `next
  // start`); where routing ended up is told through `requestMeta`.
  async function invoke(state, found, { status, query, params, meta } = {}) {
    const { req, res } = state;
    if (params === null) return plain(res, 400, 'Bad Request');
    const entry = found.entry;
    if (state.tee) state.tee.restore();
    if (status) res.statusCode = status;
    // A middleware's rewrite on this origin: the URL keeps the visitor's path and takes the
    // rewrite's query, as Vercel's proxy hands it to the function ("Deployed proxies include query
    // values added while resolving rewrites in the URL passed to the function": Next's
    // middleware-rewrites test); `next start` keeps the visitor's whole URL. The rewrite names
    // its whole query (Next's router takes the target's), so a key the middleware left out is
    // gone here too.
    const rewritten = state.rewriteTo && state.rewriteTo.origin === state.origin ? state.rewriteTo : null;
    req.url = rewritten ? `${state.publicUrl.split('?')[0]}${stripInternalQuery(rewritten.search)}` : state.publicUrl;
    if (!state.errorPage && before(state, found)) return;
    const requestMeta = {
      ...baseMeta(state),
      ...state.rscMeta,
      ...(state.isData ? { isNextDataReq: true } : {}),
      ...(state.locale ? { locale: state.locale } : {}),
      ...(query ? { query } : {}),
      ...(params ? { params } : {}),
      ...meta,
    };
    if (entry.runtime === 'edge') {
      await requestCache(req);
      return invokeEdge(state, entry, requestMeta);
    }
    const mod = await load(entry.file);
    const handler = mod.handler ?? mod.default?.handler;
    if (typeof handler !== 'function') throw new Error(`the entry ${entry.file} exports no handler`);
    if (entry.type === 'PAGES_API') requestMeta.incrementalCache = await requestCache(req, mod.routeModule);
    try {
      await handler(req, res, { waitUntil, requestMeta });
    } catch (error) {
      if (isNoFallback(error)) return notFound(state);
      throw error;
    }
  }

  // Next's own server makes an incremental cache for every request before it calls any handler
  // (base-server.ts, `handleRequest`) and leaves it in `globalThis.__incrementalCache` too. Page
  // and App Router handlers make their own; a Pages Router API route makes none, and an Edge
  // function gets the one that global holds (Next's Edge adapter otherwise makes a throwaway one
  // per request). There `unstable_cache`, `revalidateTag` and the fetch cache read it: on a worker
  // whose requests so far were only those, `unstable_cache` threw (a 500) and an Edge route's
  // `revalidateTag` changed nothing. So they get one made as Next makes it, by its own route
  // module: the entry's (bundled into it, so no file more in the package), or for an Edge
  // function any Node entry's. Undefined when the deployment has no Node entry at all, or the
  // cache couldn't be made: the handler then runs as it did without one (reported once).
  let cacheInputs = null;
  let cacheFailed = false;
  async function requestCache(req, routeModule) {
    try {
      routeModule ??= await anyRouteModule();
      if (typeof routeModule?.getIncrementalCache !== 'function') return undefined;
      cacheInputs ??= readCacheInputs();
      const { nextConfig, prerenderManifest, previewFirst } = cacheInputs;
      const cache = previewFirst
        ? await routeModule.getIncrementalCache(req, nextConfig, prerenderManifest.preview, prerenderManifest, false)
        : await routeModule.getIncrementalCache(req, nextConfig, prerenderManifest, false);
      cache?.resetRequestCache?.();
      return cache;
    } catch (error) {
      if (!cacheFailed) log.error("forkrail next: no incremental cache for a Pages API route or an Edge function (Next's own server would have made one):", error);
      cacheFailed = true;
      return undefined;
    }
  }

  function readCacheInputs() {
    const read = (name) => JSON.parse(fs.readFileSync(path.join(distDir, name), 'utf8'));
    let prerenderManifest;
    try {
      prerenderManifest = read('prerender-manifest.json');
    } catch {
      prerenderManifest = { version: -1, routes: {}, dynamicRoutes: {}, notFoundRoutes: [], preview: undefined };
    }
    // 16.4 takes the preview keys before the manifest; 16.2 and 16.3 take (req, config, manifest,
    // minimal mode).
    const [major, minor] = String(manifest.nextVersion).split('.').map(Number);
    return { nextConfig: read('required-server-files.json').config, prerenderManifest, previewFirst: major > 16 || (major === 16 && minor >= 4) };
  }

  async function anyRouteModule() {
    for (const mod of modules.values()) if (typeof mod?.routeModule?.getIncrementalCache === 'function') return mod.routeModule;
    const node = Object.values(manifest.functions).find((entry) => entry.runtime !== 'edge' && entry.file);
    return node ? (await load(node.file)).routeModule : undefined;
  }

  // What Next's server settles before it calls a page's handler (base-server.ts,
  // `renderToResponseWithComponents`). True when the request was answered here.
  function before(state, found) {
    const { req, res } = state;
    const { type, ssg } = found.entry;
    const app = type === 'APP_PAGE' || type === 'APP_ROUTE';
    // What an App Router answer varies by, for caches; `next-url` only where an intercepting
    // route could answer, and otherwise not even shown to the page.
    if (app || req.headers.rsc === '1') {
      const intercepted = app && interceptable.some((regex) => regex.test(state.url.pathname));
      res.appendHeader('vary', intercepted ? `${RSC_VARY}, next-url` : RSC_VARY);
      if (!intercepted) delete req.headers['next-url'];
    }
    // The Pages Router client's speculative data request for a page that would have to run
    // (not a prerendered one): told to skip it rather than have the page run for a hover.
    if (!ssg && req.headers['x-middleware-prefetch'] && type !== 'PAGES_API' && type !== 'APP_ROUTE') {
      res.setHeader('x-matched-path', localeOf(found.pathname).rest.slice(basePath.length) || '/');
      res.setHeader('x-middleware-skip', '1');
      res.setHeader('cache-control', NO_STORE);
      res.end('{}');
      return true;
    }
    const method = req.method;
    if (method !== 'GET' && method !== 'HEAD' && ssg && !possibleAction(req) && typeof req.headers['next-resume'] !== 'string') {
      res.statusCode = 405;
      res.setHeader('allow', ['GET', 'HEAD']);
      res.end('Method Not Allowed');
      return true;
    }
    if (method === 'OPTIONS' && type !== 'APP_ROUTE' && type !== 'PAGES_API') {
      res.statusCode = 400;
      res.end();
      return true;
    }
    return false;
  }

  // An Edge-runtime page or route, run in Next's own Edge sandbox: a Web response comes back
  // and is streamed to the client. It is told what Next's server tells it: the visitor's URL
  // with the resolved query and the route's params in its query string.
  async function invokeEdge(state, entry, requestMeta) {
    const { req, res } = state;
    const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
    const abort = new AbortController();
    res.once('close', () => {
      if (!res.writableFinished) abort.abort();
    });
    const url = new URL(state.initURL);
    // Edge's adapter reads params from `nxtP*` keys: a visitor's own are taken off first (G2).
    const search = new URLSearchParams(stripInternalQuery(url.search));
    for (const [key, value] of Object.entries({ ...requestMeta.query, ...requestMeta.params })) {
      search.delete(key);
      for (const one of Array.isArray(value) ? value : [value]) search.append(key, one);
    }
    url.search = search.toString();
    const response = await edge.run(entry, {
      url: url.href,
      method: req.method,
      headers: req.headers,
      body: hasBody ? req : undefined,
      page: { name: null, ...(requestMeta.params ? { params: requestMeta.params } : {}) },
      signal: abort.signal,
      waitUntil,
      requestMeta,
    });
    await sendWeb(res, response, { keepStatus: res.statusCode !== 200 });
  }

  // --- answers this server writes itself ----------------------------------------------------

  function redirect(state, target, status) {
    const { res } = state;
    const to = location(target, state.origin);
    res.statusCode = status;
    res.setHeader('location', to);
    // Next's own server adds this for a permanent redirect (old browsers' caches).
    if (status === 308) res.setHeader('refresh', `0;url=${to}`);
    res.end(to);
  }

  async function sendMiddlewareResponse(state) {
    const { res } = state;
    const response = state.middlewareResponse;
    if (!response) return res.end();
    for (const [name, value] of response.headers) {
      if (name === 'set-cookie' || name === 'content-length' || isMiddlewareProtocol(name)) continue;
      res.setHeader(name, value);
    }
    const cookies = response.headers.getSetCookie();
    if (cookies.length) res.setHeader('set-cookie', cookies);
    res.statusCode = response.status;
    if (!response.body) return res.end();
    await pipeline(Readable.fromWeb(response.body), res).catch(() => {});
  }

  function staticFile(state, found, status) {
    const { req, res } = state;
    if (req.method !== 'GET' && req.method !== 'HEAD') {
      res.setHeader('allow', 'GET, HEAD');
      return plain(res, 405, 'Method Not Allowed');
    }
    if (status) res.statusCode = status;
    // A file the router serves and the package doesn't hold (`null` in the manifest): known to
    // routing, so no rewrite shadows it, and reached here only through one (`/docs/:path*` to
    // `/:path*`), since the router answers a request for the file's own path itself. The router
    // is told to answer with it (`x-forkrail-static`, `forkrail_common::next::STATIC_HEADER`),
    // with what routing set on the way; the answer has nothing else.
    if (found.file === null) {
      res.setHeader(STATIC_HEADER, encodeURI(found.pathname));
      return res.end();
    }
    if (req.headers.rsc === '1') res.appendHeader('vary', RSC_VARY);
    // Hashed assets never change (routing sets this itself when it matched the file by its own
    // path). A service worker keeps its name across builds, so it is revalidated on every use, as
    // Next's own server serves it.
    if (found.pathname.startsWith(`${basePath}/_next/static/`) && !res.hasHeader('cache-control')) {
      const worker = found.pathname.startsWith(`${basePath}/_next/static/service-worker/`);
      res.setHeader('cache-control', worker ? 'public, max-age=0, must-revalidate' : 'public, max-age=31536000, immutable');
    }
    // Behind a middleware, the Pages Router client asks which page its data request landed on.
    if (req.headers['x-nextjs-data'] && found.file.endsWith('.html')) {
      const page = found.pathname.slice(basePath.length).replace(/\/index$/, '') || '/';
      res.setHeader('x-nextjs-matched-path', page);
    }
    return serveStatic(req, res, path.join(root, found.file), found.pathname, { poweredBy: manifest.poweredByHeader !== false });
  }

  // Another origin answers this request (a rewrite to an absolute URL, from the config or a
  // middleware). Streamed both ways; the visitor's own headers go along, as Next's proxy sends.
  function proxy(state, target) {
    const { req, res } = state;
    if (state.tee) state.tee.restore();
    return new Promise((resolve) => {
      const headers = {};
      for (const [name, value] of Object.entries(req.headers)) {
        if (!isHopByHop(name) && name !== 'host' && !name.startsWith('x-forkrail-')) headers[name] = value;
      }
      headers['x-forwarded-host'] = req.headers.host ?? state.hostname;
      headers['x-forwarded-proto'] = state.proto;
      // `node:https` (and TLS with it) is loaded when a rewrite first leaves for an https origin.
      const client = target.protocol === 'https:' ? builtin('node:https') : http;
      const upstream = client.request(target, { method: req.method, headers, timeout: 30_000 }, (answer) => {
        res.statusCode = answer.statusCode ?? 502;
        for (const [name, value] of Object.entries(answer.headers)) {
          if (!isHopByHop(name) && value !== undefined) res.setHeader(name, value);
        }
        pipeline(answer, res)
          .catch(() => {})
          .finally(resolve);
      });
      upstream.on('timeout', () => upstream.destroy(new Error('the rewrite target did not answer in 30 s')));
      upstream.on('error', (error) => {
        log.error(`rewrite to ${target.origin} failed:`, error.message);
        if (!res.headersSent) plain(res, 500, 'Internal Server Error');
        else res.destroy();
        resolve();
      });
      res.once('close', () => upstream.destroy());
      if (req.method === 'GET' || req.method === 'HEAD') upstream.end();
      else req.pipe(upstream);
    });
  }

  // --- 404 and errors -----------------------------------------------------------------------

  // The app's not-found page: the App Router's (`app/not-found`, or Next's default), else the
  // Pages Router's `/404`, else its `/_error`. The same order as Next's own server.
  function errorOutput(names) {
    for (const name of names) {
      const found = findOutput(manifest, `${basePath}${name}`) ?? findOutput(manifest, name);
      // A static error page the package doesn't hold (`null`) can't be sent from here.
      if (found && !(found.kind === 'static' && found.file === null)) return found;
    }
    return null;
  }

  async function notFound(state) {
    const { req, res } = state;
    if (state.rendering404) return plain(res, 404, 'This page could not be found');
    // A Pages Router data request for a page that isn't there, behind a middleware: the client
    // router is told to do a full navigation rather than given an error page.
    if (manifest.middleware && req.headers['x-nextjs-data'] && state.isData) {
      const page = dataPagePath(state.url.pathname, basePath, buildId);
      res.setHeader('x-nextjs-matched-path', page);
      res.setHeader('content-type', 'application/json');
      res.statusCode = 200;
      return res.end('{}');
    }
    return render404(state, req, res);
  }

  // `own` is false when a page's handler asked for the 404 itself (`notFound()` in
  // `getStaticProps`): it has set its own caching headers.
  async function render404(state, req, res, own = true) {
    state.rendering404 = true;
    state.errorPage = true;
    res.statusCode = 404;
    if (own) res.setHeader('cache-control', NO_STORE);
    if ((req.method === 'GET' || req.method === 'HEAD') && req.headers.rsc !== '1' && !state.isData && NON_HTML_DESTS.has(req.headers['sec-fetch-dest'])) {
      res.setHeader('cache-control', NO_STORE);
      return plain(res, 404, 'Not Found');
    }
    const locale = state.locale ? [`/${state.locale}/404`] : [];
    const found = errorOutput(['/_not-found', ...locale, '/404', '/_error']);
    if (!found) return plain(res, 404, 'This page could not be found');
    if (found.kind === 'static') return serveStatic(req, res, path.join(root, found.file), found.pathname, { status: 404, poweredBy: manifest.poweredByHeader !== false });
    return invoke(state, found, { status: 404 });
  }

  async function fail(state, error) {
    const { req, res } = state;
    log.error(error);
    if (res.headersSent || res.destroyed) {
      res.destroy();
      return;
    }
    if (state.rendering500) return plain(res, 500, 'Internal Server Error');
    state.rendering500 = true;
    state.errorPage = true;
    try {
      for (const name of res.getHeaderNames()) if (name !== 'x-forkrail-reply-to') res.removeHeader(name);
      res.statusCode = 500;
      res.setHeader('cache-control', NO_STORE);
      const found = errorOutput(['/500', '/_error']);
      if (!found) return plain(res, 500, 'Internal Server Error');
      if (found.kind === 'static') return await serveStatic(req, res, path.join(root, found.file), found.pathname, { status: 500, poweredBy: manifest.poweredByHeader !== false });
      return await invoke(state, found, { status: 500, meta: { invokeError: error } });
    } catch (again) {
      log.error(again);
      if (!res.headersSent) plain(res, 500, 'Internal Server Error');
      else res.destroy();
    }
  }

  // --- on-demand revalidation ---------------------------------------------------------------

  // `res.revalidate('/path')` (Pages Router) and Next's own internal revalidations: the page is
  // rendered again by a request of our own through the same pipeline, with the build's preview
  // id as proof (`x-prerender-revalidate`), and its answer thrown away. No network hop.
  async function revalidate({ urlPath, headers, opts }) {
    const { req, res, done } = mockExchange(urlPath, headers);
    await handle(req, res);
    await done;
    const cache = res.getHeader('x-nextjs-cache');
    if (cache !== 'REVALIDATED' && res.statusCode !== 200 && !(res.statusCode === 404 && opts?.unstable_onlyGenerated)) {
      throw new Error(`Invalid response ${res.statusCode}`);
    }
  }

  // --- the start ------------------------------------------------------------------------------
  //
  // On Lambda the start runs on a whole core whatever the function's memory size, a request on
  // the size's share of one, and most of a first request is loading Next's page runtime and the
  // page's code and running its first render. What can be done before a visitor asks is done in
  // the start: `prime` under forkrail's runtime, `preload` where there is none.

  // The entrypoints to load, in order: first what every request of a kind needs, one entry per
  // kind (the middleware, the App Router's not-found page for Next's page runtime and the root
  // layout, one route handler, one Pages Router page for `_app` and `_document`, one API route;
  // measured on Lambda, N8: most of what a first request loads, for about half the cost of
  // loading everything), then the others in the build's order. `always` of them are the first.
  function entryFiles(lead = null) {
    const node = Object.entries(manifest.functions).filter(([, entry]) => entry.runtime !== 'edge');
    const first = (type, prefer) => (node.find(([pathname, entry]) => entry.type === type && prefer?.(pathname)) ?? node.find(([, entry]) => entry.type === type))?.[1];
    const shared = [
      lead,
      manifest.middleware?.runtime === 'edge' ? null : manifest.middleware,
      first('APP_PAGE', (pathname) => pathname === `${basePath}/_not-found`),
      first('APP_ROUTE'),
      first('PAGES', (pathname) => pathname === `${basePath}/_error` || pathname === `${basePath}/404`),
      first('PAGES_API'),
    ].filter(Boolean);
    const always = new Set(shared.map((entry) => entry.file)).size;
    const files = [...new Set([...shared, ...node.map(([, entry]) => entry)].map((entry) => entry.file))];
    return { files, always };
  }

  // A module that fails to load is left for its first request to meet and report.
  function tryLoad(file) {
    try {
      load(file);
      return true;
    } catch (error) {
      modules.delete(file);
      log.error(`preload: ${file}:`, error?.message ?? error);
      return false;
    }
  }

  // Without forkrail's runtime (or with its priming turned off): the shared entrypoints, then
  // the others while `budgetMs` lasts, before the server listens.
  function preload({ budgetMs = 0 } = {}) {
    const started = performance.now();
    const { files, always } = entryFiles();
    let loaded = 0;
    for (const [index, file] of files.entries()) {
      if (index >= always && performance.now() - started >= budgetMs) break;
      if (tryLoad(file)) loaded += 1;
    }
    return { loaded, of: files.length, ms: Math.round(performance.now() - started) };
  }

  // Priming, called from the runtime's `forkrail.prime` hook (main.mjs, shim.mjs) once the server
  // listens, before the runtime calls the worker ready; everything stops at `deadline` (ms since
  // the epoch). The same steps as Next's own server gets under forkrail (next-standalone.cjs):
  //   1. the shared entrypoints (`entryFiles`), Next's `/_global-error` page's first;
  //   2. that page rendered once, where the build prerendered it, asked for as an on-demand
  //      revalidation (`x-prerender-revalidate` with the build's preview id): Next's whole App
  //      Router render path runs, and none of the app's request code (the page is Next's own,
  //      with no layout of the app's, and the middleware is skipped for it);
  //   3. the pages the project named (FORKRAIL_PRIME_PATHS), each asked for as a visitor of the
  //      deployment's own URL would, middleware included;
  //   4. the other entrypoints, one at a time, until the deadline or until the process holds
  //      `memoryLimit` bytes (`next start` loads every one at its start).
  // Each answer is thrown away. Returns what was done, for the runtime's start line.
  async function prime({ deadline = Infinity, previewId = null, paths = [], visitor = {}, memoryLimit = 0 } = {}) {
    const started = performance.now();
    const late = () => Date.now() >= deadline - 20;
    const turn = () => new Promise((resolve) => setImmediate(resolve));
    const page = `${basePath}/_global-error`;
    const errorPage = previewId && manifest.prerenders?.[page] ? findOutput(manifest, page) : null;
    const { files, always } = entryFiles(errorPage?.entry);
    const done = {};
    let loaded = 0;
    let index = 0;
    for (; index < always && !late(); index++) {
      if (tryLoad(files[index])) loaded += 1;
      await turn();
    }
    if (errorPage && !late()) done.request = `/_global-error ${await ask(page, { host: 'localhost', 'x-prerender-revalidate': previewId }, true)}`;
    for (const target of paths) {
      if (late()) break;
      done[target] = await ask(target, visitor, false);
    }
    for (; index < files.length && !late() && !(memoryLimit && process.memoryUsage.rss() > memoryLimit); index++) {
      if (tryLoad(files[index])) loaded += 1;
      await turn();
    }
    done.entries = `${loaded}/${files.length}`;
    done.ms = Math.round(performance.now() - started);
    return done;
  }

  // One GET through the whole server with no visitor, its answer thrown away: the status, or
  // `failed`. `own` marks Next's own error page, for which the middleware is skipped.
  async function ask(target, headers, own) {
    const { req, res, done } = mockExchange(target, headers);
    if (own) req[PRIMING] = true;
    try {
      await handle(req, res);
      await done;
      return res.statusCode;
    } catch (error) {
      log.error(`prime: ${target}:`, error?.message ?? error);
      return 'failed';
    }
  }

  const images = createImages({ distDir, require, handle, render404 });

  // Next's handlers find these where its own server would have put them, for the code paths
  // that have no request to carry them (`revalidatePath` from an Edge function).
  const shared = (globalThis[Symbol.for('@next/router-server-methods')] ??= {});
  shared['.'] = { ...shared['.'], revalidate };

  return {
    handle,
    load,
    preload,
    prime,
    revalidate,
    /// Background work still running (for a clean stop, and for tests).
    idle: () => Promise.all([...background]),
  };
}

// --- helpers --------------------------------------------------------------------------------

function plain(res, status, text) {
  res.statusCode = status;
  res.setHeader('content-type', 'text/plain; charset=utf-8');
  res.end(text);
}

function emptyBody() {
  return new ReadableStream({
    start(controller) {
      controller.close();
    },
  });
}

// Routing's headers onto the response: config `headers()`, what a middleware set, the immutable
// header of hashed assets. Set before the handler runs, so the page's own win.
function apply(res, headers) {
  if (!headers) return;
  for (const [name, value] of headers) {
    if (name === 'set-cookie') continue;
    try {
      res.setHeader(name, value);
    } catch {
      // A value that can't be a header (routing substituted something odd): left out.
    }
  }
  const cookies = headers.getSetCookie?.() ?? [];
  if (cookies.length) res.appendHeader('set-cookie', cookies);
}

// A POST that may be a server action (Next's `getIsPossibleServerAction`).
function possibleAction(req) {
  if (req.method !== 'POST') return false;
  const type = req.headers['content-type'] ?? '';
  return typeof req.headers['next-action'] === 'string' || type === 'application/x-www-form-urlencoded' || type.startsWith('multipart/form-data');
}

function isNoFallback(error) {
  return error?.constructor?.name === 'NoFallbackError' || error?.name === 'NoFallbackError';
}

// A Web response written to a Node one, as it comes (no buffering).
async function sendWeb(res, response, { keepStatus = false } = {}) {
  if (!keepStatus) res.statusCode = response.status;
  for (const [name, value] of response.headers) {
    if (name === 'set-cookie') continue;
    res.setHeader(name, value);
  }
  const cookies = response.headers.getSetCookie();
  if (cookies.length) res.appendHeader('set-cookie', cookies);
  if (!response.body) return res.end();
  res.flushHeaders();
  await pipeline(Readable.fromWeb(response.body), res).catch(() => {});
}

// The request body for two readers: `copy` for the middleware, and the request itself again for
// the handler (`restore`). After Next's `getCloneableBody` (server/body-streams.ts, MIT): the
// kept copy's stream state is put onto the request object, which then reads as if untouched.
function teeBody(req) {
  const copy = new PassThrough();
  const kept = new Readable({ read() {} });
  const end = new Promise((resolve) => {
    req.on('data', (chunk) => {
      copy.write(chunk);
      kept.push(chunk);
    });
    req.on('end', () => {
      copy.end();
      kept.push(null);
      resolve();
    });
    req.on('error', (error) => {
      copy.destroy(error);
      kept.destroy(error);
      resolve();
    });
    req.on('close', resolve);
  });
  let restored = false;
  return {
    copy,
    // The body is all here before the handler starts (as under Next's own server, which waits
    // for the cloned body's end): what the middleware didn't read is kept for the handler.
    settle: () => end,
    restore() {
      if (restored) return;
      restored = true;
      for (const key in kept) {
        let value = kept[key];
        if (typeof value === 'function') value = value.bind(req);
        req[key] = value;
      }
    },
  };
}

// A request and response that never touch a socket: for a revalidation of our own. `done`
// settles when the response has ended.
function mockExchange(url, headers) {
  const req = new http.IncomingMessage(null);
  req.method = 'GET';
  req.url = url;
  req.headers = { ...headers };
  req.push(null);
  const res = new http.ServerResponse(req);
  const sink = new Writable({
    write(chunk, encoding, callback) {
      callback();
    },
  });
  res.assignSocket(sink);
  const done = new Promise((resolve) => {
    res.once('finish', resolve);
    res.once('close', resolve);
  });
  return { req, res, done };
}
