'use strict'
// forkrail's part of running Next.js's own server (a build with `output: 'standalone'`, Next 13.5
// to 16). The build puts this file beside Next's generated `server.js`, which stays as built, and
// starts the app through an entry of its own:
//
//   // forkrail-server.mjs
//   import { createRequire } from 'node:module'
//   import { fileURLToPath } from 'node:url'
//   const forkrail = createRequire(import.meta.url)('./forkrail-next.cjs')
//   forkrail.prime()
//   forkrail.routerFiles()
//   await import('./server.js')
//   forkrail.install(fileURLToPath(new URL('./server.js', import.meta.url)))
//
// The order matters: `server.js` sets NODE_ENV and loads `next` as it's evaluated, and none of
// Next's modules may be loaded before it has (15.5's Pages Router breaks otherwise), so `install`
// comes after it, in the same tick, before the server takes its first request. `prime` only leaves
// a function for the shim and `routerFiles` a hook for when Next's modules load; neither loads
// anything of Next's.
//
// What it does:
//
// 1. The visitor's origin (`install`). Next's server builds every request's URL from the hostname
//    and port it listens on, in two places (`NextNodeServer.attachRequestMeta` for route handlers,
//    pages and server actions; `NextNodeServer.runMiddleware` for middleware and `proxy.ts`), so
//    behind the router `request.url` is `http://localhost:<port>/…` and
//    `NextResponse.redirect(new URL('/x', request.url))` sends visitors there. No setting changes
//    it (`experimental.trustHostHeader` reaches neither place, and makes Next's routing half take
//    middleware rewrites for external ones). Both methods are wrapped so the URL comes from the
//    headers the router sets for every request and no client can choose: `x-forwarded-host`,
//    `x-forwarded-proto` (and `x-forwarded-port`, which Next would otherwise fill in with its own
//    port). A rewrite the middleware makes to the visitor's own origin is handed on as a path, so
//    Next serves it itself. `trustHostHeader` is switched off where the build baked it in.
//    Checked on 13.5.11, 14.2.35, 15.5.29 and 16.4.6 (docs/data/n4, tools/next-n4/origin-check.sh);
//    a Next without these two methods gets a warning and its own behaviour.
//
// 2. Priming (`prime`). Next's server listens first and only then loads its config, sets up its
//    routing and (15.0 on) loads every page's module; left alone, all of that falls to the first
//    request, which on Lambda runs at the function's share of a core where the start runs at a
//    whole one: more than half of the first request's CPU, measured. The shim calls the function
//    left here once the server listens and holds "ready" until it settles or its deadline comes
//    (shim.mjs, `forkrail.prime`). In order:
//    a. One request that runs none of the app's request code. Where the build prerendered Next's
//       `/_global-error` page (16.x: its own `<html>`, no layout of the app's), that page, asked
//       for as an on-demand revalidation (`x-prerender-revalidate` with the build's own id), so
//       Next renders it again: the whole App Router render path has then run once. Otherwise a
//       static file that doesn't exist, which Next answers 404 from its routing. Either way Next
//       has started up to answer, and the app's middleware isn't run: Next skips it for a
//       revalidation, and `runMiddleware` here for the secret this request carries.
//    b. Pages the project named to be rendered at start, where a first visitor would otherwise
//       wait for the page's first render: `FORKRAIL_PRIME_PATHS=/,/pricing` in its env vars (at
//       most 8). Asked for as a visitor would on the deployment's own URL, middleware included:
//       only for pages that are safe to render with no visitor.
//    c. The pages' modules, as Next's own start does (`experimental.preloadEntriesOnStart`), one at
//       a time until the deadline, or until the process holds PRELOAD_MEMORY_PCT of the
//       function's memory. Next's own pass is switched off for it (`install`): it can't be
//       stopped, and on a big app it runs for seconds and loads everything (cal.com: 2.7 s of CPU
//       and 529 MB here), past what a start is allowed and what a small function holds. What the
//       loop doesn't reach is loaded by the first request that needs it, as before 15.0.
//    The app's `instrumentation.register()` and its modules' top level run at start, as they do
//    under `next start`; none of its handlers, pages, layouts or middleware does (but for b).
//
// 3. Files the router serves (`routerFiles`). The router answers a request for one of the
//    deployment's static files (`public/`, `.next/static`) itself, and the package doesn't hold
//    them, so Next's server knows nothing of them; a config rewrite onto one (`/docs/:path*` to
//    `/:path*`) found nothing and answered 404. The build lists them beside `server.js`
//    (`ROUTER_FILES`, one path per line, with the base path), and Next's filesystem check is told
//    of them: when it finds none of its own for a path, a listed file is a match, found as Next
//    finds its own (the base path, the default locale's prefix, a trailing slash, the path as
//    written or decoded). Next then serves the match, and that answer is the word to the router
//    to answer with the file (`x-forkrail-static`, `forkrail_common::next::STATIC_HEADER`) with
//    whatever routing set on the way, and nothing else. Next's `setupFsCheck` and `serveStatic`
//    are reached as `router-server.js` requires them (their exports can't be changed after
//    loading), by a hook on `require` that is let go of once it has seen both, before the server
//    listens. Where the two aren't found as expected (another Next), nothing changes: a 404 as
//    before. Checked against 13.5, 14.2, 16.1 and 16.4's `router-server.js`.

const { createRequire } = require('node:module')
const { randomBytes } = require('node:crypto')
const fs = require('node:fs')
const net = require('node:net')
const path = require('node:path')

const HOST = /^(?:[a-z0-9_](?:[a-z0-9_-]*[a-z0-9_])?(?:\.[a-z0-9_](?:[a-z0-9_-]*[a-z0-9_])?)*|\[[0-9a-f:.]+\])(?::\d{1,5})?$/i
const LOOPBACK = /^(?:127(?:\.\d{1,3}){3}|\[::1\]|localhost)$/i
const first = (v) => (Array.isArray(v) ? v[0] : typeof v === 'string' ? v.split(',')[0].trim() : undefined)

const PRIME_HEADER = 'x-forkrail-prime'
const PRIME_SECRET = randomBytes(16).toString('hex')
const PRIME_PATH = '/_next/static/forkrail-prime'
const MAX_PRIME_PATHS = 8
/** The loop over the pages' modules stops once the process holds this share of the function's memory. */
const PRELOAD_MEMORY_PCT = 40

/** The visitor's origin, from what the router sent; null when the request isn't the router's. */
function visitorOrigin(headers) {
  // A request Next makes to itself in-process (revalidation, an image's source) has no Host.
  if (!headers || !first(headers.host)) return null
  const host = first(headers['x-forwarded-host']) || first(headers.host)
  if (!host || !HOST.test(host)) return null
  // Next's own rule for the scheme (its server fills x-forwarded-proto in before this runs).
  const protocol = String(headers['x-forwarded-proto'] || '').includes('https') ? 'https' : 'http'
  const colon = host.lastIndexOf(':')
  const hasPort = colon > host.lastIndexOf(']')
  const hostname = hasPort ? host.slice(0, colon) : host
  const port = hasPort ? host.slice(colon + 1) : protocol === 'https' ? '443' : '80'
  return { protocol, hostname, port, origin: new URL(`${protocol}://${hostname}:${port}`).origin }
}

// NextURL turns loopback addresses into `localhost`; compare origins the same way.
const comparable = (origin) => {
  const u = new URL(origin)
  if (LOOPBACK.test(u.hostname)) u.hostname = 'localhost'
  return u.origin
}

/** The config `server.js` put in the environment, which Next reads when the server initializes. */
function standaloneConfig() {
  try {
    return JSON.parse(process.env.__NEXT_PRIVATE_STANDALONE_CONFIG || 'null')
  } catch {
    return null
  }
}

/** Change the config before Next reads it: `change(config)` says whether it changed anything. */
function editConfig(change) {
  const config = standaloneConfig()
  if (config && change(config)) process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(config)
}

function untrustHostHeader() {
  editConfig((config) => {
    if (!config.experimental || !config.experimental.trustHostHeader) return false
    config.experimental.trustHostHeader = false
    return true
  })
}

/** Whether the pages' modules are loaded here at start: when Next's own pass was on, which this takes over. */
let preloadHere = false
/** Where `server.js` is, and so the app. */
let appDir = process.cwd()

function takeOverPreload() {
  editConfig((config) => {
    if (!config.experimental || config.experimental.preloadEntriesOnStart !== true) return false
    config.experimental.preloadEntriesOnStart = false
    preloadHere = true
    return true
  })
}

const isPrime = (headers) => Boolean(headers) && first(headers[PRIME_HEADER]) === PRIME_SECRET

function install(serverJs, options = {}) {
  appDir = path.dirname(serverJs)
  if (options.trustHostHeader !== true) untrustHostHeader()
  if (primes && options.preload !== false) takeOverPreload()
  const load = createRequire(serverJs)
  const NextNodeServer = load('next/dist/server/next-server').default
  const meta = load('next/dist/server/request-meta')
  const proto = NextNodeServer && NextNodeServer.prototype
  if (!proto || typeof proto.attachRequestMeta !== 'function' || typeof proto.runMiddleware !== 'function') {
    console.warn('forkrail: this Next.js version has no attachRequestMeta/runMiddleware; request.url will be the local address')
    return false
  }

  // 1. `initURL`: what route handlers, pages and server actions take the request's URL from.
  const attach = proto.attachRequestMeta
  proto.attachRequestMeta = function (req, parsedUrl, isUpgradeReq) {
    attach.call(this, req, parsedUrl, isUpgradeReq)
    const visitor = visitorOrigin(req.headers)
    if (!visitor || typeof req.url !== 'string' || !req.url.startsWith('/')) return
    // Next 13.5 names the key `__NEXT_INIT_URL`.
    const key = meta.getRequestMeta(req, 'initURL') !== undefined ? 'initURL' : '__NEXT_INIT_URL'
    meta.addRequestMeta(req, key, visitor.origin + req.url)
  }

  // 2. Middleware (and proxy.ts): its URL is `${protocol}://${this.fetchHostname}:${this.port}${path}`,
  //    read before the method's first await, so the two fields are swapped for that long only.
  const run = proto.runMiddleware
  proto.runMiddleware = function (params) {
    const headers = params && params.request && params.request.headers
    // The priming request isn't a visitor's: answered as Next answers a request it skips
    // middleware for (an on-demand revalidation's), without the app's code.
    if (isPrime(headers)) return Promise.resolve({ response: new Response(null, { headers: { 'x-middleware-next': '1' } }) })
    const visitor = visitorOrigin(headers)
    if (!visitor) return run.call(this, params)
    const { fetchHostname, port } = this
    let pending
    this.fetchHostname = visitor.hostname
    this.port = visitor.port
    try {
      pending = run.call(this, params)
    } finally {
      this.fetchHostname = fetchHostname
      this.port = port
    }
    // The routing half of Next (resolve-routes) still compares a rewrite's origin with the local
    // address, and would proxy `https://<visitor's host>/x` out as an external rewrite. A rewrite
    // to the visitor's own origin is handed over as a path, which it serves itself.
    return Promise.resolve(pending).then((result) => {
      const response = result && result.response
      const rewrite = response && response.headers && response.headers.get('x-middleware-rewrite')
      if (!rewrite) return result
      let target
      try {
        target = new URL(rewrite)
      } catch {
        return result
      }
      if (comparable(target.origin) !== comparable(visitor.origin)) return result
      const path = target.pathname + target.search + target.hash
      try {
        response.headers.set('x-middleware-rewrite', path)
      } catch {
        const headers = new Headers(response.headers)
        headers.set('x-middleware-rewrite', path)
        result.response = new Response(response.body, { status: response.status, statusText: response.statusText, headers })
      }
      return result
    })
  }
  return true
}

/** One GET to the app's own server on a socket of its own (so it isn't reported as one of the
 *  app's outgoing calls), read to its end. Resolves with the status, or null past the deadline. */
function ask(host, port, path, headers, deadline) {
  return new Promise((resolve) => {
    const socket = net.connect({ host: host.replace(/^\[|\]$/g, ''), port })
    let head = ''
    const done = (status) => {
      clearTimeout(timer)
      socket.destroy()
      resolve(status)
    }
    const timer = setTimeout(() => done(null), Math.max(1, deadline - Date.now()))
    socket.on('connect', () => {
      const lines = [`GET ${path} HTTP/1.1`, 'connection: close', ...Object.entries(headers).map(([k, v]) => `${k}: ${v}`)]
      socket.write(lines.join('\r\n') + '\r\n\r\n')
    })
    socket.on('data', (chunk) => {
      if (head.length < 64) head += chunk.toString('latin1')
    })
    socket.on('error', () => done(null))
    socket.on('close', () => done(Number(head.slice(9, 12)) || null))
  })
}

/** The pages a project asked to have rendered at start (`FORKRAIL_PRIME_PATHS`). */
function primePaths(value = process.env.FORKRAIL_PRIME_PATHS) {
  return String(value || '')
    .split(/[\s,]+/)
    .filter((path) => path.startsWith('/') && !path.startsWith('//') && !/[\r\n]/.test(path))
    .slice(0, MAX_PRIME_PATHS)
}

const readJson = (file) => {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'))
  } catch {
    return null
  }
}

/** Load the pages' modules (their top level only), as Next's own start does, while there's time
 *  and memory. Returns how many of how many. */
async function preloadEntries(dist, deadline) {
  const server = path.join(dist, 'server')
  const files = [...Object.values(readJson(path.join(server, 'app-paths-manifest.json')) || {}), ...Object.values(readJson(path.join(server, 'pages-manifest.json')) || {})]
    .filter((file) => typeof file === 'string' && file.endsWith('.js'))
  const load = createRequire(path.join(appDir, 'server.js'))
  const limit = (Number(process.env.AWS_LAMBDA_FUNCTION_MEMORY_SIZE) || 0) * 1024 * 1024 * (PRELOAD_MEMORY_PCT / 100)
  let loaded = 0
  for (const file of files) {
    if (Date.now() >= deadline - 20 || (limit && process.memoryUsage.rss() > limit)) break
    try {
      // As Next's own pass does: its `fetch` in place before the app's code can keep another.
      const entry = load(path.resolve(server, file))
      if (entry && typeof entry.patchFetch === 'function') entry.patchFetch()
      loaded++
    } catch {
      // Loaded, and its error met, by the first request that needs it, as it would have been.
    }
    // Let the event loop turn, so the deadline's timer can fire between two of them.
    await new Promise((resolve) => setImmediate(resolve))
  }
  return `${loaded}/${files.length}`
}

// --- 3. Files the router serves ----------------------------------------------------------------

/** Beside `server.js`: the static files the router serves and the package doesn't hold. */
const ROUTER_FILES = 'forkrail-router-files.txt'
const STATIC_HEADER = 'x-forkrail-static'
/** The root a router's file is "served" from in Next's terms: a folder that never exists, so a
 *  `serveStatic` this hook missed answers 404 as before. */
const ROUTER_ROOT = '/forkrail-router-files'
const ROUTER_SERVER = /[\\/]next[\\/]dist[\\/]server[\\/]lib[\\/]router-server\.js$/

let listed = null
/** The listed files, read on the first path Next's own check doesn't find. */
function routerFileList() {
  if (listed === null) {
    try {
      listed = new Set(fs.readFileSync(path.join(appDir, ROUTER_FILES), 'utf8').split('\n').filter(Boolean))
    } catch {
      listed = new Set()
    }
  }
  return listed
}

/** A listed file as Next's filesystem check would give one of its own, for `itemPath` (with the
 *  base path; percent-encoded as the request had it); null when none is. `config` is Next's. */
function routerItem(itemPath, config, files = routerFileList()) {
  if (typeof itemPath !== 'string' || files.size === 0) return null
  const base = typeof config.basePath === 'string' ? config.basePath : ''
  let rest = itemPath
  if (base) {
    if (rest !== base && !rest.startsWith(base + '/')) return null
    rest = rest.slice(base.length) || '/'
  }
  if (rest !== '/' && rest.endsWith('/')) rest = rest.slice(0, -1)
  // A static file may be asked for under the default locale, and no other (Next's legacy rule).
  let locale
  const i18n = config.i18n
  if (i18n) {
    const defaults = [i18n.defaultLocale, ...(Array.isArray(i18n.domains) ? i18n.domains.map((d) => d && d.defaultLocale) : [])]
    const first = rest.split('/')[1] || ''
    const found = defaults.find((l) => typeof l === 'string' && l.toLowerCase() === first.toLowerCase())
    if (found) {
      locale = found
      rest = rest.slice(first.length + 1) || '/'
    }
  }
  let decoded = rest
  try {
    decoded = decodeURIComponent(rest)
  } catch {
    // Not decodable: only as written.
  }
  const name = [rest, decoded].find((candidate) => files.has(base + candidate))
  if (name === undefined) return null
  const hashed = name.startsWith('/_next/static/')
  const itemsRoot = ROUTER_ROOT + base + (hashed ? '/_next/static' : '')
  const item = hashed ? name.slice('/_next/static'.length) : name
  return { type: hashed ? 'nextStaticFolder' : 'publicFolder', fsPath: path.posix.join(itemsRoot, item), itemsRoot, itemPath: item, locale }
}

/** An object with a module's exports (Next's are getters that can't be replaced), `changes` over them. */
function exportsWith(exports, changes) {
  const out = { __esModule: true }
  for (const key of Object.keys(exports)) out[key] = exports[key]
  return Object.assign(out, changes)
}

/** `serveStatic` that answers a router's file (`routerItem`) with the word to the router. */
function withRouterServing(exports) {
  const serveStatic = exports && exports.serveStatic
  if (typeof serveStatic !== 'function') return exports
  return exportsWith(exports, {
    serveStatic(req, res, file, opts) {
      const root = opts && opts.root
      if (typeof root === 'string' && root.startsWith(ROUTER_ROOT) && typeof file === 'string') {
        res.setHeader(STATIC_HEADER, encodeURI(root.slice(ROUTER_ROOT.length) + file))
        res.end()
        return Promise.resolve()
      }
      return serveStatic.apply(this, arguments)
    },
  })
}

/** `setupFsCheck` whose checker also finds the router's files, after its own. */
function withRouterFiles(exports) {
  const setupFsCheck = exports && exports.setupFsCheck
  if (typeof setupFsCheck !== 'function') return exports
  return exportsWith(exports, {
    async setupFsCheck(opts) {
      const checker = await setupFsCheck.apply(this, arguments)
      const config = (opts && opts.config) || {}
      if (checker && typeof checker.getItem === 'function' && !(opts && opts.dev)) {
        const getItem = checker.getItem
        checker.getItem = async function (itemPath, ...rest) {
          return (await getItem.call(this, itemPath, ...rest)) || routerItem(itemPath, config)
        }
      }
      return checker
    },
  })
}

/** Hook `require` so `router-server.js` gets Next's filesystem check and file serving with the
 *  router's files in them. Call before `server.js` is loaded. */
function routerFiles() {
  const Module = require('node:module')
  const original = Module.prototype.require
  let serving = false
  let done = false
  const hook = function (id) {
    const exports = original.apply(this, arguments)
    if (done || (id !== '../serve-static' && id !== './router-utils/filesystem') || !ROUTER_SERVER.test((this && this.filename) || '')) return exports
    if (id === '../serve-static') {
      const wrapped = withRouterServing(exports)
      serving = wrapped !== exports
      return wrapped
    }
    // The second of the two (`router-server.js` requires file serving first): done. Files are
    // found only where their serving is this hook's too.
    done = true
    if (Module.prototype.require === hook) Module.prototype.require = original
    return serving ? withRouterFiles(exports) : exports
  }
  Module.prototype.require = hook
}

let primes = false

/** Leave the shim the work to do before the app is called ready. Call before `server.js` is loaded. */
function prime() {
  primes = true
  globalThis[Symbol.for('forkrail.prime')] = async ({ host, port, deadline }) => {
    const config = standaloneConfig() || {}
    const base = typeof config.basePath === 'string' ? config.basePath : ''
    const dist = path.resolve(appDir, typeof config.distDir === 'string' ? config.distDir : '.next')
    const prerender = readJson(path.join(dist, 'prerender-manifest.json')) || {}
    const id = prerender.preview && prerender.preview.previewModeId
    // a. Next's own start-up and render path, and nothing of the app's.
    const own = { host: 'localhost', [PRIME_HEADER]: PRIME_SECRET, ...(id ? { 'x-prerender-revalidate': id } : {}) }
    const page = id && prerender.routes && prerender.routes['/_global-error'] ? '/_global-error' : PRIME_PATH
    const done = { request: `${page} ${await ask(host, port, base + page, own, deadline)}` }
    // b. What the project asked for, as a visitor of the deployment's own URL would.
    const site = HOST.test(process.env.VERCEL_URL || '') ? process.env.VERCEL_URL : 'localhost'
    const visitor = { host: site, 'x-forwarded-host': site, 'x-forwarded-proto': 'https', 'x-forwarded-port': '443', 'x-forwarded-for': '127.0.0.1', 'user-agent': 'forkrail-prime' }
    for (const asked of primePaths()) {
      if (Date.now() >= deadline) break
      done[asked] = await ask(host, port, asked, visitor, deadline)
    }
    // c. The pages' modules.
    if (preloadHere) done.modules = await preloadEntries(dist, deadline)
    return done
  }
}

module.exports = { install, prime, routerFiles, routerItem, visitorOrigin, primePaths, PRIME_HEADER, ROUTER_FILES }
