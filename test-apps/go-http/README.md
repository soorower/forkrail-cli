# go-http

A Go web server (`net/http`) listening on `PORT`, built for Linux arm64 and run on
`provided.al2023` with forkrail's runtime as its `bootstrap`.

- `GET /`: Go's version and architecture
- `POST /echo`: the body back, and a line in the logs
- `GET /sleep`: 200 ms, for concurrency
- `GET /env/NAME`: an env var's value (the sealed env file included)
