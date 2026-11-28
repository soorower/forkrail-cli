// forkrail, for Next.js 13.5 to 14.2: gives the build's config forkrail's cache handler without a
// change to the project's next.config.
//
// These versions take a cache handler only from the config file (`cacheHandler` on 14.1 and 14.2,
// `experimental.incrementalCacheHandlerPath` on 13.5 and 14.0), and have no adapter to set it
// through (15.5 and later do; 15.0 to 15.4 need none, they find the handlers at run time). So the
// build's Node processes preload this file (NODE_OPTIONS=--require, set for the build command
// only), which wraps Next's own config loader: the config it returns names the handler and turns
// Next's in-process page memory off, unless the project names a handler of its own.
//
// The handler it names (FORKRAIL_NEXT_CACHE_STUB) is a two-line file the CLI writes into the
// project's `node_modules/.forkrail/`, where Next's file tracing finds it for the standalone
// folder: forkrail's class in a worker, Next's own file cache during the build.
'use strict'
const Module = require('node:module')
const path = require('node:path')

const stub = process.env.FORKRAIL_NEXT_CACHE_STUB
const CONFIG = path.join('next', 'dist', 'server', 'config.js')

function patch(config, version) {
  if (!config || typeof config !== 'object') return config
  const experimental = config.experimental || {}
  // The project's own handler stays.
  if (config.cacheHandler || experimental.incrementalCacheHandlerPath) return config
  const [major, minor] = String(version).split('.').map(Number)
  if (major === 14 && minor >= 1) return { ...config, cacheHandler: stub, cacheMaxMemorySize: 0 }
  if (major === 13 || (major === 14 && minor === 0)) {
    return { ...config, experimental: { ...experimental, incrementalCacheHandlerPath: stub, isrMemoryCacheSize: 0 } }
  }
  return config
}

if (stub) {
  const wrapped = new WeakMap()
  const load = Module._load
  Module._load = function (request, parent, isMain) {
    const loaded = load.apply(this, arguments)
    if (typeof request !== 'string' || !/config(\.js)?$/.test(request) || !loaded || typeof loaded.default !== 'function') return loaded
    if (wrapped.has(loaded)) return wrapped.get(loaded)
    let file
    try {
      file = Module._resolveFilename(request, parent, isMain)
    } catch {
      return loaded
    }
    if (!file.endsWith(CONFIG)) return loaded
    let version = ''
    try {
      version = require(path.join(path.dirname(file), '..', '..', 'package.json')).version
    } catch {}
    const original = loaded.default
    const loadConfig = async function (...args) {
      return patch(await original.apply(this, args), version)
    }
    // Next's module defines `default` as a getter that can't be redefined: the callers get the
    // same module with that one name answered here.
    const proxy = new Proxy(loaded, { get: (target, key) => (key === 'default' ? loadConfig : target[key]) })
    wrapped.set(loaded, proxy)
    return proxy
  }
}

module.exports = { patch }
