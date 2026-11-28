// What forkrail's server of Next's entrypoints (crates/runtime/src/next) needs from a project's
// `node_modules` beyond the entrypoints' own traced files, found with Next's own file tracer.
// Run by the CLI with the build's Node (next_handlers.rs):
//
//   node next_handlers.cjs '{"projectDir": "...", "repoRoot": "...", "edge": true, "instrumentation": false, "images": true}'
//
// Prints one line of JSON: `files` (paths from the repository's root), `links` (pnpm's link to
// `@next/env` beside `next`, which a trace doesn't list) and `routing` (an installed
// `@next/routing`'s folder and version, if the project has one).
'use strict'
const fs = require('node:fs')
const path = require('node:path')
const { createRequire } = require('node:module')

async function main() {
  const asked = JSON.parse(process.argv[2])
  const { projectDir, repoRoot } = asked
  const fromProject = createRequire(path.join(projectDir, 'package.json'))
  const nextPackage = fromProject.resolve('next/package.json')
  const fromNext = createRequire(nextPackage)
  const roots = [
    // `.env` files are read with Next's own reader when there are any.
    fromNext.resolve('@next/env'),
  ]
  // Edge entries run in Next's sandbox, which no function's trace has.
  if (asked.edge) roots.push(fromProject.resolve('next/dist/server/web/sandbox/index.js'))
  // `instrumentation.ts`'s `register()`, run at the start.
  if (asked.instrumentation) roots.push(fromProject.resolve('next/dist/server/lib/router-utils/instrumentation-globals.external.js'))
  // `/_next/image`, answered by Next's own optimizer as under its own server.
  if (asked.images) {
    for (const m of ['next/dist/server/image-optimizer.js', 'next/dist/server/response-cache/index.js', 'next/dist/server/serve-static.js']) roots.push(fromProject.resolve(m))
  }
  const { nodeFileTrace } = fromProject('next/dist/compiled/@vercel/nft')
  const traced = await nodeFileTrace(roots, { base: repoRoot, processCwd: projectDir, ignore: ['**/*.d.ts', '**/*.map'] })
  const links = []
  // Under pnpm `@next/env` is a link beside `next` (`.pnpm/next@x/node_modules/@next/env`).
  const beside = path.join(path.dirname(path.dirname(nextPackage)), '@next', 'env')
  if (fs.existsSync(beside) && fs.lstatSync(beside).isSymbolicLink()) links.push(path.relative(repoRoot, beside))
  let routing = null
  for (const from of [fromProject, fromNext]) {
    try {
      const file = from.resolve('@next/routing/package.json')
      routing = { dir: path.dirname(file), version: JSON.parse(fs.readFileSync(file, 'utf8')).version }
      break
    } catch {
      // Not installed there.
    }
  }
  process.stdout.write(JSON.stringify({ files: [...traced.fileList], links, routing }) + '\n')
}

main().catch((error) => {
  process.stderr.write(String((error && error.stack) || error) + '\n')
  process.exit(1)
})
