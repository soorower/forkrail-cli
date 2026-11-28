// Static files, for the requests this server answers with one: a local run (on forkrail the
// router serves a deployment's static files from its own store, before any worker), and a
// rewrite or an error page that ends on a file (`/404.html`, a file in `public/`).

import fs from 'node:fs';
import path from 'node:path';
import { pipeline } from 'node:stream/promises';

import { pageEtag } from './routing.mjs';

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=UTF-8',
  '.mjs': 'application/javascript; charset=UTF-8',
  '.css': 'text/css; charset=UTF-8',
  '.json': 'application/json; charset=UTF-8',
  '.map': 'application/json; charset=UTF-8',
  '.txt': 'text/plain; charset=UTF-8',
  '.xml': 'application/xml',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.avif': 'image/avif',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.ttf': 'font/ttf',
  '.otf': 'font/otf',
  '.wasm': 'application/wasm',
  '.mp4': 'video/mp4',
  '.webm': 'video/webm',
  '.mp3': 'audio/mpeg',
  '.pdf': 'application/pdf',
  '.webmanifest': 'application/manifest+json',
  '.rsc': 'text/x-component',
};

/// The content type of a static output. A page written as a file (`/about` → `about.html`) has
/// no extension in its pathname: the file's own decides.
export function contentType(file, pathname) {
  return TYPES[path.extname(pathname).toLowerCase()] ?? TYPES[path.extname(file).toLowerCase()] ?? 'application/octet-stream';
}

export async function serveStatic(req, res, file, pathname, { status, poweredBy = false } = {}) {
  let stat;
  try {
    stat = await fs.promises.stat(file);
    if (!stat.isFile()) throw new Error('not a file');
  } catch {
    res.statusCode = 404;
    res.setHeader('content-type', 'text/plain; charset=utf-8');
    return res.end('Not Found');
  }
  // A page the build wrote out as HTML (a Pages Router page with no data fetching, `/404`):
  // answered as Next's server answers it, which renders nothing either: its content-based ETag,
  // `x-powered-by`, and no `cache-control` of its own.
  if (file.endsWith('.html')) {
    const html = await fs.promises.readFile(file, 'utf8');
    if (poweredBy) res.setHeader('x-powered-by', 'Next.js');
    const etag = pageEtag(html);
    res.setHeader('etag', etag);
    if (status) res.statusCode = status;
    else if (req.headers['if-none-match'] === etag) {
      res.statusCode = 304;
      return res.end();
    }
    res.setHeader('content-type', 'text/html; charset=utf-8');
    res.setHeader('content-length', Buffer.byteLength(html));
    return res.end(req.method === 'HEAD' ? undefined : html);
  }
  // A route the build answered once and wrote out (`app/icon.svg`, `robots.txt`, `sitemap.xml`):
  // `<name>.body` with its headers in `<name>.meta` beside it.
  if (file.endsWith('.body')) {
    try {
      const meta = JSON.parse(await fs.promises.readFile(`${file.slice(0, -'.body'.length)}.meta`, 'utf8'));
      for (const [name, value] of Object.entries(meta.headers ?? {})) {
        if (name !== 'x-next-cache-tags' && !res.hasHeader(name)) res.setHeader(name, value);
      }
    } catch {
      // No meta: the defaults below.
    }
  }
  const etag = `W/"${stat.size.toString(16)}-${Math.floor(stat.mtimeMs).toString(16)}"`;
  if (!res.hasHeader('content-type')) res.setHeader('content-type', contentType(file, pathname));
  if (!res.hasHeader('cache-control')) res.setHeader('cache-control', 'public, max-age=0');
  res.setHeader('etag', etag);
  res.setHeader('last-modified', stat.mtime.toUTCString());
  res.setHeader('accept-ranges', 'bytes');
  if (status) res.statusCode = status;
  else if (req.headers['if-none-match'] === etag) {
    res.statusCode = 304;
    return res.end();
  }
  // One byte range (a video seeking, a download resuming); anything else gets the whole file.
  let start = 0;
  let end = stat.size - 1;
  const range = !status && /^bytes=(\d*)-(\d*)$/.exec(req.headers.range ?? '');
  if (range && (range[1] || range[2]) && stat.size > 0) {
    if (range[1] === '') start = Math.max(0, stat.size - Number(range[2]));
    else {
      start = Number(range[1]);
      if (range[2] !== '') end = Math.min(end, Number(range[2]));
    }
    if (start > end || start >= stat.size) {
      res.statusCode = 416;
      res.setHeader('content-range', `bytes */${stat.size}`);
      return res.end();
    }
    res.statusCode = 206;
    res.setHeader('content-range', `bytes ${start}-${end}/${stat.size}`);
  }
  res.setHeader('content-length', end - start + 1);
  if (req.method === 'HEAD') return res.end();
  await pipeline(fs.createReadStream(file, { start, end }), res).catch(() => {});
}
