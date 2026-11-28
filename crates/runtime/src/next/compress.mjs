// gzip for the answers that compress, as Next's own server does with its `compress` setting on
// (the default): gzip or deflate by `Accept-Encoding`, `Vary: Accept-Encoding` on every
// compressible type, nothing for an answer under 1 KB whose length is known, and each chunk
// flushed so a streamed page still arrives as it is made (Next calls `res.flush()` after every
// write of a stream; this gives it one).

import { createRequire } from 'node:module';

// zlib is loaded with the first answer that is compressed, not at start.
const builtin = createRequire(import.meta.url);

const COMPRESSIBLE =
  /^text\/|\+(?:json|text|xml)$|^application\/(?:json|javascript|x-javascript|xml|wasm|x-www-form-urlencoded|vnd\.ms-fontobject|x-ndjson|x-sh)$|^image\/(?:svg\+xml|x-icon|bmp|vnd\.microsoft\.icon)$|^font\/(?:otf|ttf)$/i;
const THRESHOLD = 1024;

/// Can a body of this `content-type` be made smaller by gzip?
export function compressible(type) {
  if (typeof type !== 'string') return false;
  return COMPRESSIBLE.test(type.split(';', 1)[0].trim());
}

/// The encoding to answer with for an `Accept-Encoding` header: gzip when it is taken, else
/// deflate, else none.
export function encodingFor(accept) {
  if (typeof accept !== 'string' || accept === '') return null;
  const q = {};
  for (const part of accept.split(',')) {
    const [name, ...params] = part.trim().toLowerCase().split(';');
    const weight = params.map((p) => p.trim()).find((p) => p.startsWith('q='));
    q[name.trim()] = weight === undefined ? 1 : Number(weight.slice(2)) || 0;
  }
  const ok = (name) => (q[name] ?? q['*'] ?? 0) > 0;
  if (ok('gzip')) return 'gzip';
  if (ok('deflate')) return 'deflate';
  return null;
}

function addVary(res, field) {
  const current = res.getHeader('vary');
  const list =
    current === undefined
      ? []
      : String(Array.isArray(current) ? current.join(', ') : current)
          .split(',')
          .map((v) => v.trim());
  if (list.includes('*') || list.some((v) => v.toLowerCase() === field.toLowerCase())) return;
  res.setHeader('vary', [...list, field].join(', '));
}

/// Wraps a response so what is written to it goes out compressed when it should.
export function compress(req, res) {
  const { write, end, writeHead } = res;
  let stream = null;
  let decided = false;

  function decide(known) {
    decided = true;
    if (!compressible(res.getHeader('content-type'))) return;
    addVary(res, 'Accept-Encoding');
    const encoded = res.getHeader('content-encoding');
    if ((encoded && encoded !== 'identity') || req.method === 'HEAD' || res.statusCode === 204 || res.statusCode === 304) return;
    if (/(?:^|,)\s*no-transform\s*(?:,|$)/.test(String(res.getHeader('cache-control') ?? ''))) return;
    const length = res.hasHeader('content-length') ? Number(res.getHeader('content-length')) : known;
    if (length !== undefined && length < THRESHOLD) return;
    const encoding = encodingFor(req.headers['accept-encoding']);
    if (!encoding) return;
    res.setHeader('content-encoding', encoding);
    res.removeHeader('content-length');
    const zlib = builtin('node:zlib');
    stream = encoding === 'gzip' ? zlib.createGzip() : zlib.createDeflate();
    stream.on('data', (chunk) => {
      if (write.call(res, chunk) === false) stream.pause();
    });
    stream.on('end', () => end.call(res));
    // A writer told to wait (`write` gave false) waits for the response's own `drain`.
    stream.on('drain', () => res.emit('drain'));
    stream.on('error', () => res.destroy());
    res.on('drain', () => {
      if (stream.isPaused()) stream.resume();
    });
    res.once('close', () => stream.destroy());
  }

  res.flush = function () {
    if (stream && !stream.writableEnded) stream.flush();
  };

  // Headers given to `writeHead` count too: they are set first, then the decision is made.
  res.writeHead = function (status, ...rest) {
    const headers = rest.find((arg) => arg !== null && typeof arg === 'object');
    if (!decided) {
      if (typeof status === 'number') res.statusCode = status;
      if (Array.isArray(headers)) {
        if (Array.isArray(headers[0])) for (const [name, value] of headers) res.setHeader(name, value);
        else for (let i = 0; i + 1 < headers.length; i += 2) res.appendHeader(headers[i], headers[i + 1]);
      } else if (headers) {
        for (const [name, value] of Object.entries(headers)) if (value !== undefined) res.setHeader(name, value);
      }
      decide();
      const reason = rest.find((arg) => typeof arg === 'string');
      return reason === undefined ? writeHead.call(res, res.statusCode) : writeHead.call(res, res.statusCode, reason);
    }
    return writeHead.call(res, status, ...rest);
  };

  res.write = function (chunk, encoding, callback) {
    if (!decided) decide();
    if (!stream) return write.call(res, chunk, encoding, callback);
    if (typeof encoding === 'function') [callback, encoding] = [encoding, undefined];
    return stream.write(typeof chunk === 'string' ? Buffer.from(chunk, encoding) : chunk, callback);
  };

  res.end = function (chunk, encoding, callback) {
    if (typeof chunk === 'function') [callback, chunk, encoding] = [chunk, undefined, undefined];
    if (typeof encoding === 'function') [callback, encoding] = [encoding, undefined];
    if (!decided) {
      // The whole body in one `end`: its length is known, as if `content-length` were set.
      const known = chunk === undefined || chunk === null ? 0 : typeof chunk === 'string' ? Buffer.byteLength(chunk, encoding) : chunk.length;
      decide(known);
      // Next's server sends its head before a body given whole, so the body goes out chunked
      // with no `content-length`: kept the same, so the two answer byte for byte alike.
      if (!res.headersSent && !res.hasHeader('content-length')) writeHead.call(res, res.statusCode);
    }
    if (!stream) return end.call(res, chunk, encoding, callback);
    if (callback) res.once('finish', callback);
    if (chunk !== undefined && chunk !== null) stream.end(typeof chunk === 'string' ? Buffer.from(chunk, encoding) : chunk);
    else stream.end();
    return res;
  };
}
