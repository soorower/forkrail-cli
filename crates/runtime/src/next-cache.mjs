// forkrail's cache for Next.js (N2): Next's two cache interfaces, kept by the router.
//
// The runtime preloads this file into a Next.js app (`node --import`, `nextcache.rs`) when the
// function has FORKRAIL_NEXT_CACHE=1. It implements
// - `cacheHandler` (one class: ISR pages, route handlers' answers, the data cache, redirects,
//   images), in every shape from Next 13.5 to 16, and
// - `cacheHandlers` (`'use cache'`): `get`, `set`, `refreshTags`, `getExpiration`, `updateTags`,
//   and the older `expireTags` / `receiveExpiredTags`,
// and hands both to Next through `globalThis[Symbol.for('@next/cache-handlers')]` (`FetchCache`,
// `DefaultCache`, `RemoteCache`), which Next reads from 15.0 when the project sets no handler of
// its own. Next 13.5-14.2 has no such hook: the builder points the project's config at a stub
// that takes the class from `globalThis[Symbol.for('forkrail.next-cache')]` (see `STUB` below).
//
// Entries and tag marks live in the router (`crates/router/src/cache/next.rs` has the design),
// one copy for every environment and region. This process talks to its runtime on a loopback
// port (FORKRAIL_NEXT_CACHE_AT, `127.0.0.1:<port>:<secret>`) in frames
// (`forkrail_common::nextcache`); the runtime passes them up the worker's tunnel.
//
// What decides whether an entry is fresh is the router, by its clock: it answers `fresh`, `lead`
// (stale, and this reader makes it again) or `wait` (stale, another reader is making it), or a
// miss for one a tag deleted. Next's own staleness checks are steered by the `lastModified` (or
// `revalidate`) handed back with the entry, so that only the leading reader regenerates.
// The router also sends, unasked, every tag mark of this project and environment and a note for
// every entry written; with those this process keeps a small memory of entries it can serve
// without asking (FORKRAIL_NEXT_CACHE_MEMORY_MB, default 16; entries up to 1 MB).
//
// Nothing here throws into Next: a read that fails is a miss, a write that fails is dropped.
// A tag invalidation that can't be sent is kept and sent again.

import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import net from 'node:net';
import path from 'node:path';

const KIND = { GET: 1, SET: 2, TAGS: 3, SYNC: 4, REPLY: 16, MARKS: 17, CHANGED: 18, HELLO: 32, UP: 33, DOWN: 34 };
const MAX_PAGE_BYTES = 20 << 20;
const MAX_DATA_BYTES = 2 << 20;
const MAX_FRAME = MAX_PAGE_BYTES + (1 << 20);
const LEASE_MS = 60_000;
const TIMEOUT = { get: 1_000, set: 5_000, tags: 3_000, sync: 3_000 };
/// A tag invalidation that couldn't be sent is tried again this often, this many times.
const TAGS_RETRY_MS = 500;
const TAGS_RETRIES = 20;
const NEXT_TAGS_HEADER = 'x-next-cache-tags';

/// What the builder writes beside the project's config for Next 13.5-14.2, and points
/// `experimental.incrementalCacheHandlerPath` (13.5, 14.0) or `cacheHandler` (14.1, 14.2) at:
/// forkrail's class in a worker, Next's own file-system cache anywhere else (the build).
export const STUB = `const forkrail = globalThis[Symbol.for('forkrail.next-cache')];
module.exports = forkrail ? forkrail.IncrementalCache : require('next/dist/server/lib/incremental-cache/file-system-cache').default;
`;

const sha256 = (text) => createHash('sha256').update(text).digest('hex');

/// A frame: `[kind u8][id u32][length u32]`, then `[head length u32][head JSON][body]`.
export function encodeFrame(kind, id, head, bodies = []) {
  const json = Buffer.from(JSON.stringify(head));
  const size = bodies.reduce((n, b) => n + b.length, 0);
  const start = Buffer.allocUnsafe(13);
  start.writeUInt8(kind, 0);
  start.writeUInt32BE(id, 1);
  start.writeUInt32BE(4 + json.length + size, 5);
  start.writeUInt32BE(json.length, 9);
  return [start, json, ...bodies];
}

/// Frames out of the bytes of a connection, as they come.
export class FrameReader {
  #chunks = [];
  #have = 0;

  /// The whole frames `chunk` completes: `{ kind, id, head, body }`. Throws on one that can't be a frame.
  push(chunk) {
    this.#chunks.push(chunk);
    this.#have += chunk.length;
    const frames = [];
    for (;;) {
      if (this.#have < 9) break;
      if (this.#chunks[0].length < 9) this.#chunks = [Buffer.concat(this.#chunks)];
      const first = this.#chunks[0];
      const length = first.readUInt32BE(5);
      if (length > MAX_FRAME || length < 4) throw new Error('not a frame');
      if (this.#have < 9 + length) break;
      const all = this.#chunks.length === 1 ? first : Buffer.concat(this.#chunks);
      const payload = all.subarray(9, 9 + length);
      const rest = all.subarray(9 + length);
      this.#chunks = rest.length ? [rest] : [];
      this.#have = rest.length;
      const headLength = payload.readUInt32BE(0);
      if (4 + headLength > payload.length) throw new Error('not a frame');
      let head;
      try {
        head = JSON.parse(payload.toString('utf8', 4, 4 + headLength));
      } catch {
        throw new Error('not a frame');
      }
      frames.push({ kind: first.readUInt8(0), id: first.readUInt32BE(1), head, body: payload.subarray(4 + headLength) });
    }
    return frames;
  }
}

/// An entry's body: `[meta length u32][meta JSON][parts]`.
export function entryBody(meta, parts) {
  const json = Buffer.from(JSON.stringify(meta));
  const start = Buffer.allocUnsafe(4);
  start.writeUInt32BE(json.length, 0);
  return [start, json, ...parts];
}

function openEntry(body) {
  if (body.length < 4) return null;
  const length = body.readUInt32BE(0);
  if (4 + length > body.length) return null;
  let meta;
  try {
    meta = JSON.parse(body.toString('utf8', 4, 4 + length));
  } catch {
    return null;
  }
  const parts = new Map();
  let at = 4 + length;
  for (const [name, size] of meta.parts ?? []) {
    if (at + size > body.length) return null;
    parts.set(name, body.subarray(at, at + size));
    at += size;
  }
  return { meta, parts };
}

const isBytes = (value) => Buffer.isBuffer(value) || value instanceof Uint8Array || value instanceof ArrayBuffer;
const bytes = (value) => (Buffer.isBuffer(value) ? value : value instanceof ArrayBuffer ? Buffer.from(value) : typeof value === 'string' ? Buffer.from(value) : Buffer.from(value.buffer, value.byteOffset, value.byteLength));
const seconds = (value) => (typeof value === 'number' && Number.isFinite(value) ? value : undefined);

function headerPairs(headers) {
  const pairs = [];
  if (!headers || typeof headers !== 'object') return pairs;
  for (const [name, value] of Object.entries(headers)) {
    if (name.toLowerCase() === NEXT_TAGS_HEADER) continue;
    for (const one of Array.isArray(value) ? value : [value]) {
      if (typeof one === 'string' || typeof one === 'number') pairs.push([name.toLowerCase(), String(one)]);
    }
  }
  return pairs;
}

function splitTags(value) {
  const raw = Array.isArray(value) ? value.join(',') : typeof value === 'string' ? value : '';
  return raw.split(',').map((tag) => tag.trim()).filter(Boolean);
}

/// A value Next hands `cacheHandler.set`, as an entry: its kind, parts and what else it holds.
/// Next 13.5-14.2 calls an App Router page and a Pages Router page both `PAGE` (and a route
/// handler's answer `ROUTE`); they're kept under 15's names, which say which is which.
export function fromNext(data, ctx = {}) {
  if (data == null) return { kind: 'NULL', parts: [], x: {} };
  const kind = data.kind;
  const tagsOf = (headers) => splitTags(headers?.[NEXT_TAGS_HEADER]);
  if (kind === 'FETCH') {
    // `unstable_cache` of `undefined` has no body at all, which isn't an empty one.
    const body = data.data?.body;
    return {
      kind,
      parts: body == null ? [] : [['body', bytes(body)]],
      x: { headers: data.data?.headers ?? {}, status: data.data?.status, url: data.data?.url, revalidate: data.revalidate, tags: data.tags },
      tags: [...new Set([...(ctx.tags ?? []), ...(data.tags ?? [])])],
      rev: seconds(data.revalidate) ?? seconds(ctx.revalidate),
    };
  }
  const rev = ctx.cacheControl ? seconds(ctx.cacheControl.revalidate) : seconds(ctx.revalidate);
  const exp = seconds(ctx.cacheControl?.expire);
  const base = { status: data.status ?? 200, headers: headerPairs(data.headers), tags: tagsOf(data.headers), rev, exp };
  if (kind === 'APP_ROUTE' || kind === 'ROUTE') {
    return { ...base, kind: 'APP_ROUTE', parts: [['body', bytes(data.body ?? '')]], x: { h: data.headers, status: data.status } };
  }
  const appPage = kind === 'APP_PAGE' || (kind === 'PAGE' && (typeof data.pageData === 'string' || isBytes(data.pageData)));
  if (appPage) {
    const rsc = kind === 'APP_PAGE' ? data.rscData : data.pageData;
    const parts = [['html', bytes(String(data.html ?? ''))]];
    const x = { h: data.headers, status: data.status };
    if (rsc != null) {
      parts.push(['rsc', bytes(rsc)]);
      x.rsc = typeof rsc === 'string' ? 'string' : 'buffer';
    }
    if (data.segmentData) {
      x.seg = data.segmentData instanceof Map ? 'map' : 'object';
      const segments = data.segmentData instanceof Map ? data.segmentData.entries() : Object.entries(data.segmentData);
      for (const [segment, value] of segments) parts.push([`seg:${segment}`, bytes(value)]);
    }
    if (data.postponed != null) parts.push(['postponed', bytes(String(data.postponed))]);
    return { ...base, kind: 'APP_PAGE', parts, x };
  }
  if (kind === 'PAGES' || kind === 'PAGE') {
    return { ...base, kind: 'PAGES', parts: [['html', bytes(String(data.html ?? ''))], ['data', Buffer.from(JSON.stringify(data.pageData ?? {}))]], x: { h: data.headers, status: data.status } };
  }
  if (kind === 'REDIRECT') return { ...base, kind, status: 200, headers: [], tags: [], parts: [], x: { props: data.props } };
  if (kind === 'IMAGE') {
    const { buffer, kind: _kind, ...rest } = data;
    return { kind, parts: [['buffer', bytes(buffer ?? '')]], x: rest, rev: seconds(ctx.revalidate) ?? seconds(data.revalidate) };
  }
  // A kind this file doesn't know: kept whole, binary values as base64.
  const json = JSON.stringify(data, (_key, value) => (value instanceof Map ? { $map: [...value] } : isBytes(value) ? { $bytes: bytes(value).toString('base64') } : value?.type === 'Buffer' && Array.isArray(value.data) ? { $bytes: Buffer.from(value.data).toString('base64') } : value));
  return { ...base, kind: String(kind), parts: [['json', Buffer.from(json)]], x: {} };
}

/// The value Next expects back, in the words of the Next that's asking (`modern`: 15 and later).
export function toNext(meta, parts, modern) {
  const x = meta.x ?? {};
  const text = (name) => parts.get(name)?.toString('utf8');
  switch (meta.kind) {
    case 'NULL':
      return null;
    case 'FETCH':
      return { kind: 'FETCH', data: { headers: x.headers ?? {}, body: text('body'), status: x.status, url: x.url }, revalidate: x.revalidate ?? meta.rev, ...(x.tags ? { tags: x.tags } : {}) };
    case 'APP_ROUTE':
      return { kind: modern ? 'APP_ROUTE' : 'ROUTE', body: Buffer.from(parts.get('body') ?? ''), status: x.status, headers: x.h };
    case 'APP_PAGE': {
      const rsc = parts.get('rsc');
      let segmentData;
      if (x.seg) {
        const segments = [...parts].filter(([name]) => name.startsWith('seg:')).map(([name, value]) => [name.slice(4), Buffer.from(value)]);
        segmentData = x.seg === 'map' ? new Map(segments) : Object.fromEntries(segments);
      }
      if (!modern) return { kind: 'PAGE', html: text('html') ?? '', pageData: rsc ? (x.rsc === 'buffer' ? Buffer.from(rsc) : rsc.toString('utf8')) : undefined, postponed: text('postponed'), headers: x.h, status: x.status };
      return { kind: 'APP_PAGE', html: text('html') ?? '', rscData: rsc ? Buffer.from(rsc) : undefined, postponed: text('postponed'), headers: x.h, status: x.status, segmentData };
    }
    case 'PAGES': {
      let pageData = {};
      try {
        pageData = JSON.parse(text('data') ?? '{}');
      } catch {}
      return { kind: modern ? 'PAGES' : 'PAGE', html: text('html') ?? '', pageData, headers: x.h, status: x.status };
    }
    case 'REDIRECT':
      return { kind: 'REDIRECT', props: x.props };
    case 'IMAGE':
      return { ...x, kind: 'IMAGE', buffer: Buffer.from(parts.get('buffer') ?? '') };
    default:
      try {
        return JSON.parse(text('json') ?? 'null', (_key, value) => (value && typeof value === 'object' && '$map' in value ? new Map(value.$map) : value && typeof value === 'object' && '$bytes' in value ? Buffer.from(value.$bytes, 'base64') : value));
      } catch {
        return null;
      }
  }
}

/// What a tag's mark says of an entry made at `made`, at the router's time `now`.
function judgeMark(mark, made, now) {
  if (!mark) return null;
  if (made < (mark.d ?? 0)) return 'deleted';
  let verdict = made < (mark.s ?? 0) ? 'stale' : null;
  for (const [at, until] of mark.p ?? []) {
    if (made < at) {
      if (now >= until) return 'deleted';
      verdict = 'stale';
    }
  }
  return verdict;
}

/// The connection to the runtime: requests by id, and what the router sends unasked.
class Client {
  #at;
  #socket = null;
  #connecting = null;
  #nextId = 1;
  #pending = new Map();
  #on;

  constructor(at, on) {
    this.#at = at;
    this.#on = on;
  }

  #connect() {
    if (this.#socket) return Promise.resolve(this.#socket);
    this.#connecting ??= new Promise((resolve) => {
      const [host, port, secret] = this.#at.split(':');
      const socket = net.connect({ host, port: Number(port) });
      const reader = new FrameReader();
      const fail = () => {
        if (this.#socket === socket) this.#socket = null;
        this.#connecting = null;
        socket.destroy();
        for (const [, pending] of this.#pending) pending(null);
        this.#pending.clear();
        this.#on({ kind: KIND.DOWN, head: {} });
        resolve(null);
      };
      socket.setNoDelay(true);
      // The connection alone doesn't keep a process (a build's worker, a script) alive.
      socket.unref();
      socket.once('connect', () => {
        socket.cork();
        for (const part of encodeFrame(KIND.HELLO, 0, { secret })) socket.write(part);
        socket.uncork();
        this.#socket = socket;
        this.#connecting = null;
        resolve(socket);
      });
      socket.on('data', (chunk) => {
        let frames;
        try {
          frames = reader.push(chunk);
        } catch {
          return fail();
        }
        for (const frame of frames) {
          if (frame.kind === KIND.REPLY) {
            const pending = this.#pending.get(frame.id);
            this.#pending.delete(frame.id);
            pending?.(frame);
          } else {
            this.#on(frame);
          }
        }
      });
      socket.once('error', fail);
      socket.once('close', fail);
    });
    return this.#connecting;
  }

  /// One request: the router's answer, or `null` (no router, too slow, an error in its answer).
  async request(kind, head, bodies, timeoutMs) {
    const socket = await this.#connect();
    if (!socket) return null;
    const id = this.#nextId++ >>> 0 || this.#nextId++;
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        resolve(null);
      }, timeoutMs);
      timer.unref();
      this.#pending.set(id, (frame) => {
        clearTimeout(timer);
        resolve(frame && !frame.head?.error ? frame : null);
      });
      socket.cork();
      for (const part of encodeFrame(kind, id, head, bodies)) socket.write(part);
      socket.uncork();
    });
  }
}

/// The Next installed with the app: its version, and its own tag manifest (16 and later), where a
/// tag marked stale makes Next serve an entry once more while it makes it again.
function nextOf(dir) {
  const found = { major: 0, minor: 0, manifest: null };
  try {
    const require = createRequire(path.join(dir, 'package.json'));
    const [major, minor] = String(require('next/package.json').version).split('.').map(Number);
    found.major = major || 0;
    found.minor = minor || 0;
    if (found.major >= 16) {
      const manifest = require('next/dist/server/lib/incremental-cache/tags-manifest.external.js').tagsManifest;
      if (manifest instanceof Map) found.manifest = manifest;
    }
  } catch {}
  return found;
}

/// Make the two handlers on a connection to the runtime at `at` (or on `client`, for tests).
export function create({ at, client: given, memoryBytes = 16 << 20, next = nextOf(process.cwd()), now = Date.now, leaseMs = LEASE_MS } = {}) {
  /// Tag marks of this project and environment, as the router keeps them: tag → { s, d, p }.
  const marks = new Map();
  const state = { up: false, gen: 0, synced: false, marksNow: 0, offset: 0, syncing: null };
  /// Entries this process may serve without asking: class + key hash → { t, head, body, gen, size }.
  const memory = new Map();
  let memoryUsed = 0;
  const pendingSets = new Map();
  /// Tag invalidations not yet taken by the router.
  const unsent = [];
  const stats = { gets: 0, local: 0, hits: 0, sets: 0, tags: 0, failed: 0 };

  /// Stale entries this process is making again behind an answer where Next doesn't hand that
  /// work to `waitUntil` (pages on 13.5-14.2; `'use cache'` entries on every version): class and
  /// key hash → `finish`. The worker is held until each is written or its lease ends, whichever
  /// comes first (`holdForRegeneration`).
  const holds = new Map();
  const regeneratesUnheld = !(next.major >= 15);

  const routerNow = () => now() + state.offset;
  /// How far this process's `performance` clock is from the wall clock (positive: behind). Next 15
  /// and later read time from it. On Lambda it kept within a few ms of the wall clock across a
  /// freeze (measured 2026-11-17: 5.9 ms ahead after 220 s, 175 of them frozen); entries' own
  /// times are kept in wall time and handed to each process in its own.
  const lag = () => (next.major >= 15 ? Date.now() - (performance.timeOrigin + performance.now()) : 0);

  function mergeMark(tag, mark) {
    const have = marks.get(tag) ?? { s: 0, d: 0, p: [] };
    have.s = Math.max(have.s, mark.s ?? 0);
    have.d = Math.max(have.d, mark.d ?? 0);
    for (const pair of mark.p ?? []) {
      if (!have.p.some(([a, u]) => a === pair[0] && u === pair[1])) have.p.push(pair);
    }
    have.p = have.p.filter(([at]) => at > have.d).slice(-8);
    marks.set(tag, have);
  }

  function forget(key) {
    const kept = memory.get(key);
    if (kept) {
      memoryUsed -= kept.size;
      memory.delete(key);
    }
  }

  function remember(key, entry) {
    forget(key);
    if (entry.body.length > 1 << 20 || memoryBytes <= 0) return;
    entry.size = entry.body.length + 512;
    memory.set(key, entry);
    memoryUsed += entry.size;
    // The least recently used first: a Map keeps the order its keys were set in.
    for (const oldest of memory.keys()) {
      if (memoryUsed <= memoryBytes) break;
      forget(oldest);
    }
  }

  function onFrame(frame) {
    const { kind, head } = frame;
    if (kind === KIND.MARKS) {
      for (const mark of head.marks ?? []) mergeMark(mark.tag, mark);
      if (head.now) {
        state.marksNow = Math.max(state.marksNow, head.now);
        state.offset = head.now - now();
      }
      if (head.synced) state.synced = true;
    } else if (kind === KIND.CHANGED) {
      const key = `${head.c}:${head.k}`;
      const kept = memory.get(key);
      if (kept && kept.t < head.t) forget(key);
    } else if (kind === KIND.UP) {
      state.up = true;
      state.gen = head.gen ?? state.gen + 1;
      state.synced = false;
      state.syncing = client.request(KIND.SYNC, { since: state.marksNow }, [], TIMEOUT.sync).finally(() => (state.syncing = null));
      flushTags();
    } else if (kind === KIND.DOWN) {
      state.up = false;
      state.synced = false;
    }
  }

  const client = given ?? new Client(at, onFrame);
  if (given) given.on = onFrame;

  /// What the marks this process has say of an entry: 'deleted', 'stale' or null.
  function judgeTags(tags, made) {
    const time = routerNow();
    let verdict = judgeMark(marks.get('*'), made, time);
    for (const tag of tags) {
      if (verdict === 'deleted') break;
      const one = judgeMark(marks.get(tag), made, time);
      if (one) verdict = one === 'deleted' ? one : (verdict ?? one);
    }
    return verdict;
  }

  /// An entry, judged: `{ t, head, meta, parts, verdict }`, or null for none. `soft` are the
  /// reader's own tags, `rev` its own revalidate time when it has one.
  async function read(cls, keyHash, { soft = [], rev } = {}) {
    stats.gets++;
    const key = `${cls}:${keyHash}`;
    const kept = memory.get(key);
    if (kept && state.up && state.synced && kept.gen === state.gen) {
      const made = kept.meta.ts ?? kept.t;
      const life = seconds(rev) ?? seconds(kept.meta.rev);
      const byTime = life !== undefined && routerNow() > made + life * 1000;
      if (!byTime && judgeTags([...kept.head.tags, ...soft], made) === null) {
        // Used again: last in line to go.
        memory.delete(key);
        memory.set(key, kept);
        stats.local++;
        return { ...kept, verdict: 'fresh' };
      }
    }
    // Before the router can have granted a lease: a hold timed from here ends before the lease.
    const asked = now();
    const frame = await client.request(KIND.GET, { c: cls, k: keyHash, have: kept?.t, rev: seconds(rev), soft: soft.length ? soft : undefined }, [], TIMEOUT.get);
    if (!frame) {
      stats.failed++;
      return null;
    }
    const head = frame.head;
    if (head.now) state.offset = head.now - now();
    if (!head.hit) {
      forget(key);
      return null;
    }
    stats.hits++;
    if (head.same && kept) {
      kept.gen = state.gen;
      return { ...kept, verdict: head.verdict, asked };
    }
    // A copy of its own: the frame's bytes are the socket's.
    const body = Buffer.from(frame.body);
    const opened = openEntry(body);
    if (!opened) return null;
    const entry = { t: head.t, head: { status: head.status, tags: head.tags ?? [] }, meta: opened.meta, parts: opened.parts, body, gen: state.gen };
    remember(key, entry);
    return { ...entry, verdict: head.verdict, asked };
  }

  /// Keep an entry. Whether the router took it.
  async function write(cls, keyHash, key, entry) {
    stats.sets++;
    const meta = { kind: entry.kind, k: keyHash, key: key.length <= 1024 ? key : undefined, parts: entry.parts.map(([name, part]) => [name, part.length]), rev: entry.rev, exp: entry.exp, ts: entry.ts, stale: entry.stale, x: entry.x };
    const body = entryBody(meta, entry.parts.map(([, part]) => part));
    const size = body.reduce((n, b) => n + b.length, 0);
    if (size > (cls === 'p' || entry.kind === 'USE_CACHE' ? MAX_DATA_BYTES : MAX_PAGE_BYTES)) return false;
    const head = { c: cls, k: keyHash, status: entry.status, headers: entry.headers, tags: entry.tags, rev: entry.rev };
    const frame = await client.request(KIND.SET, head, body, TIMEOUT.set);
    if (!frame?.head?.ok) {
      stats.failed++;
      forget(`${cls}:${keyHash}`);
      return false;
    }
    if (frame.head.now) state.offset = frame.head.now - now();
    const whole = Buffer.concat(body);
    const opened = openEntry(whole);
    if (opened) remember(`${cls}:${keyHash}`, { t: frame.head.t, head: { status: entry.status, tags: entry.tags ?? [] }, meta: opened.meta, parts: opened.parts, body: whole, gen: state.gen });
    return true;
  }

  let lastTags = null;

  /// Next's `revalidateTag` / `updateTag` / `revalidatePath` (as soft tags): marked here at once
  /// (this process reads its own write), then with the router, which tells everyone else.
  function revalidate(tagList, durations) {
    const tags = [...new Set((Array.isArray(tagList) ? tagList : [tagList]).filter((tag) => typeof tag === 'string' && tag))];
    if (!tags.length) return Promise.resolve();
    const profile = durations != null && typeof durations === 'object';
    const expire = profile ? seconds(durations.expire) : undefined;
    // Next tells the page cache and the `'use cache'` handlers of one call each: sent once.
    const same = `${profile}:${expire}:${tags.join('\n')}`;
    if (lastTags && lastTags.same === same && now() - lastTags.at < 50) return lastTags.done;
    stats.tags++;
    const at = routerNow();
    const mark = !profile || expire === 0 ? { d: at } : expire === undefined ? { s: at } : { p: [[at, at + expire * 1000]] };
    for (const tag of tags) mergeMark(tag, mark);
    const ask = { tags, profile, expire };
    const done = send(ask).then((ok) => {
      if (!ok) {
        unsent.push({ ask, tries: 0 });
        retryTags();
      }
    });
    lastTags = { same, at: now(), done };
    return done;
  }

  async function send(ask) {
    const frame = await client.request(KIND.TAGS, ask, [], TIMEOUT.tags);
    return Boolean(frame?.head?.ok);
  }

  let retrying = null;

  function retryTags() {
    retrying ??= setTimeout(flushTags, TAGS_RETRY_MS);
    retrying.unref?.();
  }

  async function flushTags() {
    clearTimeout(retrying);
    retrying = null;
    for (const item of unsent.splice(0)) {
      if (await send(item.ask)) continue;
      if (++item.tries < TAGS_RETRIES) unsent.push(item);
      else console.error(`forkrail: a tag revalidation (${item.ask.tags.slice(0, 5).join(', ')}) couldn't be sent to the router`);
    }
    if (unsent.length) retryTags();
  }

  /// This process holds the lease to make a stale entry again (`lead`), and Next is about to serve
  /// the stale one and make the new one behind the answer. Next 13.5-14.2 does that outside any
  /// `waitUntil`, so nothing would keep the worker: the router could retire it (idle, or handed
  /// off) and the regeneration be cut off, the page stale for good. Held here with the runtime's
  /// own `waitUntil` (shim.mjs), which the router counts as work: until this key's `set` (the
  /// regeneration is done, written or not) or the lease's end, whichever comes first. Never past
  /// the lease: the hold is timed from before the request that won it. Next 15 and later hand the
  /// regeneration to `waitUntil` themselves: no hold of ours. A `'use cache'` entry is held on
  /// every version (`always`): Next hands its remake to `waitUntil` only if the function ran
  /// before the page began to stream (`executeRevalidates` takes the list then), and one inside a
  /// Suspense boundary runs after (seen 2026-11-18 on 16.4: the leader's worker retired, the remake
  /// was lost, and every other reader waited out the lease on the old value).
  function holdForRegeneration(key, asked, always = false) {
    const waitUntil = globalThis[Symbol.for('forkrail.waitUntil')];
    if ((!regeneratesUnheld && !always) || typeof waitUntil !== 'function' || holds.has(key)) return;
    const left = asked + leaseMs - now();
    if (!(left > 0)) return;
    let release;
    const done = new Promise((resolve) => (release = resolve));
    let timer = null;
    const finish = () => {
      if (holds.get(key) === finish) holds.delete(key);
      clearTimeout(timer);
      release();
    };
    timer = setTimeout(finish, left);
    timer.unref?.();
    holds.set(key, finish);
    try {
      waitUntil(done);
    } catch {
      finish();
    }
  }

  /// The entry being made again is written (or its write failed): the worker goes free.
  function released(key) {
    holds.get(key)?.();
  }

  /// A stale entry this reader is to make again, by a tag rather than by its time: Next only
  /// knows of the tag through its own manifest (16 and later). Whether Next was told.
  function tellNextStale(tags, lastModified) {
    if (!next.manifest || !tags.length) return false;
    const made = lastModified + lag();
    const tag = tags.find((one) => judgeMark(marks.get(one), made, routerNow()) === 'stale') ?? tags[0];
    const have = next.manifest.get(tag) ?? {};
    next.manifest.set(tag, { ...have, stale: Math.max(have.stale ?? 0, lastModified + 1) });
    return true;
  }

  // ---- `cacheHandler`: one class, made anew for every request ----

  class IncrementalCache {
    constructor(_ctx) {}

    resetRequestCache() {}

    async get(key, ctx = {}) {
      try {
        const kind = ctx.kind ?? (ctx.kindHint === 'fetch' || ctx.fetchCache ? 'FETCH' : undefined);
        const fetch = kind === 'FETCH';
        const soft = fetch ? [...(ctx.tags ?? []), ...(ctx.softTags ?? [])] : [];
        const found = await read(fetch ? 'p' : 'd', sha256(key), { soft, rev: fetch ? ctx.revalidate : undefined });
        if (!found) return null;
        const { meta, parts, verdict, t } = found;
        // Asked for as one kind, kept as another: not this reader's.
        if (fetch !== (meta.kind === 'FETCH')) return null;
        const value = toNext(meta, parts, ctx.kind !== undefined);
        const cacheControl = fetch || meta.kind === 'NULL' ? undefined : { revalidate: meta.rev ?? false, expire: meta.exp };
        if (verdict !== 'lead') {
          // Fresh, or being made again by another reader: Next is to serve it without making
          // it again, whatever it would work out from the entry's age.
          return { lastModified: Date.now(), value, cacheControl };
        }
        // This reader makes it again. Next sees that from the entry's age; when its time hasn't
        // passed (a tag was invalidated), from its tag manifest, or else it's told there's none.
        const lastModified = t - lag();
        const life = fetch ? (seconds(ctx.revalidate) ?? seconds(meta.rev)) : seconds(meta.rev);
        const byTime = life !== undefined && routerNow() > t + life * 1000;
        // Next reads a fetch's tags from the reader, an App Router entry's from the entry, and
        // a Pages Router page's not at all.
        const seen = fetch ? soft : meta.kind === 'APP_PAGE' || meta.kind === 'APP_ROUTE' ? found.head.tags : [];
        if (!byTime && !tellNextStale(seen, lastModified)) return null;
        holdForRegeneration(`${fetch ? 'p' : 'd'}:${sha256(key)}`, found.asked);
        return { lastModified, value, cacheControl };
      } catch (error) {
        console.error('forkrail: reading Next.js cache failed:', error);
        return null;
      }
    }

    async set(key, data, ctx = {}) {
      const fetch = data?.kind === 'FETCH';
      try {
        const entry = fromNext(data, ctx);
        await write(entry.kind === 'FETCH' ? 'p' : 'd', sha256(key), key, entry);
      } catch (error) {
        console.error('forkrail: writing Next.js cache failed:', error);
      } finally {
        released(`${fetch ? 'p' : 'd'}:${sha256(key)}`);
      }
    }

    async revalidateTag(tags, durations) {
      try {
        await revalidate(tags, durations);
      } catch (error) {
        console.error('forkrail: revalidating tags failed:', error);
      }
    }
  }

  // ---- `cacheHandlers`: `'use cache'` ----

  const useCache = {
    async get(cacheKey, softTags = []) {
      try {
        await pendingSets.get(cacheKey);
        const found = await read('d', sha256(cacheKey), { soft: softTags });
        if (!found || found.meta.kind !== 'USE_CACHE') return undefined;
        const { meta, parts, verdict } = found;
        const value = Buffer.from(parts.get('value') ?? '');
        // The entry's own time, in this process's clock.
        const timestamp = (meta.ts ?? found.t) - lag();
        let revalidate = meta.rev ?? Infinity;
        if (verdict === 'lead') {
          revalidate = -1;
          holdForRegeneration(`u:${sha256(cacheKey)}`, found.asked, true);
        } else if (verdict === 'wait') {
          // Another reader is making it again: not due here until that's had its time.
          const age = (performance.timeOrigin + performance.now() - timestamp) / 1000;
          revalidate = Math.max(revalidate, age + LEASE_MS / 1000);
        }
        return {
          value: new ReadableStream({
            start(controller) {
              controller.enqueue(value);
              controller.close();
            },
          }),
          tags: found.head.tags,
          stale: meta.stale ?? 0,
          timestamp,
          expire: meta.exp ?? Infinity,
          revalidate,
        };
      } catch (error) {
        console.error('forkrail: reading a cached function failed:', error);
        return undefined;
      }
    },

    async set(cacheKey, pendingEntry) {
      let done;
      const pending = new Promise((resolve) => (done = resolve));
      pendingSets.set(cacheKey, pending);
      try {
        const entry = await pendingEntry;
        const chunks = [];
        const reader = entry.value.getReader();
        try {
          for (;;) {
            const { done: ended, value } = await reader.read();
            if (ended) break;
            if (value) chunks.push(bytes(value));
          }
        } catch {
          // Cut short: an incomplete value would be an incomplete page.
          return;
        }
        await write('d', sha256(cacheKey), cacheKey, {
          kind: 'USE_CACHE',
          parts: [['value', Buffer.concat(chunks)]],
          x: {},
          tags: entry.tags ?? [],
          rev: seconds(entry.revalidate),
          exp: seconds(entry.expire),
          stale: seconds(entry.stale),
          ts: seconds(entry.timestamp) === undefined ? undefined : entry.timestamp + lag(),
        });
      } catch (error) {
        console.error('forkrail: writing a cached function failed:', error);
      } finally {
        if (pendingSets.get(cacheKey) === pending) pendingSets.delete(cacheKey);
        done();
        released(`u:${sha256(cacheKey)}`);
      }
    },

    /// Before a request's first read: the marks are sent as they change, so there's nothing to
    /// fetch; a stream that has just come up is given a moment to catch up.
    async refreshTags() {
      try {
        if (state.up && !state.synced && state.syncing) await Promise.race([state.syncing, new Promise((resolve) => setTimeout(resolve, 200).unref())]);
      } catch {}
    },

    /// The latest time any of `tags` (a route's soft tags) was revalidated. From Next 15.5 the
    /// answer is Infinity: "check them in `get`", where the router judges them with the entry's.
    async getExpiration(...args) {
      const tags = Array.isArray(args[0]) ? args[0] : args;
      if (next.major > 15 || (next.major === 15 && next.minor >= 5)) return Infinity;
      let latest = 0;
      for (const tag of tags) {
        const mark = marks.get(tag);
        if (mark) latest = Math.max(latest, mark.d ?? 0, ...(mark.p ?? []).filter(([, until]) => until <= routerNow()).map(([, until]) => until));
      }
      return latest ? latest - lag() : 0;
    },

    async updateTags(tags, durations) {
      try {
        await revalidate(tags, durations);
      } catch {}
    },

    // Next 15.0-15.5's names for the same.
    async expireTags(...tags) {
      return useCache.updateTags(tags.flat());
    },

    async unstable_expireTags(...tags) {
      return useCache.updateTags(tags.flat());
    },

    /// Tags another instance expired, as a request's headers carried them: marked here only.
    async receiveExpiredTags(...tags) {
      const at = routerNow();
      for (const tag of tags.flat()) if (typeof tag === 'string' && tag) mergeMark(tag, { d: at });
    },
  };

  return { IncrementalCache, useCache, state, marks, memory, holds, stats, read, write, revalidate, onFrame, flushTags };
}

const AT = process.env.FORKRAIL_NEXT_CACHE_AT ?? '';
delete process.env.FORKRAIL_NEXT_CACHE_AT;
if (AT) {
  const memoryMb = Number(process.env.FORKRAIL_NEXT_CACHE_MEMORY_MB);
  const api = create({ at: AT, memoryBytes: Number.isFinite(memoryMb) && process.env.FORKRAIL_NEXT_CACHE_MEMORY_MB ? memoryMb << 20 : undefined });
  globalThis[Symbol.for('forkrail.next-cache')] = api;
  // Next 15 and later take these when the project's config names no handler of its own.
  const handlers = Symbol.for('@next/cache-handlers');
  globalThis[handlers] = { ...globalThis[handlers], FetchCache: api.IncrementalCache, DefaultCache: api.useCache, RemoteCache: api.useCache };
}
