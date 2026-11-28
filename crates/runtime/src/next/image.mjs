// `/_next/image`, for a package that carries Next's own image optimizer. On forkrail the router
// answers image requests with its image function before any worker sees them, and a deployed
// package has no optimizer in it: this file then answers 404. It is here for local runs and for
// Next's deploy tests against this server alone, where it does what Next's server does with the
// same modules (`next/dist/server/image-optimizer`), kept in Next's own image cache.

import fs from 'node:fs';
import path from 'node:path';
import { parse as parseQuery } from 'node:querystring';

export function createImages({ distDir, require, handle, render404 }) {
  let loaded;
  function load() {
    if (loaded !== undefined) return loaded;
    try {
      const optimizer = require('next/dist/server/image-optimizer');
      const cacheModule = require('next/dist/server/response-cache');
      const ResponseCache = cacheModule.default ?? cacheModule;
      const { getExtension } = require('next/dist/server/serve-static');
      const nextConfig = JSON.parse(fs.readFileSync(path.join(distDir, 'required-server-files.json'), 'utf8')).config;
      loaded = {
        optimizer,
        getExtension,
        nextConfig,
        responses: new ResponseCache({ minimalMode: false, route: 'image' }),
        cache: new optimizer.ImageOptimizerCache({ distDir, nextConfig }),
      };
    } catch {
      loaded = null;
    }
    return loaded;
  }

  /// Answers an image request. False when this package has no optimizer.
  async function serve(state) {
    const { req, res, url } = state;
    const image = load();
    if (!image) return false;
    // A local source is asked of this server with the image request's method (Next's
    // fetchInternalImage): only GET and HEAD, as the router's image function takes, so a visitor's
    // `DELETE /_next/image?url=/api/...` never reaches a route the firewall judged as an image.
    if (req.method !== 'GET' && req.method !== 'HEAD') {
      res.statusCode = 405;
      res.setHeader('allow', 'GET, HEAD');
      res.end();
      return true;
    }
    const { optimizer, nextConfig } = image;
    const config = nextConfig.images;
    if (config.loader !== 'default' || config.unoptimized) {
      await render404(state, req, res);
      return true;
    }
    const params = optimizer.ImageOptimizerCache.validateParams(req, parseQuery(url.search.slice(1)), nextConfig, false);
    if ('errorMessage' in params) {
      res.statusCode = 400;
      res.end(params.errorMessage);
      return true;
    }
    try {
      const entry = await image.responses.get(
        optimizer.ImageOptimizerCache.getCacheKey(params),
        async ({ previousCacheEntry }) => {
          // A local image is asked of this same server (so a middleware guarding it still does).
          const inner = async (innerReq, innerRes) => {
            if (innerReq.url === req.url) throw new Error('Invariant attempted to optimize _next/image itself');
            await handle(innerReq, innerRes);
          };
          const upstream = params.isAbsolute
            ? await optimizer.fetchExternalImage(params.href, config.dangerouslyAllowLocalIP, config.maximumResponseBody, config.maximumRedirects)
            : await optimizer.fetchInternalImage(params.href, req, res, config.maximumResponseBody, inner);
          const { buffer, contentType, maxAge, upstreamEtag, etag } = await optimizer.imageOptimizer(upstream, params, nextConfig, { isDev: false, previousCacheEntry });
          return { value: { kind: 'IMAGE', buffer, etag, extension: image.getExtension(contentType), upstreamEtag }, cacheControl: { revalidate: maxAge, expire: undefined } };
        },
        { routeKind: 'IMAGE', incrementalCache: image.cache, isFallback: false },
      );
      if (entry?.value?.kind !== 'IMAGE') throw entry?.error ?? new Error('invariant did not get entry from image response cache');
      const state_ = entry.isMiss ? 'MISS' : entry.isStale ? 'STALE' : 'HIT';
      optimizer.sendResponse(req, res, params.href, entry.value.extension, entry.value.buffer, entry.value.etag, params.isStatic, state_, config, entry.cacheControl?.revalidate || 0, false);
    } catch (error) {
      if (!(error instanceof optimizer.ImageError)) throw error;
      res.statusCode = error.statusCode;
      res.end(error.message);
    }
    return true;
  }

  return { serve };
}
