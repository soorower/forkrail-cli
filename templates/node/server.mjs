// A plain node:http server: forkrail runs it as it is, listening on PORT. Express, Hono and
// Fastify work the same way (listen on process.env.PORT).

import http from 'node:http'

const json = (res, status, body) => {
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8' })
  res.end(JSON.stringify(body, null, 2))
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://localhost')
  if (url.pathname === '/') {
    return json(res, 200, {
      message: 'Hello from Node.js on forkrail',
      node: process.version,
      region: process.env.AWS_REGION ?? null,
      time: new Date().toISOString(),
    })
  }
  if (url.pathname === '/health') return json(res, 200, { ok: true })
  const hello = url.pathname.match(/^\/hello\/([^/]+)$/)
  if (hello) {
    let name
    try {
      name = decodeURIComponent(hello[1])
    } catch {
      return json(res, 400, { error: 'Not a name', path: url.pathname })
    }
    return json(res, 200, { hello: name })
  }
  json(res, 404, { error: 'Not found', path: url.pathname })
})

server.listen(Number(process.env.PORT ?? 3000), () => {
  console.log(`listening on ${server.address().port}`)
})
