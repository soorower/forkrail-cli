// @forkrail/analytics: loads forkrail's routers' own script (/_forkrail/insights/script.js, the
// router's crates/router/src/insights.js) and queues calls for it until it has loaded, the way
// @vercel/analytics does; the script reads the same `window.va` queue, so either package works.

const NAME = '@forkrail/analytics'
const VERSION = '0.1.0'
const BASE = '/_forkrail/insights'

const browser = () => typeof window !== 'undefined'

/** Production unless told otherwise, or unless the page runs on this machine. */
function modeOf(mode) {
  if (mode && mode !== 'auto') return mode
  if (typeof process !== 'undefined' && process.env && process.env.NODE_ENV === 'development') return 'development'
  return 'production'
}

/** The queue the script empties when it loads: calls made before it are kept, in order. */
function queue() {
  if (!browser()) return
  if (window.va) return
  window.va = function va(...args) {
    ;(window.vaq = window.vaq || []).push(args)
  }
}

/**
 * Add the script to the page, once. `mode: 'development'` loads it to print what it would send
 * instead of sending it; `disableAutoTrack` leaves page views to `pageview()` (a router that knows
 * each page's route calls it); `beforeSend` can change or drop (return null) what's sent.
 */
export function inject(props = {}) {
  if (!browser()) return
  const mode = modeOf(props.mode)
  queue()
  window.vam = mode
  if (props.beforeSend) window.va('beforeSend', props.beforeSend)
  const src = props.scriptSrc || `${props.basePath || ''}${BASE}/script.js`
  if (document.head.querySelector(`script[src*="${src}"]`)) return
  const script = document.createElement('script')
  script.src = src
  script.defer = true
  script.dataset.sdkn = NAME + (props.framework ? `/${props.framework}` : '')
  script.dataset.sdkv = VERSION
  if (props.disableAutoTrack) script.dataset.disableAutoTrack = '1'
  if (props.endpoint) script.dataset.endpoint = props.endpoint
  if (mode === 'development' || props.debug) script.dataset.debug = 'true'
  script.onerror = () => console.log(`[forkrail analytics] couldn't load ${src}: is Web Analytics on for this project?`)
  document.head.appendChild(script)
}

/** Send a custom event: a name, and properties that are text, numbers, true or false. */
export function track(name, properties, options) {
  if (!browser()) return
  queue()
  window.va('event', { name, data: properties, options })
}

/** Count a page view yourself (with `disableAutoTrack`): its route template, and its path. */
export function pageview({ route, path } = {}) {
  if (!browser()) return
  queue()
  window.va('pageview', { route, path })
}

/**
 * The route template of a path from the router's params: `/blog/hello` with `{ slug: 'hello' }`
 * is `/blog/[slug]`. Null for none.
 */
export function computeRoute(pathname, params) {
  if (!pathname || !params) return pathname || null
  let route = pathname
  for (const [key, value] of Object.entries(params)) {
    if (Array.isArray(value)) {
      const joined = value.map(encodeURIComponent).join('/')
      if (joined) route = route.replace(new RegExp(`/${escape(joined)}(?=/|$)`), `/[...${key}]`)
    } else if (value) {
      route = route.replace(new RegExp(`/${escape(encodeURIComponent(value))}(?=/|$)`), `/[${key}]`)
    }
  }
  return route
}

const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

export default { inject, track, pageview, computeRoute }
