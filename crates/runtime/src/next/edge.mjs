// Edge-runtime entrypoints (a `middleware.ts`, a page or route with `runtime = 'edge'`) run in
// this Node process, in Next's own Edge sandbox: the module Next's server runs them in
// (`next/dist/server/web/sandbox`), loaded from the app's own Next. It is only loaded when a
// build has such an entry and one is asked for.
//
// The sandbox takes an entry as the build's `middleware-manifest.json` describes it (its files in
// load order, its WebAssembly and assets). The adapter's output names the same entry by its
// registry key (`edge.entryKey`, `middleware_<name>`), which is how the two are matched.

import fs from 'node:fs';
import path from 'node:path';

export function createEdge({ projectDir, distDir, manifest, require }) {
  let sandbox = null;
  let functions = null;
  const infos = new Map();

  function info(entry) {
    let found = infos.get(entry.id);
    if (found) return found;
    functions ??= JSON.parse(fs.readFileSync(path.join(distDir, 'server', 'middleware-manifest.json'), 'utf8'));
    const all = [...Object.values(functions.middleware ?? {}), ...Object.values(functions.functions ?? {})];
    const fn = all.find((one) => `middleware_${one.name}` === entry.edge?.entryKey);
    if (!fn) throw new Error(`the Edge entry ${entry.edge?.entryKey ?? entry.id} is not in the build's middleware-manifest.json`);
    const absolute = (list) => (list ?? []).map((binding) => ({ ...binding, filePath: path.join(distDir, binding.filePath) }));
    found = { name: fn.name, page: fn.page, paths: fn.files.map((file) => path.join(distDir, file)), wasm: absolute(fn.wasm), assets: absolute(fn.assets), env: fn.env ?? {} };
    infos.set(entry.id, found);
    return found;
  }

  /// Runs an Edge entry for one request and gives its `Response`. `body` is a Node readable (or
  /// nothing); `headers` a Node-style object.
  async function run(entry, { url, method, headers, body, page, signal, waitUntil, requestMeta }) {
    sandbox ??= require('next/dist/server/web/sandbox');
    const fn = info(entry);
    const result = await sandbox.run({
      distDir,
      name: fn.name,
      paths: fn.paths,
      edgeFunctionEntry: fn,
      request: {
        headers,
        method,
        nextConfig: { basePath: manifest.basePath, i18n: manifest.i18n ?? undefined, trailingSlash: manifest.trailingSlash },
        url,
        page: { ...page, ...(page && 'name' in page ? { name: fn.page } : {}) },
        body: body && method !== 'GET' && method !== 'HEAD' ? { cloneBodyStream: () => body, finalize: async () => {} } : undefined,
        signal: signal ?? new AbortController().signal,
        waitUntil,
        requestMeta,
      },
      useCache: true,
      incrementalCache: globalThis.__incrementalCache,
      clientAssetToken: manifest.immutableAssets ? '' : manifest.deploymentId,
    });
    if (result.waitUntil) waitUntil(result.waitUntil);
    return result.response;
  }

  return { run };
}
