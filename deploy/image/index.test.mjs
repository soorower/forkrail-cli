// `deploy/aws/image.sh test`: the resizer against a folder laid out like the bucket.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, readFile, writeFile, mkdir } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { test } from 'node:test'

const dir = await mkdtemp(path.join(tmpdir(), 'forkrail-image-'))
process.env.FORKRAIL_IMAGE_DIR = dir
const { animated, fetchRemote, handler, immutable, isPrivate, maxAge, outputType, sniff } = await import('./index.mjs')
const sharp = (await import('sharp')).default

const sha = (b) => createHash('sha256').update(b).digest('hex')
let n = 0
const storeKey = () => `images/shop/${String(++n).padStart(40, '0')}`

/// A file in the folder as the CLI uploads one: `static/<sha256>`.
async function put(bytes) {
  const key = `static/${sha(bytes)}`
  await mkdir(path.join(dir, 'static'), { recursive: true })
  await writeFile(path.join(dir, key), bytes)
  return key
}

const photo = (width, height, format = 'png') =>
  sharp({ create: { width, height, channels: 3, background: { r: 200, g: 80, b: 40 } } })
    [format]()
    .toBuffer()
const event = (source, more = {}) => ({ source, width: 640, quality: 75, format: null, svg: false, minimumCacheTTL: 60, immutable: true, store: storeKey(), ...more })
const size = async (answer) => sharp(Buffer.from(answer.body, 'base64')).metadata()

test("a framework's hashed static file (immutable) is answered immutable, as Vercel does", async () => {
  const key = await put(await photo(100, 80))
  const a = await handler(event({ key }, { cacheControl: 'public, max-age=31536000, immutable' }))
  assert.equal(a.ok, true, a.message)
  assert.deepEqual([a.maxAge, a.revalidate, a.immutable], [31536000, true, true])
})

test('a static PNG resized to the width, as WebP, and kept', async () => {
  const key = await put(await photo(2000, 1500))
  const e = event({ key }, { format: 'image/webp' })
  const a = await handler(e)
  assert.equal(a.ok, true, a.message)
  assert.equal(a.contentType, 'image/webp')
  assert.equal(a.optimized, true)
  assert.equal(a.expires, 0, 'a static file is named by its content: never stale')
  assert.deepEqual([a.maxAge, a.revalidate, a.immutable], [0, true, false])
  const m = await size(a)
  assert.deepEqual([m.format, m.width, m.height], ['webp', 640, 480])
  const kept = JSON.parse(await readFile(path.join(dir, `${e.store}.meta.json`), 'utf8'))
  assert.equal(kept['content-type'], 'image/webp')
  assert.equal(kept['forkrail-expires'], '0')
  assert.equal((await readFile(path.join(dir, e.store))).length, a.bytes)
})

test('never enlarged; a PNG stays PNG without a format, and becomes a palette', async () => {
  const key = await put(await photo(64, 48))
  const a = await handler(event({ key }, { width: 1080 }))
  const m = await size(a)
  assert.deepEqual([a.contentType, m.width, m.height, m.paletteBitDepth > 0 || m.isPalette === true], ['image/png', 64, 48, true])
})

test('WebP and AVIF become JPEG for a client that takes neither; AVIF when asked', async () => {
  const webp = await put(await photo(800, 600, 'webp'))
  const a = await handler(event({ key: webp }))
  assert.equal(a.contentType, 'image/jpeg')
  assert.equal((await size(a)).width, 640)
  const avif = await handler(event({ key: webp }, { format: 'image/avif' }))
  assert.equal(avif.contentType, 'image/avif')
  assert.equal((await size(avif)).format, 'heif')
})

test("a function's answer, sent in the request, kept for its own max-age or minimumCacheTTL", async () => {
  const body = (await photo(1200, 900, 'jpeg')).toString('base64')
  const before = Math.floor(Date.now() / 1000)
  const a = await handler(event({ body }, { contentType: 'image/jpeg', cacheControl: 'public, max-age=3600', immutable: false, fetched: true, minimumCacheTTL: 60 }))
  assert.equal(a.contentType, 'image/jpeg')
  assert.deepEqual([a.maxAge, a.revalidate], [3600, false])
  assert.ok(a.expires >= before + 3600 && a.expires <= before + 3602)
  const b = await handler(event({ body }, { contentType: 'image/jpeg', cacheControl: 'public, max-age=0', immutable: false, fetched: true, minimumCacheTTL: 60 }))
  assert.deepEqual([b.maxAge, b.revalidate], [60, false], 'a fetched image says minimumCacheTTL, as Vercel does')
  assert.ok(b.expires >= before + 60 && b.expires <= before + 62)
})

test('what isn\'t an image, or an SVG not allowed, is refused; what Vercel doesn\'t optimize goes as it is', async () => {
  const text = await put(Buffer.from('User-agent: *\n'))
  assert.equal((await handler(event({ key: text }))).code, 'NOT_AN_IMAGE')
  const declared = await put(await photo(10, 10))
  assert.equal((await handler(event({ key: declared }, { contentType: 'text/plain' }))).code, 'NOT_AN_IMAGE')
  const svg = Buffer.from('<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>')
  assert.equal((await handler(event({ key: await put(svg) }))).code, 'NOT_AN_IMAGE')
  const allowed = await handler(event({ key: await put(svg) }, { svg: true }))
  assert.deepEqual([allowed.ok, allowed.contentType, allowed.optimized, Buffer.from(allowed.body, 'base64').equals(svg)], [true, 'image/svg+xml', false, true])
  const gif = Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64')
  const g = await handler(event({ key: await put(gif) }, { format: 'image/webp' }))
  assert.deepEqual([g.contentType, g.optimized], ['image/gif', false])
  assert.equal((await handler(event({ key: `static/${'0'.repeat(64)}` }))).code, 'BAD_UPSTREAM')
})

test('too big a source is refused; keys only where the router puts them', async () => {
  const wide = await put(await photo(9000, 10))
  assert.equal((await handler(event({ key: wide }))).code, 'NOT_AN_IMAGE')
  assert.equal((await handler(event({ key: '../etc/passwd' }))).code, 'BAD_REQUEST')
  assert.equal((await handler(event({ key: await put(await photo(10, 10)) }, { store: 'static/x' }))).code, 'BAD_REQUEST')
  assert.equal((await handler(event({ key: 'x' }, { width: 0 }))).code, 'BAD_REQUEST')
})

test('types, animations, ages and output types', () => {
  assert.equal(sniff(Buffer.from('\x89PNG\r\n\x1a\n....', 'latin1')), 'image/png')
  assert.equal(sniff(Buffer.from('<!--ftypavif-->')), null)
  // An APNG: acTL before IDAT. A WebP with VP8X's animation flag.
  const chunk = (kind, len = 0) => Buffer.concat([Buffer.from([0, 0, 0, len]), Buffer.from(kind), Buffer.alloc(len + 4)])
  const png = (kinds) => Buffer.concat([Buffer.from('\x89PNG\r\n\x1a\n', 'latin1'), ...kinds.map((k) => chunk(k, k === 'IHDR' ? 13 : 0))])
  assert.equal(animated(png(['IHDR', 'acTL', 'IDAT']), 'image/png'), true)
  assert.equal(animated(png(['IHDR', 'IDAT']), 'image/png'), false)
  const webp = (flags) => Buffer.concat([Buffer.from('RIFF\0\0\0\0WEBPVP8X', 'latin1'), Buffer.from([10, 0, 0, 0, flags])])
  assert.equal(animated(webp(0x02), 'image/webp'), true)
  assert.equal(animated(webp(0x10), 'image/webp'), false)
  assert.equal(maxAge('public, max-age=0, must-revalidate'), 0)
  assert.equal(immutable('public, max-age=31536000, immutable'), true)
  assert.equal(immutable('public, max-age=0, must-revalidate'), false)
  assert.equal(maxAge('max-age=10, s-maxage=60'), 60)
  assert.equal(maxAge(null), 0)
  assert.equal(maxAge('max-age=99999999999999999999'), 315_360_000)
  assert.equal(outputType('image/webp', null), 'image/jpeg')
  assert.equal(outputType('image/png', null), 'image/png')
  assert.equal(outputType('image/png', 'image/avif'), 'image/avif')
})

test('the fetch elsewhere refuses this machine and private networks before connecting', async () => {
  for (const ip of ['127.0.0.1', '10.1.2.3', '172.16.0.1', '192.168.1.1', '169.254.169.254', '100.64.0.1', '0.0.0.0', '::1', 'fd00::1', '::ffff:127.0.0.1']) {
    assert.equal(isPrivate(ip), true, ip)
  }
  for (const ip of ['8.8.8.8', '1.1.1.1', '2606:4700::1111']) assert.equal(isPrivate(ip), false, ip)
  for (const url of ['http://127.0.0.1:9001/2018-06-01/runtime/invocation/next', 'http://localhost/a.png', 'http://[::1]/a.png', 'http://169.254.169.254/latest/meta-data']) {
    await assert.rejects(fetchRemote(url), (e) => e.code === 'OPTIMIZED_EXTERNAL_IMAGE_REQUEST_FAILED', url)
  }
  await assert.rejects(fetchRemote('ftp://example.com/a.png'), (e) => e.code === 'OPTIMIZED_EXTERNAL_IMAGE_REQUEST_FAILED')
})
