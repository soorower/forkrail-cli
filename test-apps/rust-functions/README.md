# rust-functions

Vercel's Rust layout: each file in `api/` is a `[[bin]]` in `Cargo.toml`, written with Vercel's
`vercel_runtime` crate (2.x), unchanged. forkrail's runtime runs each as its own program (the
crate serves HTTP on `VERCEL_DEV_PORT`) and sends it its function's requests.

- `GET /api/hello` (`api/hello.rs`)
- `GET /api/users/42` (`api/users/[id].rs`): `id` in the query
- `GET /api/nope`: 404
