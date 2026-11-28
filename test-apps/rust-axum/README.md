# rust-axum

An axum web server listening on `PORT`, built for Linux arm64 (Amazon Linux 2023's glibc) and run
on `provided.al2023` with forkrail's runtime as its `bootstrap`.

- `GET /`: the architecture
- `POST /echo`: the body back, and a line in the logs
- `GET /env/NAME`: an env var's value
