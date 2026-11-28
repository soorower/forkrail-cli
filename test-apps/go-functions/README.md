# go-functions

Vercel's zero-config Go layout: every file in `api/` is a function with one exported
`func(http.ResponseWriter, *http.Request)`. forkrail builds them into one program with a
generated dispatcher; the router's routes say which function a request is for.

- `GET /api` (`api/index.go`): Go's version, and a helper from `api/_lib` (not a function)
- `GET /api/hello?name=ada`, `POST /api/hello` (`api/hello.go`): same package and handler name
  as `api/index.go`
- `GET /api/users/42` (`api/users/[id].go`): `id` in the query
- `GET /api/nope`: 404
- `GET /`: `public/index.html`, served by the router
