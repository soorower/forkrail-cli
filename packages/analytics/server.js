// track() from a function: sends the event to the deployment's own address (VERCEL_URL, which
// forkrail sets), saying it comes from a server (`x-va-server`) and whose visit it was: their
// address and browser, from the request being answered. Nothing is sent outside production.

const NAME = '@forkrail/analytics/server'
const VERSION = '0.1.0'

/** The visitor's address and browser from a request's headers (a Request, Headers, or a plain object). */
function visitorOf(request) {
  const headers = request && (request.headers || request)
  const get = (name) => {
    if (!headers) return undefined
    if (typeof headers.get === 'function') return headers.get(name) ?? undefined
    const v = headers[name] ?? headers[name.toLowerCase()]
    return Array.isArray(v) ? v[0] : v
  }
  const forwarded = get('x-forwarded-for')
  return {
    ip: get('x-real-ip') || (forwarded ? String(forwarded).split(',')[0].trim() : undefined),
    agent: get('user-agent'),
    url: request && typeof request.url === 'string' && /^https?:/.test(request.url) ? request.url : undefined,
  }
}

export async function track(name, properties, options = {}) {
  const env = typeof process !== 'undefined' ? process.env : {}
  const host = env.FORKRAIL_ANALYTICS_HOST || env.VERCEL_URL
  if (!host) {
    if (env.NODE_ENV !== 'production') console.log(`[forkrail analytics] track(${JSON.stringify(name)}) not sent: no VERCEL_URL, so not running on forkrail`)
    return
  }
  if ((env.VERCEL_ENV || 'production') === 'development') {
    console.log(`[forkrail analytics] track(${JSON.stringify(name)}, ${JSON.stringify(properties ?? {})})`)
    return
  }
  const visitor = visitorOf(options.request || options.headers)
  const origin = /^https?:\/\//.test(host) ? host : `https://${host}`
  const body = {
    o: visitor.url || origin,
    ts: Date.now(),
    en: name,
    ed: properties,
    sdkn: NAME,
    sdkv: VERSION,
    ...(options.flags ? { f: Array.isArray(options.flags) ? { p: Object.fromEntries(options.flags.map((f) => [f, true])) } : { p: options.flags } } : {}),
  }
  const headers = { 'content-type': 'application/json', 'x-va-server': '1' }
  if (visitor.ip) headers['x-vercel-ip'] = visitor.ip
  if (visitor.agent) headers['user-agent'] = visitor.agent
  try {
    const res = await fetch(`${origin}/_forkrail/insights/event`, { method: 'POST', headers, body: JSON.stringify(body) })
    if (!res.ok && res.status !== 204) console.log(`[forkrail analytics] track(${JSON.stringify(name)}) answered ${res.status}`)
  } catch (e) {
    console.log(`[forkrail analytics] track(${JSON.stringify(name)}) failed: ${e && e.message}`)
  }
}

export default { track }
