# node-functions

Vercel's zero-config Node functions beside a static site (Vite): every `.js`, `.mjs`, `.ts` and
`.tsx` file in `api/` is a function, served at its path without the extension. forkrail runs them
all in one Node app (the runtime's shim dispatches by the router's routes).

- `GET /`: the Vite site, which calls `/api/hello`
- `GET /api/hello?name=ada`, `GET /hello/ada` (a rewrite): `(req, res)` with Vercel's helpers, a
  module from `api/_lib` (not a function) and an npm package (`ms`)
- `GET /api/users`, `GET /api/users/42`: an `index` file and a dynamic segment (`id` in the query);
  `functions` gives `api/users/**` 512 MB
- `GET /api/posts/hello`: `[...slug]`, which Vercel's zero-config routes match as one segment, in
  the query as `...slug`
- `GET /api/web`, `POST /api/web`: Web handlers per method, the GET one streamed; `PUT` is 405
- `GET /api/fetch`: `export default { fetch }`
- `GET /api/edge`: `config.runtime = 'edge'`, a default export that takes a Request
- `POST /api/echo` (JSON, form or text), with cookies: `req.body`, `req.cookies`, `res.send`
- `GET /api/redirect`: `res.redirect` (307)
- `GET /api/cron`: the cron (`*/5 * * * *`); 401 without `Authorization: Bearer $CRON_SECRET`
- `GET /api/slow?ms=3000`: `config.maxDuration = 2` in the file, so 504 after 2 s
- `GET /api/nope`: 404
