// Entry of forkrail's Next.js server ("H"): `node --import shim.mjs server.mjs`, where the
// package's `server.mjs` imports this file. Reads the build's manifest (`forkrail-next.json`,
// written by forkrail's Next adapter at build), sets up Node the way Next's entrypoints expect,
// and listens. See server.mjs for what a request goes through.
//
// What the start does before the first request (server.mjs, "the start"): under forkrail's
// runtime, priming, which the runtime asks for once the server listens and waits for before it
// calls the worker ready (shim.mjs, `forkrail.prime`); elsewhere, or with FORKRAIL_PRIME=0, the
// entrypoints every request of a kind needs, loaded before listening.
//
// Environment (all optional):
//   FORKRAIL_PRIME=0          no priming (the project's env vars; read by the runtime too)
//   FORKRAIL_PRIME_PATHS      pages to render once at the start, as a visitor of the
//                             deployment's own URL would ask for them (`/,/pricing`, at most 8):
//                             only for pages safe to render with no visitor
//   FORKRAIL_NEXT_HOST        the address to listen on (default 127.0.0.1)
//   FORKRAIL_NEXT_DEBUG=1     a line when listening, with the start's timings
const T0 = performance.now();
import fs from 'node:fs';
import http from 'node:http';
import { createRequire } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const marks = [['node', T0]];
const mark = (name) => marks.push([name, performance.now()]);
const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.dirname(here);
const manifest = JSON.parse(fs.readFileSync(path.join(root, 'forkrail-next.json'), 'utf8'));
const projectDir = path.join(root, manifest.projectDir);

// This server is built against Next's internals, which move: it runs only the release lines it
// has passed Next's deploy tests on (verified.json), with the `@next/routing` of exactly the
// app's Next version. The build decides that (anything else is packaged with Next's own
// server); a package that got here otherwise is a packaging mistake, refused at start, where a
// deployment's warm-up meets it before any visitor does. FORKRAIL_NEXT_UNVERIFIED=1 runs it
// anyway (for trying a new Next release against the test suite).
const { versionVerdict, primePaths, primeVisitor, PRELOAD_MEMORY_PCT } = await import('./routing.mjs');
const verified = JSON.parse(fs.readFileSync(path.join(here, 'verified.json'), 'utf8')).next;
const routingVersion = JSON.parse(fs.readFileSync(path.join(here, 'node_modules', '@next', 'routing', 'package.json'), 'utf8')).version;
const verdict = versionVerdict(manifest.nextVersion, verified);
const problem = routingVersion !== manifest.nextVersion ? `@next/routing is ${routingVersion}, the app's Next is ${manifest.nextVersion}` : verdict.ok ? null : `Next ${verdict.reason}`;
if (problem) {
  if (process.env.FORKRAIL_NEXT_UNVERIFIED !== '1') {
    console.error(`forkrail next: not starting: ${problem}. This build should run on Next's own server.`);
    process.exit(1);
  }
  console.warn(`forkrail next: ${problem}; running anyway (FORKRAIL_NEXT_UNVERIFIED=1)`);
}

// Next's entrypoints read their manifests relative to the working directory, and are built for
// production.
process.env.NODE_ENV = 'production';
process.chdir(projectDir);
if (manifest.deploymentId && !process.env.NEXT_DEPLOYMENT_ID) process.env.NEXT_DEPLOYMENT_ID = manifest.deploymentId;
// The config's `env` values, for code that reads them by a computed name (the build inlined the
// plain reads), as Next's server puts them at its start.
for (const [key, value] of Object.entries(manifest.env ?? {})) process.env[key] ||= value;
// Next's shared page runtime reads this one at run time: with it a handler works out a config
// rewrite's query for a URL that ends in a slash, as Next's own router would have.
if (manifest.trailingSlash) process.env.__NEXT_TRAILING_SLASH = 'true';

// Next's own setup of the Node environment for entrypoints run without its server (traced into
// every function by the build): AsyncLocalStorage globals, the require hook for its compiled
// React, WebCrypto.
const require = createRequire(path.join(projectDir, 'package.json'));
mark('manifest');
require('next/dist/build/adapter/setup-node-env.external.js');
mark('node env');

// `.env`, `.env.production` and their `.local` forms, as `next start` reads them at start, with
// Next's own reader. It is a dependency of `next`, not of the app (pnpm keeps it beside `next`),
// so it is looked for from Next's own folder. A package with env files and no reader says so:
// the app would run without values it was built expecting.
if (['.env', '.env.production', '.env.local', '.env.production.local'].some((name) => fs.existsSync(path.join(projectDir, name)))) {
  try {
    createRequire(require.resolve('next/dist/build/adapter/setup-node-env.external.js'))('@next/env').loadEnvConfig(projectDir, false, { info() {}, error: console.error });
  } catch (error) {
    console.error(`forkrail next: the project's .env files were not read (${error.code ?? error.message}): @next/env is missing from the package`);
  }
}

// The build's preview keys (Draft Mode, on-demand revalidation). Next's server hands them to
// middleware and Edge functions through the environment.
let previewId = null;
try {
  const { preview } = JSON.parse(fs.readFileSync(path.join(projectDir, manifest.distDir, 'prerender-manifest.json'), 'utf8'));
  if (preview) {
    previewId = preview.previewModeId;
    process.env.__NEXT_PREVIEW_MODE_ID ??= preview.previewModeId;
    process.env.__NEXT_PREVIEW_MODE_SIGNING_KEY ??= preview.previewModeSigningKey;
    process.env.__NEXT_PREVIEW_MODE_ENCRYPTION_KEY ??= preview.previewModeEncryptionKey;
  }
} catch {
  // No prerender manifest: nothing was prerendered and nothing can be previewed.
}

const { createNextServer } = await import('./server.mjs');
const app = createNextServer({ root, manifest });
mark('server');

// `instrumentation.ts`'s `register()` runs once, before the first request, as under `next start`
// (and inside Lambda's init, which runs at full speed whatever the memory size).
if (fs.existsSync(path.join(projectDir, manifest.distDir, 'server', 'instrumentation.js'))) {
  const { ensureInstrumentationRegistered } = require('next/dist/server/lib/router-utils/instrumentation-globals.external.js');
  await ensureInstrumentationRegistered(projectDir, manifest.distDir);
}

// forkrail's runtime gives the server its `waitUntil` and calls a `forkrail.prime` hook (both
// from its shim, which ran before this file). The hook's result goes into the runtime's line for
// the start.
const runtimePrimes = typeof globalThis[Symbol.for('forkrail.waitUntil')] === 'function' && process.env.FORKRAIL_PRIME !== '0';
let preloaded = null;
if (runtimePrimes) {
  globalThis[Symbol.for('forkrail.prime')] = ({ deadline }) =>
    app.prime({
      deadline,
      previewId,
      paths: primePaths(process.env.FORKRAIL_PRIME_PATHS),
      visitor: primeVisitor(process.env.VERCEL_URL),
      memoryLimit: (Number(process.env.AWS_LAMBDA_FUNCTION_MEMORY_SIZE) || 0) * 1024 * 1024 * (PRELOAD_MEMORY_PCT / 100),
    });
} else {
  preloaded = app.preload();
  mark('preload');
}

const server = http.createServer((req, res) => app.handle(req, res));
// The runtime (or a local router) is the only client and keeps its connections.
server.keepAliveTimeout = 0;
const port = Number(process.env.PORT || 3000);
const host = process.env.FORKRAIL_NEXT_HOST || '127.0.0.1';
server.listen(port, host, () => {
  // Where Next sends a server action on to the worker that has its page: this process.
  process.env.__NEXT_PRIVATE_ORIGIN ??= `http://${host}:${server.address().port}`;
  if (process.env.FORKRAIL_NEXT_DEBUG) {
    mark('listen');
    const steps = marks
      .slice(1)
      .map(([name, at], i) => `${name} ${(at - marks[i][1]).toFixed(0)}`)
      .join(', ');
    const loaded = runtimePrimes ? 'priming next' : `${preloaded.loaded} of ${preloaded.of} entrypoints loaded`;
    // `node`: Node's own bootstrap; `preloads`: what ran before this file (the runtime's shim).
    const boot = performance.nodeTiming.bootstrapComplete;
    console.log(
      `forkrail next: listening on ${port} ${performance.now().toFixed(0)} ms after the process started (Next ${manifest.nextVersion}; ms: node ${boot.toFixed(0)}, preloads ${(T0 - boot).toFixed(0)}, ${steps}; ${loaded})`,
    );
  }
});
