// The build's prerenders as the seed of forkrail's Next.js cache (N2).
//
// Run by the builder after `next build`, in the folder Next's server runs from (the standalone
// folder): reads every prerendered page, route handler answer and fallback shell the build left
// in `<dist>/server` through Next's own file-system cache (so whatever that Next's layout is,
// the values are the ones its server would read), and writes each as an entry of the router's
// store, exactly as the worker's handler would write it (`next-cache.mjs`, beside this file):
//
//   <out>/index.json            { v: 1, built_ms, entries: [<key hash>, ...] }
//   <out>/<key hash>            a page as the router keeps one (`forkrail_common::nextcache`)
//
// The builder uploads <out> to `static/next-seed/<deployment>/` in each region's bucket; the
// router reads an entry the first time it's asked for. The prerendered files then needn't ship in
// the function package.
//
// Usage: node next-seed.mjs APP_DIR OUT_DIR DEPLOYMENT [DIST_DIR]
// Prints one line of JSON: { entries, bytes, skipped, built_ms }.

import { createHash } from 'node:crypto';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';

import { entryBody, fromNext } from './next-cache.mjs';

const MAGIC = 'forkrail-response 1\n';
const MAX_PAGE_BYTES = 20 << 20;
const sha256 = (text) => createHash('sha256').update(text).digest('hex');
const exists = (file) => fs.existsSync(file);
const json = (file) => JSON.parse(fs.readFileSync(file, 'utf8'));

function storableTag(tag) {
  const trimmed = String(tag).trim();
  // eslint-disable-next-line no-control-regex
  return trimmed && Buffer.byteLength(trimmed) <= 256 && !trimmed.includes(',') && !/[\x00-\x1f\x7f]/.test(trimmed) ? trimmed : null;
}

// eslint-disable-next-line no-control-regex
const storableHeader = (name, value) => /^[!#$%&'*+\-.^_`|~0-9a-z]+$/.test(name) && !/[\x00-\x08\x0a-\x1f\x7f]/.test(value);

/// An entry as the router keeps a page: the format line, a line of JSON, the headers as
/// `name\0value\n`, a blank line, the body.
export function encodePage({ scope, keyHash, status, storedMs, rev, headers, tags, body }) {
  const hash = sha256(`next\n${scope}\n${keyHash}`);
  const freshUntil = typeof rev === 'number' && Number.isFinite(rev) ? storedMs + Math.max(0, rev) * 1000 : null;
  const meta = { status, stored_ms: storedMs, fresh_until: freshUntil === null ? null : Math.floor(freshUntil), group: null, deployment: scope, hash };
  const lines = [];
  for (const [name, value] of headers) {
    if (storableHeader(name, value) && name !== 'x-forkrail-next' && name !== 'vercel-cache-tag') lines.push(Buffer.from(`${name}\0${value}\n`));
  }
  lines.push(Buffer.from('x-forkrail-next\x001\n'));
  const kept = [...new Set(tags.map(storableTag).filter(Boolean))].slice(0, 128);
  if (kept.length) lines.push(Buffer.from(`vercel-cache-tag\0${kept.join(',')}\n`));
  return Buffer.concat([Buffer.from(MAGIC), Buffer.from(`${JSON.stringify(meta)}\n`), ...lines, Buffer.from('\n'), ...body]);
}

/// Every file under `dir`, relative to it.
function walk(dir, under = '') {
  const found = [];
  if (!exists(dir)) return found;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = `${under}/${entry.name}`;
    if (entry.isDirectory()) found.push(...walk(path.join(dir, entry.name), rel));
    else found.push(rel);
  }
  return found;
}

/// Next's `normalizePagePath`: `/` is `/index`, and a path that starts with `/index` (not a
/// dynamic one) gets another `/index` in front, so the two never meet.
export function pagePath(page) {
  const path = page.startsWith('/') ? page : `/${page}`;
  if (path === '/') return '/index';
  return /^\/index(\/|$)/.test(path) && !/\[[^/]+?\](?=\/|$)/.test(path) ? `/index${path}` : path;
}

export async function seed({ appDir, outDir, deployment, distDir = '.next' }) {
  const dist = path.join(appDir, distDir);
  const serverDir = path.join(dist, 'server');
  const require = createRequire(path.join(appDir, 'package.json'));
  const manifest = exists(path.join(dist, 'prerender-manifest.json')) ? json(path.join(dist, 'prerender-manifest.json')) : { routes: {}, dynamicRoutes: {} };
  const interop = (module) => module?.default ?? module;
  const FileSystemCache = interop(require('next/dist/server/lib/incremental-cache/file-system-cache'));
  let nodeFs;
  try {
    nodeFs = require('next/dist/server/lib/node-fs-methods').nodeFs;
  } catch {}
  nodeFs ??= { existsSync: fs.existsSync, readFile: fs.promises.readFile, readFileSync: fs.readFileSync, writeFile: (f, d) => fs.promises.writeFile(f, d), mkdir: (d) => fs.promises.mkdir(d, { recursive: true }), stat: (f) => fs.promises.stat(f) };
  // What the server's own IncrementalCache would give it. Next 13.5 and 14 look for a page under
  // `app/` only with `_appDir`, and 14 reads `experimental.ppr` for every App Router page (without
  // it every such page threw, and was left out of the seed).
  let experimental = {};
  try {
    experimental = json(path.join(dist, 'required-server-files.json')).config?.experimental ?? {};
  } catch {}
  // Nothing is written back: reads only.
  const cache = new FileSystemCache({
    fs: nodeFs,
    dev: false,
    flushToDisk: false,
    serverDistDir: serverDir,
    revalidatedTags: [],
    maxMemoryCacheSize: 0,
    _requestHeaders: {},
    _appDir: exists(path.join(serverDir, 'app')),
    _pagesDir: exists(path.join(serverDir, 'pages')),
    experimental,
  });

  const kindOf = (route) => (route.dataRoute === null || route.dataRoute === undefined ? (route.routeType === 'route' || route.dataRoute === null ? 'APP_ROUTE' : 'PAGES') : String(route.dataRoute).endsWith('.json') ? 'PAGES' : 'APP_PAGE');
  const ppr = (route) => route?.renderingMode === 'PARTIALLY_STATIC' || route?.experimentalPPR === true;

  /// What to ask Next's cache for: [key, kind, route, fallback].
  const asks = new Map();
  const ask = (key, kind, route, fallback = false) => asks.has(key) || asks.set(key, { key, kind, route, fallback });

  // Next 15.5.27+, 16.3.8+: keys carry the kind and a hash of the source route
  // (`/route-cache/APP_PAGE/<sha256>/$/blog/first`). With an adapter the build writes the files
  // under those names; without one, each page's `.meta` names its key.
  const scoped = path.join(serverDir, 'route-cache');
  for (const file of walk(scoped)) {
    const match = /^\/(APP_PAGE|APP_ROUTE|PAGES)\/[0-9a-f]{64}\/\$(\/.*)\.(html|body)$/.exec(file);
    if (!match) continue;
    const pathname = match[2] === '/index' ? '/' : match[2];
    const route = manifest.routes?.[pathname] ?? manifest.dynamicRoutes?.[pathname];
    ask(`/route-cache${file.slice(0, -(match[3].length + 1))}`, match[1], route, Boolean(!manifest.routes?.[pathname] && manifest.dynamicRoutes?.[pathname]));
  }
  for (const under of ['app', 'pages']) {
    for (const file of walk(path.join(serverDir, under))) {
      if (!file.endsWith('.meta')) continue;
      try {
        const meta = json(path.join(serverDir, under, file));
        if (meta.routeCache?.key) {
          const pathname = file.slice(0, -5) === '/index' ? '/' : file.slice(0, -5);
          ask(meta.routeCache.key, meta.routeCache.key.split('/')[2], manifest.routes?.[pathname] ?? manifest.dynamicRoutes?.[pathname], Boolean(meta.routeCache.isFallback));
        }
      } catch {}
    }
  }
  // Every other Next: a page's key is its path as Next normalizes it (`/` is `/index`), which is
  // what its server asks the handler for.
  if (asks.size === 0) {
    for (const [pathname, route] of Object.entries(manifest.routes ?? {})) ask(pagePath(pathname), kindOf(route), route);
  }

  fs.mkdirSync(outDir, { recursive: true });
  const entries = [];
  let bytes = 0;
  let skipped = 0;
  let builtMs = 0;
  for (const { key, kind, route, fallback } of asks.values()) {
    let found = null;
    try {
      found = await cache.get(key, { kind, kindHint: kind === 'PAGES' ? 'pages' : 'app', isRoutePPREnabled: ppr(route), isFallback: fallback, tags: [], softTags: [] });
    } catch (error) {
      console.error(`forkrail: ${key}: Next's cache couldn't read it: ${error?.message ?? error}`);
    }
    if (!found?.value) {
      skipped++;
      continue;
    }
    const revalidate = route?.initialRevalidateSeconds ?? route?.fallbackRevalidate;
    const expire = route?.initialExpireSeconds ?? route?.fallbackExpire;
    const entry = fromNext(found.value, { cacheControl: { revalidate, expire } });
    if (entry.kind === 'FETCH') continue;
    const keyHash = sha256(key);
    const meta = { kind: entry.kind, k: keyHash, key, parts: entry.parts.map(([name, part]) => [name, part.length]), rev: entry.rev, exp: entry.exp, x: entry.x };
    const storedMs = Math.floor(found.lastModified > 0 ? found.lastModified : Date.now());
    const page = encodePage({ scope: deployment, keyHash, status: entry.status ?? 200, storedMs, rev: entry.rev, headers: entry.headers ?? [], tags: entry.tags ?? [], body: entryBody(meta, entry.parts.map(([, part]) => part)) });
    if (page.length > MAX_PAGE_BYTES) {
      skipped++;
      continue;
    }
    fs.writeFileSync(path.join(outDir, keyHash), page);
    entries.push(keyHash);
    bytes += page.length;
    builtMs = Math.max(builtMs, storedMs);
  }
  fs.writeFileSync(path.join(outDir, 'index.json'), JSON.stringify({ v: 1, built_ms: builtMs, entries }));
  return { entries: entries.length, bytes, skipped, built_ms: builtMs };
}

// Run as a script (its own path, links resolved: a temporary folder may be reached through one).
const real = (file) => {
  try {
    return fs.realpathSync(file);
  } catch {
    return path.resolve(file);
  }
};
if (process.argv[1] && real(process.argv[1]) === real(new URL(import.meta.url).pathname)) {
  const [appDir, outDir, deployment, distDir] = process.argv.slice(2);
  if (!appDir || !outDir || !deployment) {
    console.error('usage: node next-seed.mjs APP_DIR OUT_DIR DEPLOYMENT [DIST_DIR]');
    process.exit(2);
  }
  seed({ appDir: path.resolve(appDir), outDir: path.resolve(outDir), deployment, distDir }).then(
    (summary) => console.log(JSON.stringify(summary)),
    (error) => {
      console.error(`forkrail: seeding Next.js's cache failed: ${error?.stack ?? error}`);
      process.exit(1);
    },
  );
}
