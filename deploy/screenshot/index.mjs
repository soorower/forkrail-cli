// forkrail-screenshot: the dashboard's page previews. Given one URL of the install's own hosts, it
// opens it in headless Chromium at a desktop size and answers a JPEG of what's above the fold, as
// base64. The dashboard checks the host, keeps the image in S3 per deployment and never asks twice;
// only its role may invoke this (deploy/install/screenshot.yaml). It writes nothing anywhere.
//
// Event: { url, cookie?: { name, value }, width?, height?, timeoutMs? }
// Answer: { ok: true, image, status, finalUrl, title, ms } or { ok: false, error, ms }

import dns from 'node:dns/promises'
import net from 'node:net'

import chromium from '@sparticuz/chromium'
import puppeteer from 'puppeteer-core'

chromium.setGraphicsMode = false

const LOCAL = process.env.FORKRAIL_SCREENSHOT_CHROME // a local Chrome, to try this away from Lambda
let browser = null

async function launch() {
  if (browser?.connected) return browser
  browser = await puppeteer.launch(
    LOCAL
      ? { executablePath: LOCAL, headless: true }
      : {
          // Not single-process: that Chromium can't open a second browser context ("Target
          // closed"), and each shot has its own, so no deployment's token is left for the next.
          args: await puppeteer.defaultArgs({ args: chromium.args.filter((a) => a !== '--single-process'), headless: 'shell' }),
          executablePath: await chromium.executablePath(),
          headless: 'shell',
        },
  )
  return browser
}

// The page's own requests may go anywhere on the internet, never to this machine or a private
// network (the Lambda runtime API listens on 127.0.0.1:9001, over plain HTTP). So: HTTPS only,
// and every host is resolved here first and refused if any of its addresses is private, whatever
// its name says (127.0.0.1.nip.io). Chromium resolves again on its own, so a name that changes
// its answer in between could still slip through; the runtime API's plain HTTP can't. The role
// has no rights either.
const isPrivate = (ip) => {
  const v4 = ip.replace(/^::ffff:/i, '')
  if (net.isIPv4(v4)) {
    const [a, b] = v4.split('.').map(Number)
    return a === 0 || a === 10 || a === 127 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 100 && b >= 64 && b <= 127) || a >= 224
  }
  const x = ip.toLowerCase()
  return x === '::' || x === '::1' || /^f[cd]/.test(x) || /^fe[89ab]/.test(x) || x.startsWith('::ffff:')
}
const resolved = new Map()
export async function allowed(raw) {
  let u
  try {
    u = new URL(raw)
  } catch {
    return false
  }
  if (u.protocol === 'data:' || u.protocol === 'blob:') return true
  if (u.protocol !== 'https:') return false
  const host = u.hostname.replace(/^\[|\]$/g, '')
  if (net.isIP(host)) return !isPrivate(host)
  let ok = resolved.get(host)
  if (ok === undefined) {
    ok = dns.lookup(host, { all: true }).then((all) => all.length > 0 && all.every((a) => !isPrivate(a.address)), () => false)
    resolved.set(host, ok)
  }
  return ok
}

const clamp = (v, lo, hi, d) => (Number.isFinite(v) ? Math.min(hi, Math.max(lo, Math.round(v))) : d)

export async function handler(event) {
  const t0 = Date.now()
  let url
  try {
    url = new URL(String(event?.url ?? ''))
    if (url.protocol !== 'https:' && !(LOCAL && url.protocol === 'http:')) throw new Error('only https URLs')
  } catch (e) {
    return { ok: false, error: `not a URL: ${e.message}`, ms: 0 }
  }
  const width = clamp(event.width, 320, 1920, 1280)
  const height = clamp(event.height, 240, 1600, 800)
  const timeout = clamp(event.timeoutMs, 1000, 25_000, 15_000)
  let page
  resolved.clear()
  try {
    if (!LOCAL && !(await allowed(url.toString()))) throw new Error(`${url.hostname} isn't a public HTTPS address`)
    const b = await launch()
    const context = await b.createBrowserContext()
    page = await context.newPage()
    await page.setViewport({ width, height, deviceScaleFactor: 1 })
    if (!LOCAL) {
      await page.setRequestInterception(true)
      page.on('request', (r) => {
        allowed(r.url()).then(
          (ok) => (ok ? r.continue() : r.abort('blockedbyclient')).catch(() => {}),
          () => r.abort('blockedbyclient').catch(() => {}),
        )
      })
    }
    await page.setUserAgent({ userAgent: `${await b.userAgent()} forkrail-preview` })
    if (event.cookie?.name && event.cookie?.value) {
      await context.setCookie({ name: event.cookie.name, value: event.cookie.value, domain: url.hostname, path: '/', secure: true, httpOnly: true, sameSite: 'Lax' })
    }
    let response = null
    try {
      // Most pages are done by then; one that keeps a connection open is shot as it is.
      response = await page.goto(url.toString(), { waitUntil: 'networkidle2', timeout })
    } catch (e) {
      if (!/timeout/i.test(String(e?.message))) throw e
    }
    // Web fonts and late paints.
    await new Promise((done) => setTimeout(done, 400))
    const image = await page.screenshot({ type: 'jpeg', quality: 78, encoding: 'base64', captureBeyondViewport: false })
    const title = await page.title().catch(() => '')
    await context.close()
    return { ok: true, image, status: response?.status() ?? 0, finalUrl: page.url(), title, width, height, ms: Date.now() - t0 }
  } catch (e) {
    await page?.browserContext().close().catch(() => {})
    // A broken browser is started again next time.
    if (!browser?.connected) browser = null
    return { ok: false, error: String(e?.message ?? e).slice(0, 500), ms: Date.now() - t0 }
  }
}

// `node index.mjs URL > out.json`, with FORKRAIL_SCREENSHOT_CHROME set: the dashboard's dev path
// (FORKRAIL_SCREENSHOT_CMD) runs it so, reading the answer as JSON.
if (import.meta.url === `file://${process.argv[1]}`) {
  const input = process.argv[2]?.startsWith('{') ? JSON.parse(process.argv[2]) : { url: process.argv[2] }
  const out = await handler(input)
  process.stdout.write(JSON.stringify(out))
  await browser?.close()
}
