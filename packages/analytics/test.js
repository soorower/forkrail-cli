// node --test test.js: the browser half against a stand-in page, the server half against a
// stand-in router.
import { test } from 'node:test'
import assert from 'node:assert/strict'
import http from 'node:http'

import { computeRoute, inject, pageview, track } from './index.js'
import { track as serverTrack } from './server.js'

test('computeRoute names the dynamic segments', () => {
  assert.equal(computeRoute('/blog/hello', { slug: 'hello' }), '/blog/[slug]')
  assert.equal(computeRoute('/docs/a/b', { path: ['a', 'b'] }), '/docs/[...path]')
  assert.equal(computeRoute('/', {}), '/')
  assert.equal(computeRoute(null, null), null)
})

test('inject adds the script once and queues calls until it loads', () => {
  const added = []
  globalThis.window = {}
  globalThis.document = {
    head: { querySelector: (sel) => added.find((s) => sel.includes(s.src)) ?? null, appendChild: (s) => added.push(s) },
    createElement: () => ({ dataset: {} }),
  }
  inject({ mode: 'production', disableAutoTrack: true })
  inject({ mode: 'production' })
  assert.equal(added.length, 1)
  assert.equal(added[0].src, '/_forkrail/insights/script.js')
  assert.equal(added[0].dataset.sdkn, '@forkrail/analytics')
  assert.equal(added[0].dataset.disableAutoTrack, '1')
  assert.equal(window.vam, 'production')
  track('Signup', { plan: 'pro' })
  pageview({ route: '/blog/[slug]', path: '/blog/hello' })
  assert.deepEqual(window.vaq, [
    ['event', { name: 'Signup', data: { plan: 'pro' }, options: undefined }],
    ['pageview', { route: '/blog/[slug]', path: '/blog/hello' }],
  ])
  delete globalThis.window
  delete globalThis.document
})

test("the server's track names the visitor and says it's a server's", async () => {
  const got = []
  const server = http.createServer((req, res) => {
    let body = ''
    req.on('data', (d) => (body += d))
    req.on('end', () => {
      got.push({ url: req.url, headers: req.headers, body: JSON.parse(body) })
      res.writeHead(204).end()
    })
  })
  await new Promise((done) => server.listen(0, '127.0.0.1', done))
  process.env.VERCEL_URL = `http://127.0.0.1:${server.address().port}`
  const request = new Request('https://shop.example.com/checkout', { headers: { 'x-forwarded-for': '203.0.113.9, 10.0.0.1', 'user-agent': 'Mozilla/5.0 Test' } })
  await serverTrack('Checkout', { total: 42 }, { request, flags: ['new-checkout'] })
  server.close()
  delete process.env.VERCEL_URL
  assert.equal(got.length, 1)
  assert.equal(got[0].url, '/_forkrail/insights/event')
  assert.equal(got[0].headers['x-va-server'], '1')
  assert.equal(got[0].headers['x-vercel-ip'], '203.0.113.9')
  assert.equal(got[0].headers['user-agent'], 'Mozilla/5.0 Test')
  assert.equal(got[0].body.o, 'https://shop.example.com/checkout')
  assert.equal(got[0].body.en, 'Checkout')
  assert.deepEqual(got[0].body.ed, { total: 42 })
  assert.deepEqual(got[0].body.f, { p: { 'new-checkout': true } })
})
