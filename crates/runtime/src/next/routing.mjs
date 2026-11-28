// forkrail's Next.js server ("H"): the parts with no Next code in them. Everything here is plain
// data in, plain data out, so it is tested on its own (routing.test.mjs).
//
// What the server does with these, in order, for each request:
//   1. `publicOrigin`      the visitor's scheme and host: every URL the app sees is built on it
//   2. `untrusted`         headers only the platform may set, taken off what the client sent
//   3. `repeatedSlashes`   Next's own 308 for `//` and `\` in a path
//   4. `rscTarget`         `/page.rsc` and `/page.segments/…​.segment.rsc` back to `/page` + headers
//   5. @next/routing's resolveRoutes (server.mjs)
//   6. `routeParams`, `handlerQuery`, `location`: what the entrypoint is told

/// Request headers that carry Next's own routing verdicts between its layers. A client that sends
/// one would be speaking for the platform: `next-resume: 1` makes a page read the request body as
/// trusted postponed state, `x-matched-path` and `x-now-route-matches` choose the page and its
/// params, `x-middleware-*` are what a middleware's answer is made of, and `x-invocation-id`
/// scopes Next's in-memory response cache. Same list as Next's own server filters
/// (`server/lib/server-ipc/utils.ts`), plus the ones only a proxy in front of Next sets.
export const UNTRUSTED = [
  'x-middleware-rewrite',
  'x-middleware-redirect',
  'x-middleware-set-cookie',
  'x-middleware-skip',
  'x-middleware-override-headers',
  'x-middleware-next',
  'x-middleware-refresh',
  // Names the middlewares a request has been through: a forged one skips them (CVE-2025-29927).
  'x-middleware-subrequest',
  'x-middleware-subrequest-id',
  'x-middleware-invoke',
  'x-now-route-matches',
  'x-matched-path',
  'x-nextjs-data',
  'x-next-resume-state-length',
  'next-resume',
  'x-invocation-id',
  'x-invoke-path',
  'x-invoke-query',
  'x-invoke-output',
  'x-invoke-status',
  'x-invoke-error',
];

/// Takes the platform-only headers off a Node request's headers, in place. `x-nextjs-data` is
/// the Pages Router client's own hint; it is kept only on a real data URL (`isData`), where Next
/// reads it. `x-middleware-request-*` are a middleware's header overrides.
export function untrusted(headers, isData) {
  for (const name of Object.keys(headers)) {
    if (name.startsWith('x-middleware-request-') || (UNTRUSTED.includes(name) && !(isData && name === 'x-nextjs-data'))) {
      delete headers[name];
    }
  }
  return headers;
}

/// The visitor's scheme and host, from what the router sends: `x-forwarded-proto` and
/// `x-forwarded-host` (the router writes both itself, whatever the client sent), else `Host`.
/// No port of ours, no localhost. `x-forwarded-host` comes first because Next's own forwarded
/// requests (a server action sent on to the worker that has its page) arrive on our loopback
/// address with the visitor's host kept there.
export function publicOrigin(headers) {
  const forwarded = String(headers['x-forwarded-proto'] ?? '')
    .split(',')[0]
    .trim()
    .toLowerCase();
  const proto = forwarded === 'https' || forwarded === 'http' ? forwarded : 'http';
  const named = [headers['x-forwarded-host'], headers.host].map((value) =>
    String(value ?? '')
      .split(',')[0]
      .trim(),
  );
  const host = named.find((value) => /^[A-Za-z0-9.\-_\[\]:]+$/.test(value)) ?? 'localhost';
  return { proto, host, origin: `${proto}://${host}` };
}

/// Next answers a path with `//` or `\` with a 308 to the cleaned path (base-server's
/// `normalizeRepeatedSlashes`). Returns the target (path and query), or null.
export function repeatedSlashes(rawUrl) {
  const at = rawUrl.indexOf('?');
  const path = at === -1 ? rawUrl : rawUrl.slice(0, at);
  if (!/\\|\/\//.test(path)) return null;
  const cleaned = path.replace(/\\/g, '/').replace(/\/\/+/g, '/');
  return cleaned + (at === -1 ? '' : rawUrl.slice(at));
}

/// A path's RSC form, as a CDN or `output: export` client asks for it:
/// `/page.rsc` (the payload) or `/page.segments/<segment path>.segment.rsc` (one segment's
/// prefetch). Gives the page's own path and the headers that say the same thing, which is how
/// Next's client asks a server (`rsc`, `next-router-prefetch`, `next-router-segment-prefetch`).
/// `/index` is the home page's name in these forms.
export function rscTarget(pathname, rsc) {
  if (!rsc) return null;
  const suffix = rsc.suffix ?? '.rsc';
  const dir = rsc.prefetchSegmentDirSuffix ?? '.segments';
  const segmentSuffix = rsc.prefetchSegmentSuffix ?? '.segment.rsc';
  const page = (path) => (path === '/index' ? '/' : path);
  if (pathname.endsWith(segmentSuffix)) {
    const at = pathname.lastIndexOf(dir + '/');
    if (at > 0) {
      const segment = pathname.slice(at + dir.length, -segmentSuffix.length);
      return { pathname: page(pathname.slice(0, at)), headers: { rsc: '1', 'next-router-prefetch': '1', 'next-router-segment-prefetch': segment }, segment };
    }
  }
  if (pathname.endsWith(suffix) && pathname.length > suffix.length) {
    return { pathname: page(pathname.slice(0, -suffix.length)), headers: { rsc: '1' }, segment: null };
  }
  return null;
}

/// `/_next/data/<build id>/<page>.json`: the Pages Router's data request for a page.
export function isDataPath(pathname, basePath, buildId) {
  return pathname.startsWith(`${basePath}/_next/data/${buildId}/`) && pathname.endsWith('.json');
}

/// The page a data URL is for (`/_next/data/<id>/blog/a.json` → `/blog/a`, `index` → `/`).
export function dataPagePath(pathname, basePath, buildId) {
  const rest = pathname.slice(`${basePath}/_next/data/${buildId}/`.length, -'.json'.length);
  return rest === 'index' ? '/' : `/${rest}`;
}

/// Query keys Next's routing adds for a route's own params (`nxtPslug`, `nxtIslug` for an
/// intercepted one). A handler called directly reads them as the route's params, so a visitor's
/// own must not get there; nor do they belong in what the page sees as its query.
const INTERNAL_QUERY = /^(nxtP|nxtI)/;
export function isInternalQuery(key) {
  return INTERNAL_QUERY.test(key) || key === '__nextLocale' || key === '__nextDefaultLocale' || key === '__nextInferredLocaleFromDefault' || key === '__nextDataReq';
}

/// A query string without the keys above. A key counts by what it decodes to (`%6ExtPslug` is
/// `nxtPslug` to the handler that parses it). Every other pair is kept byte for byte, so `?a+b`
/// and `%7E` reach the app exactly as sent.
export function stripInternalQuery(search) {
  if (!search || search === '?') return search;
  // Nothing that could spell one of the keys, encoded or not: the usual case.
  if (!/nxt|__next|%/i.test(search)) return search;
  const pairs = search.slice(1).split('&');
  const kept = pairs.filter((pair) => {
    let key = pair.split('=', 1)[0].replace(/\+/g, ' ');
    try {
      key = decodeURIComponent(key);
    } catch {
      // Not decodable, so not one of ours to any parser.
    }
    return !isInternalQuery(key);
  });
  if (kept.length === pairs.length) return search;
  return kept.some((pair) => pair !== '') ? `?${kept.join('&')}` : '';
}

/// A route's params from what `resolveRoutes` matched (`routeMatches`: named groups such as
/// `nxtPslug`, values still percent-encoded as in the path), in the shape Next's handlers take as
/// `requestMeta.params`: decoded strings, an array for a catch-all, nothing for an optional
/// catch-all that matched nothing. Only names in the route's own template are taken.
export function routeParams(template, matches) {
  if (!matches) return undefined;
  const out = {};
  let any = false;
  for (const m of template.matchAll(/\[\[\.\.\.([^\]]+)\]\]|\[\.\.\.([^\]]+)\]|\[([^\]]+)\]/g)) {
    const name = m[1] ?? m[2] ?? m[3];
    const repeat = m[3] === undefined;
    const value = matches[`nxtP${name}`] ?? matches[`nxtI${name}`] ?? matches[name];
    if (typeof value !== 'string' || value === '') continue;
    try {
      out[name] = repeat
        ? value
            .split('/')
            .filter(Boolean)
            .map((part) => decodeURIComponent(part))
        : decodeURIComponent(value);
    } catch {
      return null; // not decodable: a 400, as Next answers
    }
    any = true;
  }
  return any ? out : undefined;
}

/// A route's params read off a concrete path, for a path routing matched as itself (a
/// prerendered `/docs/a/b` of `/docs/[...path]`) and so gave no matches for. `template` is the
/// route (`/docs/[...path]`); `pathname` the concrete path, still percent-encoded. Undefined
/// when the path isn't one of the route's.
export function paramsFromPath(template, pathname) {
  const names = [];
  const escape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const pattern = template
    .split('/')
    .map((whole) => {
      // An intercepting route's segment carries its marker before the param (`(.)[id]`, the
      // concrete path `(.)1`): the marker is plain text on both sides.
      const marked = /^((?:\(\.{1,3}\))+)(\[.+\])$/.exec(whole);
      const marker = marked ? escape(marked[1]) : '';
      const part = marked ? marked[2] : whole;
      const optional = /^\[\[\.\.\.(.+)\]\]$/.exec(part);
      if (optional) return names.push(optional[1]) && `(?:/${marker}(.+?))?`;
      const rest = /^\[\.\.\.(.+)\]$/.exec(part);
      if (rest) return names.push(rest[1]) && `/${marker}(.+?)`;
      const one = /^\[(.+)\]$/.exec(part);
      if (one) return names.push(one[1]) && `/${marker}([^/]+?)`;
      return part === '' ? '' : `/${escape(part)}`;
    })
    .join('');
  if (names.length === 0) return undefined;
  const match = new RegExp(`^${pattern}/?$`).exec(pathname);
  if (!match) return undefined;
  return routeParams(template, Object.fromEntries(names.map((name, i) => [name, match[i + 1] ?? ''])));
}

/// A request path as the build names prerendered paths: each part decoded, except that an
/// encoded slash stays encoded (`/a%20b/c%2Fd` → `/a b/c%2Fd`). Null when it doesn't decode.
export function decodedPath(pathname) {
  try {
    return pathname
      .split('/')
      .map((part) => decodeURIComponent(part.replace(/%2f/gi, '%252F')))
      .join('/');
  } catch {
    return null;
  }
}

/// The query a handler is told (`requestMeta.query`): what routing resolved (the visitor's own
/// plus what rewrites and middleware added), without the internal keys.
export function handlerQuery(resolved) {
  const out = {};
  for (const [key, value] of Object.entries(resolved ?? {})) {
    if (!isInternalQuery(key)) out[key] = value;
  }
  return out;
}

/// A `Location` as Next's own server writes it: a path when the target is on the visitor's
/// origin, the whole URL when it is somewhere else.
export function location(target, origin) {
  return target.origin === origin ? `${target.pathname}${target.search}${target.hash}` : target.href;
}

/// Node's request headers as a `Headers`, as middleware and routing take them. `cookie` values
/// join with `; `, the rest with `, ` (what Node itself does for repeats it joins).
export function toWebHeaders(nodeHeaders) {
  const headers = new Headers();
  for (const [name, value] of Object.entries(nodeHeaders)) {
    if (value === undefined) continue;
    if (Array.isArray(value)) {
      if (name === 'set-cookie') for (const v of value) headers.append(name, v);
      else headers.set(name, value.join(name === 'cookie' ? '; ' : ', '));
    } else {
      try {
        headers.set(name, value);
      } catch {
        // A value Node let through that `Headers` refuses (a control character): not passed on.
      }
    }
  }
  return headers;
}

/// The other way: a `Headers` as the object a Node request carries (`req.headers`).
export function toNodeHeaders(headers) {
  const out = {};
  for (const [name, value] of headers) {
    if (name === 'set-cookie') continue;
    out[name] = value;
  }
  const cookies = headers.getSetCookie?.() ?? [];
  if (cookies.length) out['set-cookie'] = cookies;
  return out;
}

/// Headers of a middleware's own answer that are its protocol with the router, not the visitor's.
const MIDDLEWARE_PROTOCOL = /^x-middleware-(next|rewrite|redirect|refresh|override-headers|set-cookie|request-.*)$/;
export function isMiddlewareProtocol(name) {
  return MIDDLEWARE_PROTOCOL.test(name);
}

/// Hop-by-hop headers: never copied across a proxy hop (an external rewrite).
const HOP = new Set(['connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization', 'te', 'trailer', 'transfer-encoding', 'upgrade']);
export function isHopByHop(name) {
  return HOP.has(name);
}

/// Which of the build's outputs answers a resolved pathname. `functions` and `statics` are the
/// manifest's maps (pathname → entry). A trailing slash and the `/index` spelling are tried too
/// (the home page is `/index` when the build wrote it as a file). Where a pathname is both a
/// file and a function (`/404`, `/500`), the file is the build's verdict that the page is
/// static, and it wins, as in Next's own server.
export function findOutput(manifest, pathname) {
  if (!pathname) return null;
  const names = [pathname];
  if (pathname.length > 1 && pathname.endsWith('/')) names.push(pathname.slice(0, -1));
  else if (pathname !== '/') names.push(`${pathname}/`);
  if (pathname.endsWith('/index')) names.push(pathname.slice(0, -'/index'.length) || '/');
  for (const name of [...names]) names.push(name === '/' ? '/index' : `${name.replace(/\/$/, '')}/index`);
  for (const name of names) {
    if (Object.hasOwn(manifest.statics, name)) return { kind: 'static', pathname: name, file: manifest.statics[name] };
    if (Object.hasOwn(manifest.functions, name)) return { kind: 'function', pathname: name, entry: manifest.functions[name] };
  }
  // A path the build prerendered (`/blog/first`, a page's `/_next/data/<id>/x.json`): answered by
  // the page it was made from.
  for (const name of names) {
    const parent = Object.hasOwn(manifest.prerenders ?? {}, name) ? manifest.prerenders[name].parent : undefined;
    if (parent === undefined) continue;
    for (const [pathname, entry] of Object.entries(manifest.functions)) {
      if (entry.id === parent) return { kind: 'function', pathname, entry, prerendered: name };
    }
  }
  return null;
}

/// A path as a browser writes it in a URL: what isn't plain ASCII, and `%`, percent-encoded
/// (UTF-8); `[` and `]` (Next's own `[slug]` chunk names) as they are.
export function urlSpelling(pathname) {
  try {
    return encodeURI(pathname).replace(/%5B/g, '[').replace(/%5D/g, ']');
  } catch {
    return pathname;
  }
}

/// Every pathname routing may land on: the functions, the static files, and the paths the build
/// prerendered. The last are how a route with `fallback: false` (or `dynamicParams = false`)
/// works at all: its pattern only matches in Draft Mode, so a path is found only if the build
/// made it. With `trailingSlash: true` a page's address ends in a slash, so each page is known
/// under that form too.
export function routablePathnames(manifest) {
  const base = manifest.basePath || '';
  const names = new Set([...Object.keys(manifest.functions), ...Object.keys(manifest.statics), ...Object.keys(manifest.prerenders ?? {})]);
  for (const name of [...names]) {
    if (name === `${base}/index`) names.add(base || '/');
    else if (name.endsWith('/index')) names.add(name.slice(0, -'/index'.length));
  }
  if (manifest.trailingSlash) {
    for (const name of [...names]) if (!name.endsWith('/') && !/\.[^/]+$/.test(name)) names.add(`${name}/`);
  }
  return [...names];
}

// --- priming (main.mjs): the rules Next's own server gets under forkrail (next-standalone.cjs) ---

/// Priming stops loading entrypoints once the process holds this share of the function's memory.
export const PRELOAD_MEMORY_PCT = 40;
const MAX_PRIME_PATHS = 8;
const SITE = /^(?:[a-z0-9_](?:[a-z0-9_-]*[a-z0-9_])?(?:\.[a-z0-9_](?:[a-z0-9_-]*[a-z0-9_])?)*|\[[0-9a-f:.]+\])(?::\d{1,5})?$/i;

/// The pages a project asked to have rendered at the start (FORKRAIL_PRIME_PATHS: `/,/pricing`).
export function primePaths(value) {
  return String(value || '')
    .split(/[\s,]+/)
    .filter((path) => path.startsWith('/') && !path.startsWith('//') && !/[\r\n]/.test(path))
    .slice(0, MAX_PRIME_PATHS);
}

/// The headers of a visitor of the deployment's own URL (`VERCEL_URL`), for those pages.
export function primeVisitor(site) {
  const host = SITE.test(site || '') ? site : 'localhost';
  return { host, 'x-forwarded-host': host, 'x-forwarded-proto': 'https', 'x-forwarded-port': '443', 'x-forwarded-for': '127.0.0.1', 'user-agent': 'forkrail-prime' };
}

/// Is this Next version one the server has been verified on? `verified` is a list of
/// `major.minor` strings. Prereleases (`-canary.N`, `-rc.N`) are never verified: their internals
/// move between builds.
export function versionVerdict(version, verified) {
  const m = /^(\d+)\.(\d+)\.(\d+)(-.+)?$/.exec(String(version ?? ''));
  if (!m) return { ok: false, reason: `"${version}" is not a version` };
  if (m[4]) return { ok: false, reason: `${version} is a prerelease` };
  const minor = `${m[1]}.${m[2]}`;
  if (!verified.includes(minor)) return { ok: false, reason: `${minor} is not a verified release line (${verified.join(', ')})` };
  return { ok: true, minor };
}

// --- the RSC cache-busting parameter ---------------------------------------------------------
//
// Next's client sends `_rsc=<hash>` with every RSC request: a hash of the headers that change the
// answer (`next-router-prefetch`, `next-router-segment-prefetch`, `next-router-state-tree`,
// `next-url`). A cache keyed by URL alone (one that ignores `Vary`) would otherwise hand one
// kind of payload to a request for another. Next's own server checks it and answers a mismatch
// with a 307 to the right URL (base-server.ts); with no Next server here, this file does.
// After Next's `shared/lib/router/utils/cache-busting-search-param.ts` and `shared/lib/hash.ts`
// (MIT).

function rscInput(prefetch, segment, stateTree, nextUrl) {
  const join = (value) => (value === undefined ? '0' : Array.isArray(value) ? value.join(',') : value);
  if ((prefetch === undefined || prefetch === '0') && segment === undefined && stateTree === undefined && nextUrl === undefined) return null;
  return [prefetch ?? '0', join(segment), join(stateTree), join(nextUrl)].join(',');
}

/// The `_rsc` value Next's client computes for these request headers: SHA-256 cut to 96 bits,
/// base64url. Empty when none of the four headers is there.
export async function rscHash(headers) {
  const prefetch = headers['next-router-prefetch'];
  const input = rscInput(['1', '2', '3'].includes(prefetch) ? prefetch : undefined, headers['next-router-segment-prefetch'], headers['next-router-state-tree'], headers['next-url']);
  if (input === null) return { hash: '', legacy: '' };
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(input))).subarray(0, 12);
  const hash = Buffer.from(digest).toString('base64url');
  // Clients without WebCrypto (no secure context) send the old five-character form.
  let h = 5381;
  for (let i = 0; i < input.length; i++) h = ((h << 5) + h + input.charCodeAt(i)) & 0xffffffff;
  return { hash, legacy: (h >>> 0).toString(36).slice(0, 5) };
}

/// The request's own `_rsc` (null when it has none), read without re-encoding the query.
export function rscParam(search) {
  for (const pair of search.replace(/^\?/, '').split('&')) {
    if (pair === '_rsc') return '';
    if (pair.startsWith('_rsc=')) return pair.slice(5);
  }
  return null;
}

/// A query string with `_rsc` replaced (`hash` a string) or taken off (`hash` null). The other
/// pairs are kept byte for byte (`%20` stays `%20`).
export function withRsc(search, hash) {
  const pairs = search
    .replace(/^\?/, '')
    .split('&')
    .filter((pair) => pair && pair !== '_rsc' && !pair.startsWith('_rsc='));
  if (hash !== null) pairs.push(hash.length ? `_rsc=${hash}` : '_rsc');
  return pairs.length ? `?${pairs.join('&')}` : '';
}

// --- a page's ETag ---------------------------------------------------------------------------

/// The ETag Next gives a page it sends whole: FNV-1a (52 bits) of the body and its length, base
/// 36. After Next's `server/lib/etag.ts` (MIT), so a page this server sends from a file answers
/// `If-None-Match` the same as when Next renders it.
export function pageEtag(payload) {
  let v0 = 0x2325;
  let v1 = 0x8422;
  let v2 = 0x9ce4;
  let v3 = 0xcbf2;
  for (let i = 0; i < payload.length; i++) {
    v0 ^= payload.charCodeAt(i);
    const t0 = v0 * 435;
    let t1 = v1 * 435;
    let t2 = v2 * 435;
    let t3 = v3 * 435;
    t2 += v0 << 8;
    t3 += v1 << 8;
    t1 += t0 >>> 16;
    v0 = t0 & 65535;
    t2 += t1 >>> 16;
    v1 = t1 & 65535;
    v3 = (t3 + (t2 >>> 16)) & 65535;
    v2 = t2 & 65535;
  }
  const hash = (v3 & 15) * 281474976710656 + v2 * 4294967296 + v1 * 65536 + (v0 ^ (v3 >> 4));
  return `"${hash.toString(36)}${payload.length.toString(36)}"`;
}
