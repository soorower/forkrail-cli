// Tests of next-cache.mjs against a router of a few lines (the rules of the real one,
// `crates/router/src/cache/next.rs`): run with `node --test crates/runtime/src/`.

import assert from 'node:assert/strict';
import net from 'node:net';
import { test } from 'node:test';

import { FrameReader, STUB, create, encodeFrame, fromNext, toNext } from './next-cache.mjs';

const KIND = { GET: 1, SET: 2, TAGS: 3, SYNC: 4, REPLY: 16, MARKS: 17, CHANGED: 18, UP: 33, DOWN: 34 };
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/// A router: entries, marks and leases for one project, and the workers it tells of changes.
class Router {
  entries = new Map();
  marks = new Map();
  leases = new Map();
  workers = new Set();
  clock = 0;
  seen = [];
  down = false;

  now() {
    return Math.max(Date.now(), ++this.clock);
  }

  judge(tags, made, now) {
    let verdict = null;
    for (const tag of ['*', ...tags]) {
      const mark = this.marks.get(tag);
      if (!mark) continue;
      if (made < mark.d) return 'deleted';
      if (made < mark.s) verdict = 'stale';
      for (const [at, until] of mark.p) {
        if (made < at) {
          if (now >= until) return 'deleted';
          verdict = 'stale';
        }
      }
    }
    return verdict;
  }

  handle(worker, kind, head, body) {
    this.seen.push({ kind, head });
    const now = this.now();
    if (kind === KIND.GET) {
      const entry = this.entries.get(`${head.c}:${head.k}`);
      if (!entry) return [{ hit: false, now }];
      const made = entry.meta.ts ?? entry.t;
      const byTags = this.judge([...entry.tags, ...(head.soft ?? [])], made, now);
      if (byTags === 'deleted') return [{ hit: false, now }];
      const life = head.rev ?? entry.meta.rev;
      const stale = byTags === 'stale' || (life != null && now > made + life * 1000);
      let verdict = 'fresh';
      if (stale) {
        const key = `${head.c}:${head.k}`;
        verdict = (this.leases.get(key) ?? 0) > now ? 'wait' : 'lead';
        if (verdict === 'lead') this.leases.set(key, now + 60_000);
      }
      const same = head.have === entry.t;
      return [{ hit: true, same, verdict, t: entry.t, status: entry.status, tags: same ? [] : entry.tags, now }, same ? [] : [entry.body]];
    }
    if (kind === KIND.SET) {
      const length = body.readUInt32BE(0);
      const meta = JSON.parse(body.toString('utf8', 4, 4 + length));
      const t = now;
      const key = `${head.c}:${head.k}`;
      this.entries.set(key, { t, status: head.status ?? 200, tags: (head.tags ?? []).filter((tag) => !tag.includes(',')), meta, body: Buffer.from(body) });
      this.leases.delete(key);
      for (const other of this.workers) other.push(KIND.CHANGED, { c: head.c, k: head.k, t });
      return [{ ok: true, t, now }];
    }
    if (kind === KIND.TAGS) {
      const mark = !head.profile || head.expire === 0 ? { s: 0, d: now, p: [] } : head.expire == null ? { s: now, d: 0, p: [] } : { s: 0, d: 0, p: [[now, now + head.expire * 1000]] };
      const changed = [];
      for (const tag of head.tags) {
        const have = this.marks.get(tag) ?? { s: 0, d: 0, p: [] };
        this.marks.set(tag, { s: Math.max(have.s, mark.s), d: Math.max(have.d, mark.d), p: [...have.p, ...mark.p] });
        changed.push({ tag, ...mark });
      }
      for (const other of this.workers) other.push(KIND.MARKS, { marks: changed, now });
      return [{ ok: true, t: now, now }];
    }
    if (kind === KIND.SYNC) {
      worker.push(KIND.MARKS, { marks: [...this.marks].map(([tag, mark]) => ({ tag, ...mark })), synced: true, now });
      return [{ ok: true, now }];
    }
    return [{ error: 'unknown' }];
  }

  /// A worker's process on this router, its stream up: `create`'s handlers and the counters.
  worker(options = {}) {
    const router = this;
    const client = {
      on: null,
      push(kind, head) {
        // As frames arrive: after the code that's running.
        queueMicrotask(() => client.on?.({ kind, head, body: Buffer.alloc(0) }));
      },
      async request(kind, head, bodies) {
        await sleep(0);
        if (router.down) return null;
        // Through its own encoding and back, as on the wire.
        const [frame] = new FrameReader().push(Buffer.concat(encodeFrame(kind, 1, head, bodies)));
        const [reply, parts = []] = router.handle(client, frame.kind, frame.head, frame.body);
        if (reply.error) return null;
        const [back] = new FrameReader().push(Buffer.concat(encodeFrame(KIND.REPLY, 1, reply, parts)));
        return back;
      },
    };
    const api = create({ client, next: options.next ?? { major: 16, minor: 4, manifest: new Map() }, memoryBytes: options.memoryBytes, leaseMs: options.leaseMs });
    api.client = client;
    if (options.up !== false) {
      this.workers.add(client);
      api.onFrame({ kind: KIND.UP, head: { gen: 1 } });
    }
    return api;
  }
}

const appPage = (html, tags = '') => ({ kind: 'APP_PAGE', html, rscData: Buffer.from(`rsc:${html}`), headers: { 'content-type': 'text/html', 'x-next-cache-tags': tags, 'set-cookie': ['a=1', 'b=2'] }, status: 200, postponed: undefined, segmentData: new Map([['/_tree', Buffer.from('tree')]]) });
const fetched = (body, tags = []) => ({ kind: 'FETCH', data: { headers: { 'content-type': 'application/json' }, body, status: 200, url: 'https://api.example/x' }, revalidate: 60, tags });

test('entries of every kind come back as Next wrote them, in the asking Next\'s words', () => {
  const back = (data, ctx, modern = true) => {
    const entry = fromNext(data, ctx);
    return toNext({ kind: entry.kind, x: entry.x, rev: entry.rev }, new Map(entry.parts), modern);
  };
  const page = appPage('<p>hi</p>', 'posts,_N_T_/blog/layout');
  const entry = fromNext(page, { cacheControl: { revalidate: 60, expire: 3600 } });
  assert.deepEqual([entry.kind, entry.rev, entry.exp, entry.status], ['APP_PAGE', 60, 3600, 200]);
  assert.deepEqual(entry.tags, ['posts', '_N_T_/blog/layout']);
  assert.deepEqual(entry.parts.map(([name]) => name), ['html', 'rsc', 'seg:/_tree']);
  assert.deepEqual(entry.headers, [['content-type', 'text/html'], ['set-cookie', 'a=1'], ['set-cookie', 'b=2']], 'the page\'s own headers for the router, without Next\'s tag list');
  assert.deepEqual(back(page, {}), page);
  // 15.0 keeps segments in a plain object; a page that postponed keeps its state.
  const older = { ...page, segmentData: { '/_tree': Buffer.from('tree') }, postponed: '{"p":1}' };
  assert.deepEqual(back(older, {}), older);
  // Next 13.5-14.2: PAGE for both routers, ROUTE for a handler's answer.
  const appOld = { kind: 'PAGE', html: '<p>a</p>', pageData: '1:rsc', postponed: undefined, headers: { 'x-next-cache-tags': 'a' }, status: 200 };
  assert.equal(fromNext(appOld, { revalidate: 30 }).kind, 'APP_PAGE');
  assert.equal(fromNext(appOld, { revalidate: 30 }).rev, 30);
  assert.deepEqual(back(appOld, {}, false), appOld);
  const pagesOld = { kind: 'PAGE', html: '<p>p</p>', pageData: { props: { n: 1 } }, headers: undefined, status: undefined };
  assert.equal(fromNext(pagesOld).kind, 'PAGES');
  assert.deepEqual(back(pagesOld, {}, false), pagesOld);
  assert.deepEqual(back(pagesOld, {}, true), { ...pagesOld, kind: 'PAGES' }, 'and under 15\'s name for a 15 that asks');
  const route = { kind: 'APP_ROUTE', body: Buffer.from([0, 255, 10]), status: 201, headers: { 'content-type': 'application/octet-stream' } };
  assert.deepEqual(back(route, {}), route);
  assert.deepEqual(back({ ...route, kind: 'ROUTE' }, {}, false), { ...route, kind: 'ROUTE' });
  const data = fetched('eyJhIjoxfQ==', ['products']);
  assert.deepEqual(back(data, { tags: ['products'] }), data);
  assert.deepEqual(fromNext(data, { tags: ['products', 'extra'] }).tags, ['products', 'extra']);
  // unstable_cache of `undefined` keeps no body (Next reads `undefined` back as "no value"), and
  // an empty body stays an empty one.
  const nothing = { kind: 'FETCH', data: { headers: {}, body: undefined, status: 200, url: '' }, revalidate: 60, tags: [] };
  assert.deepEqual(back(nothing, {}), nothing);
  assert.equal(back({ ...nothing, data: { ...nothing.data, body: '' } }, {}).data.body, '');
  const redirect = { kind: 'REDIRECT', props: { pageProps: { __N_REDIRECT: '/x', __N_REDIRECT_STATUS: 307 } } };
  assert.deepEqual(back(redirect, {}), redirect);
  const image = { kind: 'IMAGE', etag: 'e', upstreamEtag: 'u', buffer: Buffer.from([1, 2, 3]), extension: 'webp', revalidate: 60 };
  assert.deepEqual(back(image, {}), image);
  assert.equal(back(null, {}), null);
  assert.equal(fromNext(null).kind, 'NULL');
  // A kind from a Next not yet written is kept whole.
  const unknown = { kind: 'FUTURE', bytes: Buffer.from('b'), list: new Map([['a', 1]]) };
  assert.deepEqual(back(unknown, {}), unknown);
  assert.match(STUB, /forkrail\.next-cache/);
});

test('frames in pieces, and one that is not a frame', () => {
  const wire = Buffer.concat([...encodeFrame(KIND.GET, 7, { k: 'a' }), ...encodeFrame(KIND.REPLY, 8, { hit: true }, [Buffer.from('body')])]);
  const reader = new FrameReader();
  const got = [];
  for (let i = 0; i < wire.length; i += 5) got.push(...reader.push(wire.subarray(i, i + 5)));
  assert.deepEqual(got.map((f) => [f.kind, f.id, f.head, f.body.toString()]), [[KIND.GET, 7, { k: 'a' }, ''], [KIND.REPLY, 8, { hit: true }, 'body']]);
  assert.throws(() => new FrameReader().push(Buffer.from([1, 0, 0, 0, 1, 255, 255, 255, 255])));
});

test('one environment\'s page is every environment\'s, and the data cache is kept apart', async () => {
  const router = new Router();
  const [a, b] = [router.worker(), router.worker()];
  const A = new a.IncrementalCache({});
  const B = new b.IncrementalCache({});
  assert.equal(await B.get('/blog/a', { kind: 'APP_PAGE' }), null);
  const page = appPage('<p>one</p>', 'posts');
  await A.set('/blog/a', page, { cacheControl: { revalidate: 60, expire: undefined } });
  const got = await B.get('/blog/a', { kind: 'APP_PAGE' });
  assert.deepEqual(got.value, page);
  assert.deepEqual(got.cacheControl, { revalidate: 60, expire: undefined });
  assert.ok(Math.abs(got.lastModified - Date.now()) < 1000, 'fresh: Next is to serve it as it is');
  // The same key in the data cache is another entry, and a page isn't a fetch.
  await A.set('k1', fetched('one', ['products']), { fetchCache: true, tags: ['products'] });
  assert.deepEqual((await B.get('k1', { kind: 'FETCH', tags: ['products'], softTags: [], revalidate: 60 })).value, fetched('one', ['products']));
  assert.equal(await B.get('k1', { kind: 'APP_PAGE' }), null);
  assert.equal(await B.get('/blog/a', { kind: 'FETCH' }), null);
  assert.equal(router.seen.find((s) => s.kind === KIND.SET && s.head.c === 'p').head.tags[0], 'products');
  // Next 14 says `kindHint`, 13.5 `fetchCache`: the same entries.
  assert.ok(await B.get('k1', { kindHint: 'fetch', tags: [], softTags: [] }));
  assert.ok(await B.get('k1', { fetchCache: true }));
  assert.equal((await B.get('/blog/a', { kindHint: 'app' })).value.kind, 'PAGE');
  assert.equal((await B.get('/blog/a')).value.kind, 'PAGE');
  // `notFound`: Next keeps "nothing", and gets it back as that.
  await A.set('/gone', null, {});
  assert.deepEqual(await B.get('/gone', { kind: 'PAGES' }), { lastModified: (await B.get('/gone', { kind: 'PAGES' })).lastModified, value: null, cacheControl: undefined });
});

test('a hot entry is read once: the process keeps it while its stream and marks are in step', async () => {
  const router = new Router();
  const [a, b] = [router.worker(), router.worker()];
  const A = new a.IncrementalCache({});
  const B = new b.IncrementalCache({});
  await A.set('/p', appPage('<p>one</p>', 'posts'), { cacheControl: { revalidate: 60 } });
  await sleep(5);
  const gets = () => router.seen.filter((s) => s.kind === KIND.GET).length;
  await B.get('/p', { kind: 'APP_PAGE' });
  const before = gets();
  for (let i = 0; i < 5; i++) assert.equal((await B.get('/p', { kind: 'APP_PAGE' })).value.html, '<p>one</p>');
  assert.equal(gets(), before, 'no round trip for the five');
  assert.equal(b.stats.local, 5);
  // Written again by the other environment (Pages Router's res.revalidate() has no tag to tell
  // by): the router's note drops the copy, and the next read is the new one.
  await A.set('/p', appPage('<p>two</p>', 'posts'), { cacheControl: { revalidate: 60 } });
  await sleep(5);
  assert.equal((await B.get('/p', { kind: 'APP_PAGE' })).value.html, '<p>two</p>');
  // A new invocation's stream: what was kept is checked once, without its body coming again.
  b.onFrame({ kind: KIND.UP, head: { gen: 2 } });
  await sleep(5);
  const again = await B.get('/p', { kind: 'APP_PAGE' });
  assert.equal(again.value.html, '<p>two</p>');
  const last = router.seen.filter((s) => s.kind === KIND.GET).at(-1);
  assert.ok(last.head.have > 0, 'asked with the copy it has');
  const settled = gets();
  await B.get('/p', { kind: 'APP_PAGE' });
  assert.equal(gets(), settled, 'and trusted again after');
  // No stream (the worker was retired, its app works on): every read asks the router.
  b.onFrame({ kind: KIND.DOWN, head: {} });
  await B.get('/p', { kind: 'APP_PAGE' });
  assert.equal(gets(), settled + 1);
  // Without a memory, every read asks.
  const none = router.worker({ memoryBytes: 0 });
  const N = new none.IncrementalCache({});
  await N.get('/p', { kind: 'APP_PAGE' });
  await N.get('/p', { kind: 'APP_PAGE' });
  assert.equal(none.stats.local, 0);
});

test('revalidateTag in one environment is seen by all, and by its own next read', async () => {
  const router = new Router();
  const [a, b] = [router.worker(), router.worker()];
  const A = new a.IncrementalCache({});
  const B = new b.IncrementalCache({});
  await A.set('/p', appPage('<p>one</p>', 'posts,_N_T_/p'), { cacheControl: { revalidate: false } });
  await A.set('k1', fetched('one', ['products']), { fetchCache: true, tags: ['products'] });
  await sleep(5);
  assert.ok(await B.get('/p', { kind: 'APP_PAGE' }));
  assert.ok(await A.get('k1', { kind: 'FETCH', tags: ['products'], softTags: ['_N_T_/p'] }));
  // updateTag / revalidateTag(tag): gone, for the caller at once and for the other without asking.
  await sleep(2);
  await A.revalidateTag('posts');
  assert.equal(await A.get('/p', { kind: 'APP_PAGE' }), null, 'read-your-own-write');
  await sleep(5);
  assert.equal(await B.get('/p', { kind: 'APP_PAGE' }), null);
  assert.ok(b.marks.get('posts').d > 0, 'the mark was sent to the other environment, unasked');
  // revalidatePath: a soft tag of the reader's route judges a fetch that doesn't carry it.
  await B.revalidateTag(['_N_T_/p']);
  assert.equal(await A.get('k1', { kind: 'FETCH', tags: ['products'], softTags: ['_N_T_/p'] }), null);
  assert.ok(await A.get('k1', { kind: 'FETCH', tags: ['products'], softTags: ['_N_T_/other'] }), 'another route still reads it');
  // Written after the invalidation: fresh again.
  await sleep(2);
  await B.set('/p', appPage('<p>two</p>', 'posts,_N_T_/p'), { cacheControl: { revalidate: false } });
  assert.equal((await A.get('/p', { kind: 'APP_PAGE' })).value.html, '<p>two</p>');
  // Next calls the page cache and the 'use cache' handlers for one revalidateTag: sent once.
  const sent = () => router.seen.filter((s) => s.kind === KIND.TAGS).length;
  const before = sent();
  await Promise.all([A.revalidateTag(['x']), a.useCache.updateTags(['x'])]);
  assert.equal(sent(), before + 1);
  // Older Nexts: one tag at a time, and 15's expireTags.
  await sleep(60);
  await A.revalidateTag('y');
  await a.useCache.expireTags('z1', 'z2');
  assert.deepEqual(router.seen.filter((s) => s.kind === KIND.TAGS).slice(-2).map((s) => s.head.tags), [['y'], ['z1', 'z2']]);
});

test('a stale entry is made again by one reader; the others are served it meanwhile', async () => {
  const router = new Router();
  const manifest = new Map();
  const [a, b] = [router.worker({ next: { major: 16, minor: 4, manifest } }), router.worker()];
  const A = new a.IncrementalCache({});
  const B = new b.IncrementalCache({});
  // By its time.
  await A.set('/t', appPage('<p>one</p>', 'posts'), { cacheControl: { revalidate: 0.02 } });
  await sleep(40);
  const lead = await A.get('/t', { kind: 'APP_PAGE' });
  assert.ok(lead.lastModified < Date.now() - 20, 'the leader is given the entry\'s own time: Next finds it stale and makes it again');
  const wait = await B.get('/t', { kind: 'APP_PAGE' });
  assert.ok(Math.abs(wait.lastModified - Date.now()) < 1000, 'the other is given now: Next serves it and makes nothing');
  assert.equal(wait.value.html, '<p>one</p>');
  await A.set('/t', appPage('<p>two</p>', 'posts'), { cacheControl: { revalidate: 60 } });
  await sleep(5);
  assert.equal((await B.get('/t', { kind: 'APP_PAGE' })).value.html, '<p>two</p>');
  // By a tag invalidated with a profile (Next 16): the leader's Next learns it from its manifest.
  await sleep(2);
  await B.revalidateTag(['posts'], { expire: 300 });
  await sleep(5);
  const led = await A.get('/t', { kind: 'APP_PAGE' });
  assert.ok(led && manifest.get('posts').stale > led.lastModified, 'Next\'s own check now says stale');
  assert.ok(Math.abs((await B.get('/t', { kind: 'APP_PAGE' })).lastModified - Date.now()) < 1000);
  // A Next without that manifest (before 16) can't be told: the leader gets a miss and renders.
  const old = router.worker({ next: { major: 14, minor: 2, manifest: null } });
  const O = new old.IncrementalCache({});
  await O.set('/o', { kind: 'PAGE', html: '<p>o</p>', pageData: '1:x', headers: { 'x-next-cache-tags': 'posts' }, status: 200 }, { revalidate: false });
  await sleep(2);
  await A.revalidateTag(['posts'], {});
  await sleep(5);
  assert.equal(await O.get('/o', { kindHint: 'app' }), null);
  assert.ok(await B.get('/o', { kind: 'APP_PAGE' }), 'while the others are still served it');
  // A fetch stale by the reader's own revalidate time.
  await A.set('f', fetched('one'), { fetchCache: true, tags: [] });
  await sleep(30);
  const stale = await A.get('f', { kind: 'FETCH', revalidate: 0.01, tags: [], softTags: [] });
  assert.ok(stale.lastModified < Date.now() - 20);
  assert.ok(Math.abs((await B.get('f', { kind: 'FETCH', revalidate: 0.01, tags: [], softTags: [] })).lastModified - Date.now()) < 1000);
});

/// The runtime's `waitUntil` (shim.mjs), as the handler finds it: each promise handed, and
/// whether it has settled.
function runtimeWaitUntil() {
  const handed = [];
  globalThis[Symbol.for('forkrail.waitUntil')] = (promise) => {
    const one = { settled: false, at: Date.now(), ended: null };
    handed.push(one);
    promise.then(() => {
      one.settled = true;
      one.ended = Date.now();
    });
  };
  return { handed, open: () => handed.filter((h) => !h.settled).length, restore: () => delete globalThis[Symbol.for('forkrail.waitUntil')] };
}

test('Next 13.5-14.2: the worker is held while it makes a stale page again, until the set or the lease ends', async () => {
  const router = new Router();
  const v14 = { major: 14, minor: 2, manifest: null };
  const wait = runtimeWaitUntil();
  try {
    const [a, b] = [router.worker({ next: v14, leaseMs: 300 }), router.worker({ next: v14, leaseMs: 300 })];
    const A = new a.IncrementalCache({});
    const B = new b.IncrementalCache({});
    const page = (html) => ({ kind: 'PAGE', html, pageData: '1:x', headers: { 'x-next-cache-tags': 'posts' }, status: 200 });
    await A.set('/isr', page('<p>one</p>'), { revalidate: 0.02 });
    await sleep(40);
    // The reader that leads gets the stale page (Next serves it and makes it again behind the
    // answer) and the worker is held; the other is served it and holds nothing.
    const led = await A.get('/isr', { kindHint: 'app' });
    assert.ok(led.lastModified < Date.now() - 20, 'the leader is given the entry\'s own time');
    assert.deepEqual([wait.handed.length, wait.open()], [1, 1]);
    assert.equal(a.holds.size, 1);
    assert.ok(Math.abs((await B.get('/isr', { kindHint: 'app' })).lastModified - Date.now()) < 1000);
    assert.equal(wait.handed.length, 1, 'no hold for a reader that waits');
    // Read again by the same worker while it's being made: no second hold.
    await A.get('/isr', { kindHint: 'app' });
    assert.equal(wait.handed.length, 1);
    // The regeneration writes it: let go at once.
    await A.set('/isr', page('<p>two</p>'), { revalidate: 60 });
    await sleep(0);
    assert.deepEqual([wait.open(), a.holds.size], [0, 0]);
    assert.ok(wait.handed[0].ended - wait.handed[0].at < 200, 'released on the set, well before the lease ends');
    // Fresh: nothing held.
    await A.get('/isr', { kindHint: 'app' });
    assert.equal(wait.handed.length, 1);

    // A regeneration that never writes (it failed): let go when the lease ends, not after.
    await A.set('/gone', page('<p>g</p>'), { revalidate: 0.02 });
    await sleep(40);
    const started = Date.now();
    assert.ok(await B.get('/gone', { kindHint: 'app' }));
    assert.deepEqual([wait.handed.length, wait.open()], [2, 1]);
    await sleep(150);
    assert.equal(wait.open(), 1, 'still held within the lease');
    await sleep(250);
    assert.equal(wait.open(), 0, 'and let go by its end');
    assert.ok(wait.handed[1].ended - started <= 310, `held ${wait.handed[1].ended - started} ms for a 300 ms lease`);
    assert.equal(b.holds.size, 0);

    // A fetch the reader finds stale by its own time: the same, released by the fetch's set.
    await A.set('f1', fetched('one'), { fetchCache: true, revalidate: 60, tags: [] });
    await sleep(30);
    const stale = await B.get('f1', { kindHint: 'fetch', revalidate: 0.01, tags: [], softTags: [] });
    assert.ok(stale.lastModified < Date.now() - 20);
    assert.equal(wait.open(), 1);
    await B.set('f1', fetched('two'), { fetchCache: true, revalidate: 0.01, tags: [] });
    assert.equal(wait.open(), 0);

    // Stale by a tag on 14.2: Next can't be told, so the leader gets a miss and renders while the
    // visitor waits, in the request: nothing to hold.
    await A.set('/tagged', page('<p>t</p>'), { revalidate: false });
    await sleep(3);
    await A.revalidateTag(['posts'], {});
    await sleep(5);
    assert.equal(await B.get('/tagged', { kindHint: 'app' }), null);
    assert.equal(wait.handed.length, 3);
  } finally {
    wait.restore();
  }
});

test('Next 15 and later hand the regeneration to waitUntil themselves: no hold of ours', async () => {
  const router = new Router();
  const wait = runtimeWaitUntil();
  try {
    for (const next of [{ major: 15, minor: 5, manifest: null }, { major: 16, minor: 4, manifest: new Map() }]) {
      const a = router.worker({ next, leaseMs: 300 });
      const A = new a.IncrementalCache({});
      const key = `/isr-${next.major}`;
      await A.set(key, appPage('<p>one</p>'), { cacheControl: { revalidate: 0.02 } });
      await sleep(40);
      const led = await A.get(key, { kind: 'APP_PAGE' });
      assert.ok(led.lastModified < Date.now() - 20, 'it leads');
      assert.equal(a.holds.size, 0);
    }
    assert.equal(wait.handed.length, 0);
  } finally {
    wait.restore();
  }
});

test("'use cache' on every Next: the worker that leads a remake is held until its set or the lease's end", async () => {
  const router = new Router();
  const wait = runtimeWaitUntil();
  try {
    const next = { major: 16, minor: 4, manifest: new Map() };
    const [a, b] = [router.worker({ next, leaseMs: 300 }), router.worker({ next, leaseMs: 300 })];
    const stream = (text) => new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode(text)); c.close(); } });
    const entry = (text) => Promise.resolve({ value: stream(text), tags: ['products'], stale: 30, timestamp: performance.timeOrigin + performance.now(), expire: 3600, revalidate: 60 });
    await a.useCache.set('["build","fn",[]]', entry('one'));
    await sleep(2);
    await a.useCache.updateTags(['products'], { expire: 300 });
    await sleep(5);
    // Next 16.4 leaves this remake out of waitUntil when the function runs inside a Suspense
    // boundary: the leader's worker is held by the handler itself; the reader that waits isn't.
    assert.equal((await b.useCache.get('["build","fn",[]]', [])).revalidate, -1, 'b leads');
    assert.deepEqual([wait.handed.length, wait.open(), b.holds.size], [1, 1, 1]);
    assert.ok((await a.useCache.get('["build","fn",[]]', [])).revalidate >= 60, 'a waits');
    assert.deepEqual([wait.handed.length, a.holds.size], [1, 0]);
    // The remake is written: let go at once.
    await b.useCache.set('["build","fn",[]]', entry('two'));
    await sleep(0);
    assert.deepEqual([wait.open(), b.holds.size], [0, 0]);
    assert.ok(wait.handed[0].ended - wait.handed[0].at < 200, 'released on the set');
    // A remake that never writes (it failed): let go when the lease ends, not after.
    await a.useCache.set('["build","gone",[]]', Promise.resolve({ value: stream('g'), tags: ['gone'], stale: 30, timestamp: performance.timeOrigin + performance.now(), expire: 3600, revalidate: 60 }));
    await sleep(2);
    await a.useCache.updateTags(['gone'], { expire: 300 });
    await sleep(5);
    const started = Date.now();
    assert.equal((await a.useCache.get('["build","gone",[]]', [])).revalidate, -1, 'a leads');
    assert.deepEqual([wait.handed.length, wait.open()], [2, 1]);
    await sleep(400);
    assert.equal(wait.open(), 0, 'let go by the lease\'s end');
    assert.ok(wait.handed[1].ended - started <= 310, `held ${wait.handed[1].ended - started} ms for a 300 ms lease`);
    assert.equal(a.holds.size, 0);
  } finally {
    wait.restore();
  }
});

test("'use cache' entries: pending sets, tags, soft tags and one regeneration", async () => {
  const router = new Router();
  const [a, b] = [router.worker(), router.worker()];
  const stream = (text) => new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode(text)); c.close(); } });
  const text = async (entry) => Buffer.from(await new Response(entry.value).arrayBuffer()).toString();
  const made = performance.timeOrigin + performance.now();
  let release;
  const pending = new Promise((resolve) => (release = resolve));
  const setting = a.useCache.set('["build","fn",[]]', pending);
  const reading = a.useCache.get('["build","fn",[]]', []);
  release({ value: stream('cached'), tags: ['products'], stale: 30, timestamp: made, expire: 3600, revalidate: 60 });
  await setting;
  const own = await reading;
  assert.equal(await text(own), 'cached', 'a get waits for the set of the same key');
  const got = await b.useCache.get('["build","fn",[]]', ['_N_T_/shop']);
  assert.deepEqual([got.tags, got.stale, got.expire, got.revalidate], [['products'], 30, 3600, 60]);
  assert.ok(Math.abs(got.timestamp - made) < 50, 'its own time, in this process\'s clock');
  assert.equal(await text(got), 'cached');
  assert.equal(await text(await b.useCache.get('["build","fn",[]]', [])), 'cached', 'a new stream on every read');
  assert.equal(await b.useCache.get('["build","other",[]]', []), undefined);
  // 16 is told to leave soft tags to `get`; 15.3 gets the time itself.
  assert.equal(await a.useCache.getExpiration(['_N_T_/shop']), Infinity);
  await a.useCache.refreshTags();
  // revalidatePath of the reader's route: a miss there, still an entry for another route.
  await sleep(2);
  await a.useCache.updateTags(['_N_T_/shop']);
  await sleep(5);
  assert.equal(await b.useCache.get('["build","fn",[]]', ['_N_T_/shop']), undefined);
  assert.ok(await b.useCache.get('["build","fn",[]]', ['_N_T_/other']));
  const old = router.worker({ next: { major: 15, minor: 3, manifest: null } });
  await sleep(5);
  assert.ok((await old.useCache.getExpiration('_N_T_/shop', '_N_T_/x')) > 0);
  assert.equal(await old.useCache.getExpiration('_N_T_/x'), 0);
  // With a profile: the first reader is told to refresh (-1), the other isn't due yet.
  await a.useCache.updateTags(['products'], { expire: 300 });
  await sleep(5);
  assert.equal((await a.useCache.get('["build","fn",[]]', [])).revalidate, -1);
  assert.ok((await b.useCache.get('["build","fn",[]]', [])).revalidate >= 60);
  // A stream that fails midway keeps nothing; an entry over 2 MB isn't kept.
  const broken = new ReadableStream({ start(c) { c.enqueue(new Uint8Array(3)); c.error(new Error('cut')); } });
  await a.useCache.set('broken', Promise.resolve({ value: broken, tags: [], stale: 0, timestamp: made, expire: 60, revalidate: 60 }));
  assert.equal(await b.useCache.get('broken', []), undefined);
  await a.useCache.set('big', Promise.resolve({ value: stream('x'.repeat((2 << 20) + 1)), tags: [], stale: 0, timestamp: made, expire: 60, revalidate: 60 }));
  assert.equal(await b.useCache.get('big', []), undefined);
});

test('the router away: reads miss, writes are dropped, tag invalidations are kept and sent', async () => {
  const router = new Router();
  const a = router.worker();
  const A = new a.IncrementalCache({});
  await A.set('/p', appPage('<p>one</p>', 'posts'), { cacheControl: { revalidate: 60 } });
  router.down = true;
  a.onFrame({ kind: KIND.DOWN, head: {} });
  assert.equal(await A.get('/p', { kind: 'APP_PAGE' }), null);
  await A.set('/q', appPage('<p>q</p>'), { cacheControl: { revalidate: 60 } });
  assert.equal(await a.useCache.get('k', []), undefined);
  await a.useCache.set('k', Promise.resolve({ value: new ReadableStream({ start(c) { c.close(); } }), tags: [], stale: 0, timestamp: 1, expire: 60, revalidate: 60 }));
  await A.revalidateTag('posts');
  assert.equal(router.marks.size, 0);
  // Back: the invalidation made meanwhile reaches the router; the dropped write never does.
  router.down = false;
  a.onFrame({ kind: KIND.UP, head: { gen: 2 } });
  await sleep(20);
  assert.ok(router.marks.get('posts').d > 0);
  assert.equal(router.entries.has('d:' + [...router.entries.keys()].find((k) => k.includes('/q'))), false);
  assert.equal(await A.get('/p', { kind: 'APP_PAGE' }), null, 'and what it invalidated is gone');
  assert.ok(a.stats.failed >= 3);
});

test('over a real connection to the runtime: the secret first, answers by id, a dead runtime is a miss', async () => {
  const seen = [];
  const sockets = [];
  const server = net.createServer((socket) => {
    sockets.push(socket);
    const reader = new FrameReader();
    socket.on('data', (chunk) => {
      for (const frame of reader.push(chunk)) {
        seen.push(frame);
        if (frame.kind === KIND.GET) {
          // Out of order, to check ids pair answers.
          setTimeout(() => socket.write(Buffer.concat(encodeFrame(KIND.REPLY, frame.id, { hit: false, now: Date.now() }))), frame.head.k.endsWith('1') ? 30 : 0);
        }
      }
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const api = create({ at: `127.0.0.1:${server.address().port}:s3cret`, next: { major: 16, minor: 0, manifest: null } });
  const [one, two] = await Promise.all([api.read('d', 'k1'), api.read('d', 'k2')]);
  assert.deepEqual([one, two], [null, null]);
  assert.deepEqual([seen[0].kind, seen[0].head.secret], [32, 's3cret']);
  assert.equal(seen.filter((f) => f.kind === KIND.GET).length, 2);
  for (const socket of sockets) socket.destroy();
  await new Promise((resolve) => server.close(resolve));
  // Nothing listening: a miss, quickly, and no throw.
  const dead = create({ at: '127.0.0.1:9:x', next: { major: 16, minor: 0, manifest: null } });
  const started = Date.now();
  assert.equal(await new dead.IncrementalCache({}).get('/p', { kind: 'APP_PAGE' }), null);
  assert.ok(Date.now() - started < 900);
});
