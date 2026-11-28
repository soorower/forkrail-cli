// forkrail's Next.js adapter. The CLI writes this file into the build's scratch folder and names it
// in NEXT_ADAPTER_PATH (Next 15.5, and 16.2 and later: 16.0 and 16.1 would build their pages in
// minimal mode with one); it is never added to the project. It does two things:
//
// - `modifyConfig`: asks Next for its standalone server (`output: 'standalone'`), which is what
//   forkrail runs, and sets the shared cache's handlers when the build names them (see "The cache
//   seam" below). A static export (`output: 'export'`) is left as it is.
// - `onBuildComplete` (Next 16.2 and later, whose Adapters API hands over every output; defined
//   only there, FORKRAIL_NEXT_OUTPUTS=1): writes
//   what forkrail's manifest is made from to `<distDir>/forkrail-next.json`, and under `handlers`
//   in it what a server that calls Next's entrypoints directly needs of the same build: every
//   output's entry file, `config`, `edgeRuntime` and traced files (one table, each file once, by
//   its path from the repository's root). That part holds the keys Next puts in an Edge entry's
//   environment: the file is read by the CLI and never packaged. Before 16.2 the context has
//   another shape and nothing is written: the CLI reads `.next`'s own manifests instead.
//
// The cache seam: FORKRAIL_NEXT_CACHE_HANDLERS, when set, is JSON
//   { "cacheHandler": "/abs/handler.js", "cacheHandlers": { "default": "/abs/x.js", ... },
//     "cacheMaxMemorySize": 0 }
// and each key given is put in the config as it is. Nothing sets it yet.
'use strict'
const fs = require('node:fs')
const path = require('node:path')

const OUTPUT = 'forkrail-next.json'
const FORMAT = 1

function cacheSeam() {
  const raw = process.env.FORKRAIL_NEXT_CACHE_HANDLERS
  if (!raw) return null
  try {
    const seam = JSON.parse(raw)
    return seam && typeof seam === 'object' ? seam : null
  } catch {
    throw new Error("forkrail: FORKRAIL_NEXT_CACHE_HANDLERS isn't JSON")
  }
}

// A function output, without its traced files.
function fn(kind, o, projectDir) {
  const config = o.config || {}
  return {
    kind,
    id: o.id,
    pathname: o.pathname,
    sourcePage: o.sourcePage,
    runtime: o.runtime,
    file: o.filePath ? path.relative(projectDir, o.filePath) : undefined,
    maxDuration: config.maxDuration,
    preferredRegion: config.preferredRegion,
  }
}

const adapter = {
  name: 'forkrail',

  modifyConfig(config) {
    if (config.output === 'export') return config
    const next = { ...config, output: 'standalone' }
    const cache = cacheSeam()
    if (cache) {
      if (typeof cache.cacheHandler === 'string') next.cacheHandler = cache.cacheHandler
      if (cache.cacheHandlers && typeof cache.cacheHandlers === 'object') next.cacheHandlers = { ...(config.cacheHandlers || {}), ...cache.cacheHandlers }
      if (typeof cache.cacheMaxMemorySize === 'number') next.cacheMaxMemorySize = cache.cacheMaxMemorySize
    }
    return next
  }
}

async function onBuildComplete(ctx) {
  const outputs = ctx && ctx.outputs
  // Next 15.5 to 16.1: the experimental API's context. `.next`'s manifests say the same.
  if (!outputs || !ctx.routing || !ctx.config || !Array.isArray(outputs.prerenders) || !ctx.distDir) return
  const projectDir = ctx.projectDir || process.cwd()
  const repoRoot = ctx.repoRoot || projectDir
  // The traced files of every output, each once: an output names its own by their place here.
  // An entry is the file's path from the repository's root, or [that, where it is read from]
  // when Next places it under another name (Turbopack's externals).
  const files = []
  const places = new Map()
  const place = (rel, abs) => {
    if (!places.has(rel)) {
      const from = path.relative(repoRoot, abs)
      places.set(rel, files.length)
      files.push(from === rel ? rel : [rel, from])
    }
    return places.get(rel)
  }
  const handler = (kind, o) => ({
    kind,
    id: o.id,
    pathname: o.pathname,
    sourcePage: o.sourcePage,
    runtime: o.runtime,
    file: path.relative(repoRoot, o.filePath),
    config: o.config || {},
    edgeRuntime: o.edgeRuntime,
    assets: Object.entries(o.assets || {}).map(([rel, abs]) => place(rel, abs)),
    wasmAssets: o.wasmAssets ? Object.fromEntries(Object.entries(o.wasmAssets).map(([name, abs]) => [name, path.relative(repoRoot, abs)])) : undefined,
  })
  const handlers = { repoRoot, projectDir: path.relative(repoRoot, projectDir), outputs: [], middleware: null, files }
  // Every static file Next lists (`.next/static`, `public/`, static pages, static metadata routes), [path served at, file from the root].
  handlers.staticFiles = (outputs.staticFiles || []).map((s) => [s.pathname, path.relative(repoRoot, s.filePath)])
  const distDir = path.isAbsolute(ctx.distDir) ? ctx.distDir : path.join(projectDir, ctx.distDir)
  const config = ctx.config
  const experimental = config.experimental || {}
  const functions = []
  for (const kind of ['pages', 'pagesApi', 'appPages', 'appRoutes']) {
    for (const o of outputs[kind] || []) {
      functions.push({ ...fn(kind, o, projectDir), edgeRuntime: o.edgeRuntime })
      handlers.outputs.push(handler(kind, o))
    }
  }
  let middleware = null
  if (outputs.middleware) {
    const o = outputs.middleware
    middleware = { ...fn('middleware', o, projectDir), matchers: (o.config && o.config.matchers) || [] }
    handlers.middleware = handler('middleware', o)
  }
  const prerenders = outputs.prerenders.map((p) => {
    const fallback = p.fallback
      ? {
          file: p.fallback.filePath ? path.relative(projectDir, p.fallback.filePath) : undefined,
          initialStatus: p.fallback.initialStatus,
          initialHeaders: p.fallback.initialHeaders,
          initialExpiration: p.fallback.initialExpiration,
          initialRevalidate: p.fallback.initialRevalidate,
          postponedState: p.fallback.postponedState,
        }
      : undefined
    const c = p.config || {}
    return {
      id: p.id,
      pathname: p.pathname,
      parentOutputId: p.parentOutputId,
      groupId: p.groupId,
      route: p.route,
      routeType: p.routeType,
      response: p.response,
      compute: p.compute,
      parentFallbackMode: p.parentFallbackMode,
      pprChain: p.pprChain,
      fallback,
      allowQuery: c.allowQuery,
      allowHeader: c.allowHeader,
      bypassFor: c.bypassFor,
      renderingMode: c.renderingMode,
      partialFallback: c.partialFallback,
    }
  })
  const dump = {
    format: FORMAT,
    nextVersion: ctx.nextVersion,
    buildId: ctx.buildId,
    projectDir,
    repoRoot: ctx.repoRoot,
    distDir: path.relative(projectDir, distDir),
    config: {
      basePath: config.basePath,
      assetPrefix: config.assetPrefix,
      trailingSlash: config.trailingSlash,
      i18n: config.i18n,
      deploymentId: config.deploymentId,
      cacheComponents: config.cacheComponents,
      ppr: experimental.ppr,
      cacheHandler: Boolean(config.cacheHandler),
    },
    routing: ctx.routing,
    functions,
    middleware,
    prerenders,
    handlers,
  }
  fs.writeFileSync(path.join(distDir, OUTPUT), JSON.stringify(dump))
}

// Next 15.5 to 16.1 make the context they'd hand the hook with a pass of their own over the build,
// which fails on some apps (15.5's Pages Router with i18n: `Invariant: failed to find source route
// /en/isr`), and its shape isn't the one read here: `.next`'s manifests say the same. So the hook
// is there only from 16.2, which the CLI says with FORKRAIL_NEXT_OUTPUTS=1.
if (process.env.FORKRAIL_NEXT_OUTPUTS === '1') adapter.onBuildComplete = onBuildComplete

module.exports = adapter
