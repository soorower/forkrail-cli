// forkrail-image: Image Optimization's resizer (Vercel's `/_vercel/image`). The router checks each
// request as Vercel does (crates/common/src/images.rs), keeps what this makes, and invokes it only
// on a miss: it's never reachable from the internet (deploy/install/image.yaml lets the routers'
// role alone invoke it). Given a source and what to make of it, it resizes and re-encodes it as
// Next.js's optimizer does, which Vercel's follows: turned by its EXIF orientation, scaled to the
// width without enlarging, AVIF quality scaled to look like WebP's, PNG to a palette. What Vercel
// doesn't optimize (anything but JPEG, PNG, WebP and AVIF, and animations) is passed on as it is.
// The result goes to the install's one store of optimized images (the home bucket's images/, for
// every region's routers) under the key the router chose, and back to the router in base64 when it
// fits in Lambda's 6 MB answer.
//
// Event: { source: { key } | { url } | { body }, contentType?, cacheControl?, width, quality,
//          format?, svg?, minimumCacheTTL, immutable?, fetched?, store }
//   key: a static file (`static/<sha256>`)
//   url: an image elsewhere, fetched here behind an address guard
//   body: a function's answer, in base64
// Answer: { ok: true, contentType, bytes, maxAge, revalidate, immutable, expires, created, optimized, body? }
//   maxAge, revalidate, immutable: the answer's Cache-Control as Vercel words it: a deployment's
//   file's own max-age with must-revalidate, or with immutable when its own Cache-Control says
//   immutable (a framework's hashed assets); for a fetched source (a function's, one elsewhere: `fetched`, or
//   any `url`) the longer of its own and minimumCacheTTL, without
//      or { ok: false, status, code, message }; `NOT_AN_IMAGE` and `BAD_UPSTREAM` are the router's
//         to word as Vercel does (400, 404)
//
// On Lambda it reads sources from its region's bucket, FORKRAIL_BUCKET (static/*), and writes
// results to the home bucket, FORKRAIL_STORE_BUCKET in FORKRAIL_STORE_REGION (images/*; both
// default to its own), and its role allows no more. Locally, `node index.mjs serve [port]`
// answers the same JSON over HTTP, with a folder of the bucket's layout (FORKRAIL_IMAGE_DIR, the
// router's FORKRAIL_STATIC_DIR) in place of the bucket.

import dns from 'node:dns/promises'
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import http from 'node:http'
import https from 'node:https'
import net from 'node:net'
import path from 'node:path'
import zlib from 'node:zlib'

import sharp from 'sharp'

// Only the decoders for what's optimized: libvips can read many formats, and each is code an
// image from anywhere would reach.
sharp.block({ operation: ['VipsForeignLoad'] })
sharp.unblock({ operation: ['VipsForeignLoadJpeg', 'VipsForeignLoadPng', 'VipsForeignLoadWebp', 'VipsForeignLoadHeif'] })
// One image per invocation: libvips's cache of decoded images would only hold memory.
sharp.cache(false)

const BUCKET = process.env.FORKRAIL_BUCKET
/// Where results are kept: the home bucket, one store for every region (Vercel's shared cache).
const STORE_BUCKET = process.env.FORKRAIL_STORE_BUCKET || BUCKET
const STORE_REGION = process.env.FORKRAIL_STORE_REGION || undefined
const DIR = process.env.FORKRAIL_IMAGE_DIR
/// What Vercel optimizes; anything else is served as it is.
const OPTIMIZED = ['image/jpeg', 'image/png', 'image/webp', 'image/avif']
const FORMATS = ['image/avif', 'image/webp', 'image/jpeg', 'image/png']
/// The widest or tallest source Vercel optimizes.
const MAX_SIDE = 8192
/// Answers up to this size go back in the invocation's answer (base64 makes 5.4 MB of it); larger
/// ones the router reads from the bucket.
const MAX_INLINE = 4 << 20
/// A source elsewhere is read up to this size, in this long, through this many redirects.
const MAX_SOURCE = 50 << 20
const FETCH_TIMEOUT_MS = 10_000
const MAX_REDIRECTS = 3
/// As Next.js's optimizer: past this, the source is served as it is.
const TRANSFORM_TIMEOUT_S = 7

const SOURCE_KEY = /^static\/[0-9a-f]{64}$/
const STORE_KEY = /^images\/[A-Za-z0-9._-]{1,100}\/[0-9a-f]{40}$/

class Refused extends Error {
  constructor(status, code, message) {
    super(message)
    this.status = status
    this.code = code
  }
}
const notAnImage = (why) => new Refused(400, 'NOT_AN_IMAGE', why)
const badUpstream = (status) => new Refused(404, 'BAD_UPSTREAM', `the source answered ${status}`)
const externalFailed = (why) => new Refused(502, 'OPTIMIZED_EXTERNAL_IMAGE_REQUEST_FAILED', why)

/// The type an image's first bytes say it is (Next.js's `detectContentType`, the formats that
/// matter), or null.
export function sniff(b) {
  const starts = (sig, at = 0) => b.length >= at + sig.length && sig.every((x, i) => b[at + i] === x)
  const ftyp = (brand) => {
    if (b.length < 12) return false
    const size = b.readUInt32BE(0)
    return size >= 16 && size <= b.length && starts([0x66, 0x74, 0x79, 0x70], 4) && starts([...brand].map((c) => c.charCodeAt(0)), 8)
  }
  if (starts([0xff, 0xd8, 0xff])) return 'image/jpeg'
  if (starts([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return 'image/png'
  if (starts([0x47, 0x49, 0x46, 0x38])) return 'image/gif'
  if (starts([0x52, 0x49, 0x46, 0x46]) && starts([0x57, 0x45, 0x42, 0x50], 8)) return 'image/webp'
  if (starts([0x3c, 0x3f, 0x78, 0x6d, 0x6c]) || starts([0x3c, 0x73, 0x76, 0x67])) return 'image/svg+xml'
  if (ftyp('avif')) return 'image/avif'
  if (starts([0, 0, 1, 0])) return 'image/x-icon'
  if (starts([0x69, 0x63, 0x6e, 0x73])) return 'image/x-icns'
  if (starts([0x49, 0x49, 0x2a, 0]) || starts([0x4d, 0x4d, 0, 0x2a])) return 'image/tiff'
  if (starts([0x42, 0x4d])) return 'image/bmp'
  if (starts([0xff, 0x0a]) || starts([0, 0, 0, 0x0c, 0x4a, 0x58, 0x4c, 0x20, 0x0d, 0x0a, 0x87, 0x0a])) return 'image/jxl'
  if (ftyp('heic')) return 'image/heic'
  return null
}

/// Whether a PNG or WebP holds frames to play: an APNG's `acTL` chunk before its image data, a
/// WebP's animation flag. Those are passed on as they are, as Vercel does.
export function animated(b, type) {
  if (type === 'image/png') {
    let at = 8
    while (at + 8 <= b.length) {
      const len = b.readUInt32BE(at)
      const kind = b.toString('latin1', at + 4, at + 8)
      if (kind === 'acTL') return true
      if (kind === 'IDAT') return false
      at += 12 + len
    }
    return false
  }
  if (type === 'image/webp') return b.toString('latin1', 12, 16) === 'VP8X' && (b[20] & 0x02) !== 0
  return false
}

/// Whether a source's Cache-Control says `immutable`.
export function immutable(cacheControl) {
  return String(cacheControl ?? '').split(',').some((d) => d.trim().toLowerCase() === 'immutable')
}

/// How long a source says it may be kept: `s-maxage`, else `max-age`, else 0 (Next.js's getMaxAge).
export function maxAge(cacheControl) {
  const found = {}
  for (const directive of String(cacheControl ?? '').split(',')) {
    const [key, value] = directive.trim().split('=', 2)
    if (value !== undefined) found[key.toLowerCase()] = value.trim().replace(/^"|"$/g, '')
  }
  const n = parseInt(found['s-maxage'] ?? found['max-age'] ?? '', 10)
  // At most ten years: a source's 1e20 would be written as no integer the router reads.
  return Number.isFinite(n) && n > 0 ? Math.min(n, MAX_AGE_S) : 0
}

/// The longest a source may ask to be kept (the router's own ceiling).
export const MAX_AGE_S = 315_360_000

/// The output type: what Accept chose, else the source's, with WebP and AVIF made JPEG for a
/// client that named neither (as Next.js's optimizer does).
export function outputType(source, format) {
  if (format) return format
  if (source === 'image/webp' || source === 'image/avif') return 'image/jpeg'
  return source
}

/// Resize and encode, as Next.js's `optimizeImage`.
export async function transform(buffer, { width, quality, type }) {
  const t = sharp(buffer, { sequentialRead: true }).timeout({ seconds: TRANSFORM_TIMEOUT_S }).rotate().resize(width, undefined, { withoutEnlargement: true })
  if (type === 'image/avif') t.avif({ quality: Math.max(Math.round(quality * (50 / 80)), 1), effort: 3 })
  else if (type === 'image/webp') t.webp({ quality })
  else if (type === 'image/png') t.png({ quality })
  else if (type === 'image/jpeg') t.jpeg({ quality, mozjpeg: true })
  return t.toBuffer()
}

// The fetch elsewhere goes anywhere on the internet, never to this machine or a private network:
// the Lambda runtime API listens on 127.0.0.1:9001 over plain HTTP. Every name is resolved here,
// refused if any of its addresses is private, and connected to at the address checked, so a name
// can't change its answer in between (deploy/screenshot's guard, pinned).
export const isPrivate = (ip) => {
  const v4 = ip.replace(/^::ffff:/i, '')
  if (net.isIPv4(v4)) {
    const [a, b] = v4.split('.').map(Number)
    return a === 0 || a === 10 || a === 127 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 100 && b >= 64 && b <= 127) || a >= 224
  }
  const x = ip.toLowerCase()
  return x === '::' || x === '::1' || /^f[cd]/.test(x) || /^fe[89ab]/.test(x) || x.startsWith('::ffff:') || x.startsWith('64:ff9b:')
}

async function publicAddress(host) {
  const bare = host.replace(/^\[|\]$/g, '')
  const all = net.isIP(bare) ? [{ address: bare, family: net.isIP(bare) }] : await dns.lookup(bare, { all: true }).catch(() => [])
  if (all.length === 0) throw externalFailed(`${host} doesn't resolve`)
  if (all.some((a) => isPrivate(a.address))) throw externalFailed(`${host} isn't a public address`)
  return all[0]
}

/// GET an image elsewhere: its status, headers and body, redirects followed (each one guarded).
/// `loop` is the loop guard's mark, which the router gives only for a URL on one of the install's
/// own hosts (crates/router/src/loops.rs); a redirect to any other host goes without it.
export async function fetchRemote(raw, redirects = 0, loop = null) {
  let url
  try {
    url = new URL(raw)
  } catch {
    throw externalFailed('not a URL')
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') throw externalFailed('only http and https')
  const address = await publicAddress(url.hostname)
  const client = url.protocol === 'https:' ? https : http
  const res = await new Promise((resolve, reject) => {
    const req = client.request(
      url,
      {
        method: 'GET',
        headers: {
          accept: 'image/avif,image/webp,image/png,image/jpeg,*/*;q=0.8',
          'accept-encoding': 'gzip, deflate, br',
          'user-agent': 'forkrail-image',
          ...(typeof loop === 'string' && loop ? { 'x-forkrail-loop': loop } : {}),
        },
        lookup: (_name, options, done) => (options?.all ? done(null, [address]) : done(null, address.address, address.family)),
        timeout: FETCH_TIMEOUT_MS,
      },
      resolve,
    )
    req.on('timeout', () => req.destroy(externalFailed('the source took too long')))
    req.on('error', (e) => reject(e instanceof Refused ? e : externalFailed(e.message)))
    req.end()
  })
  if ([301, 302, 303, 307, 308].includes(res.statusCode)) {
    res.resume()
    if (redirects >= MAX_REDIRECTS) throw externalFailed('too many redirects')
    const next = res.headers.location
    if (!next) throw badUpstream(res.statusCode)
    const to = new URL(next, url)
    return fetchRemote(to.toString(), redirects + 1, to.hostname === url.hostname ? loop : null)
  }
  const chunks = []
  let size = 0
  for await (const chunk of res) {
    size += chunk.length
    if (size > MAX_SOURCE) {
      res.destroy()
      throw externalFailed(`the source is over ${MAX_SOURCE >> 20} MB`)
    }
    chunks.push(chunk)
  }
  let body = Buffer.concat(chunks)
  const encoding = String(res.headers['content-encoding'] ?? '').toLowerCase()
  const options = { maxOutputLength: MAX_SOURCE }
  if (encoding === 'gzip' || encoding === 'x-gzip') body = zlib.gunzipSync(body, options)
  else if (encoding === 'deflate') body = zlib.inflateSync(body, options)
  else if (encoding === 'br') body = zlib.brotliDecompressSync(body, options)
  return { status: res.statusCode, headers: res.headers, body }
}

let s3 = null
async function bucket() {
  if (!s3) {
    const { S3Client, GetObjectCommand, PutObjectCommand } = await import('@aws-sdk/client-s3')
    // The store's client names the home bucket's region: S3 refuses a write sent to another.
    const client = new S3Client({})
    const storeClient = STORE_REGION ? new S3Client({ region: STORE_REGION }) : client
    s3 = { client, storeClient, GetObjectCommand, PutObjectCommand }
  }
  return s3
}

async function readKey(key) {
  if (!SOURCE_KEY.test(key)) throw new Refused(400, 'BAD_REQUEST', `${key} isn't a source the router names`)
  if (BUCKET) {
    const { client, GetObjectCommand } = await bucket()
    try {
      const out = await client.send(new GetObjectCommand({ Bucket: BUCKET, Key: key }))
      return Buffer.from(await out.Body.transformToByteArray())
    } catch (e) {
      if (e?.name === 'NoSuchKey' || e?.$metadata?.httpStatusCode === 404) throw badUpstream(404)
      throw e
    }
  }
  if (!DIR) throw new Error('neither FORKRAIL_BUCKET nor FORKRAIL_IMAGE_DIR is set')
  return readFile(path.join(DIR, key)).catch((e) => {
    throw e.code === 'ENOENT' ? badUpstream(404) : e
  })
}

async function store(key, body, contentType, meta) {
  if (!STORE_KEY.test(key)) throw new Refused(400, 'BAD_REQUEST', `${key} isn't where results are kept`)
  const metadata = Object.fromEntries(Object.entries(meta).map(([k, v]) => [k, String(v)]))
  if (STORE_BUCKET) {
    const { storeClient, PutObjectCommand } = await bucket()
    await storeClient.send(new PutObjectCommand({ Bucket: STORE_BUCKET, Key: key, Body: body, ContentType: contentType, Metadata: metadata }))
    return
  }
  const file = path.join(DIR, key)
  await mkdir(path.dirname(file), { recursive: true })
  // Written aside and renamed, so the router never reads half of one.
  await writeFile(`${file}.meta.json.tmp`, JSON.stringify({ 'content-type': contentType, ...metadata }))
  await writeFile(`${file}.tmp`, body)
  await rename(`${file}.meta.json.tmp`, `${file}.meta.json`)
  await rename(`${file}.tmp`, file)
}

function check(event) {
  const int = (v, lo, hi) => Number.isInteger(v) && v >= lo && v <= hi
  if (!event || typeof event !== 'object') throw new Refused(400, 'BAD_REQUEST', 'no event')
  if (!int(event.width, 1, 16384)) throw new Refused(400, 'BAD_REQUEST', 'width: 1 to 16384')
  if (!int(event.quality, 1, 100)) throw new Refused(400, 'BAD_REQUEST', 'quality: 1 to 100')
  if (event.format != null && !FORMATS.includes(event.format)) throw new Refused(400, 'BAD_REQUEST', `format: one of ${FORMATS.join(', ')}`)
  if (typeof event.store !== 'string') throw new Refused(400, 'BAD_REQUEST', 'store: the key to keep it at')
  const s = event.source ?? {}
  if ([s.key, s.url, s.body].filter((v) => typeof v === 'string').length !== 1) throw new Refused(400, 'BAD_REQUEST', 'source: one of key, url, body')
}

/// Make one image. Errors the router words itself are answered, not thrown.
export async function handler(event) {
  try {
    check(event)
    const { source } = event
    let buffer
    let declared = event.contentType ?? null
    let cacheControl = event.cacheControl ?? null
    if (source.key) buffer = await readKey(source.key)
    else if (source.body) buffer = Buffer.from(source.body, 'base64')
    else {
      const got = await fetchRemote(source.url, 0, event.loop ?? null)
      if (got.status < 200 || got.status > 299) throw badUpstream(got.status)
      declared = String(got.headers['content-type'] ?? '')
      cacheControl = got.headers['cache-control'] ?? null
      buffer = got.body
    }
    // Vercel needs a type that says image, and so does what the bytes say they are.
    if (declared !== null && !declared.toLowerCase().trim().startsWith('image/')) throw notAnImage(`the source's type is ${declared || 'missing'}`)
    const type = sniff(buffer)
    if (!type) throw notAnImage("the source isn't an image")
    if (type === 'image/svg+xml' && !event.svg) throw notAnImage('SVG needs dangerouslyAllowSVG')
    const created = Math.floor(Date.now() / 1000)
    const own = maxAge(cacheControl)
    const kept = Math.max(own, Number(event.minimumCacheTTL) || 0)
    const fetched = Boolean(event.fetched || source.url)
    const age = fetched ? kept : own
    const expires = event.immutable ? 0 : created + kept
    let out = buffer
    let contentType = type
    let optimized = false
    if (OPTIMIZED.includes(type) && !animated(buffer, type)) {
      const { width = 0, height = 0 } = await sharp(buffer).metadata().catch(() => ({}))
      if (width > MAX_SIDE || height > MAX_SIDE) throw notAnImage(`the source is ${width}x${height}, over ${MAX_SIDE} pixels a side`)
      const wanted = outputType(type, event.format)
      try {
        out = await transform(buffer, { width: event.width, quality: event.quality, type: wanted })
        contentType = wanted
        optimized = true
      } catch (e) {
        // As Next.js does: what can't be resized in time is served as it is.
        console.error(`resizing failed, serving the source as it is: ${e?.message ?? e}`)
      }
    }
    const fixed = !fetched && immutable(cacheControl)
    const meta = { 'forkrail-created': created, 'forkrail-expires': expires, 'forkrail-max-age': age, 'forkrail-revalidate': fetched ? 0 : 1, 'forkrail-immutable': fixed ? 1 : 0, 'forkrail-optimized': optimized ? 1 : 0 }
    await store(event.store, out, contentType, meta)
    const answer = { ok: true, contentType, bytes: out.length, maxAge: age, revalidate: !fetched, immutable: fixed, expires, created, optimized }
    if (out.length <= MAX_INLINE) answer.body = out.toString('base64')
    return answer
  } catch (e) {
    if (e instanceof Refused) return { ok: false, status: e.status, code: e.code, message: e.message }
    console.error(e)
    return { ok: false, status: 502, code: 'ROUTER_IMAGE_OPTIMIZER_FAILED', message: String(e?.message ?? e).slice(0, 300) }
  }
}

// `node index.mjs serve [port]`: the router's FORKRAIL_IMAGE_URL for local runs, the same JSON
// over HTTP. FORKRAIL_IMAGE_DIR is the router's FORKRAIL_STATIC_DIR.
if (import.meta.url === `file://${process.argv[1]}` && process.argv[2] === 'serve') {
  if (!DIR && !BUCKET) throw new Error('set FORKRAIL_IMAGE_DIR to the router\'s FORKRAIL_STATIC_DIR')
  const port = Number(process.argv[3] ?? 3090)
  http
    .createServer(async (req, res) => {
      const chunks = []
      for await (const c of req) chunks.push(c)
      let answer
      let event = {}
      const started = Date.now()
      try {
        event = JSON.parse(Buffer.concat(chunks).toString('utf8'))
        answer = await handler(event)
      } catch (e) {
        answer = { ok: false, status: 400, code: 'BAD_REQUEST', message: String(e?.message ?? e) }
      }
      // One line a call, as the router's log has one a request.
      const source = event.source?.key ?? event.source?.url ?? (event.source?.body ? 'body' : '?')
      console.error(`${new Date().toISOString()} ${source} w=${event.width} q=${event.quality} ${event.format ?? 'auto'} -> ${answer.ok ? `${answer.contentType} ${answer.bytes} B` : answer.code} ${Date.now() - started} ms`)
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify(answer))
    })
    .listen(port, '127.0.0.1', () => console.error(`forkrail-image on http://127.0.0.1:${port}/ (${DIR ?? BUCKET})`))
}
