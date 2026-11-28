# node-express

An Express app in TypeScript (`src/index.ts`) that exports its app, as Vercel's Express preset
takes it. `engines.node` is `22.x`, so it runs on Lambda's `nodejs22.x`.

- `GET /?name=ada`: the framework, Node version and a greeting from another module
- `POST /echo`: a JSON body back, and a line in the logs
- `GET /items/42`: a route with a parameter
