# node-hono

A Hono app in TypeScript that exports itself, served through its `fetch` (ES module, Node 24).

- `GET /`: the framework and Node version
- `POST /echo`: the body back
- `GET /cookies`: two `set-cookie` headers
