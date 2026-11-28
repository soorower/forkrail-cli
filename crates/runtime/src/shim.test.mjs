// Isolation between concurrent requests in one Node app under the shim (shim.mjs), as the runtime
// runs it: one process, requests over pooled HTTP/1.1 connections, the control fd on 3.
//
// What each request must see of itself only, however many are in flight: Vercel's request context
// (`@vercel/request-context`: headers, url, waitUntil) across timers, immediates, ticks, fetch,
// promises another request made, streaming, and work that outlives its response (`waitUntil`, a
// timer); its own lines tagged with its own request id; its outgoing calls reported with it.
//
// Run: node --test crates/runtime/src/shim.test.mjs

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

// FORKRAIL_TEST_SHIM: another shim to run under (to see these tests fail on a broken one).
const SHIM = process.env.FORKRAIL_TEST_SHIM ?? fileURLToPath(new URL('./shim.mjs', import.meta.url));
const IN_FLIGHT = 200;
const ABORTED_EVERY = 10;

async function freePort() {
  const server = http.createServer();
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  await new Promise((resolve) => server.close(resolve));
  return port;
}

// The app in `files` (name -> source), started as the runtime starts it.
async function start(files, entry, env = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'forkrail-shim-test-'));
  for (const [name, source] of Object.entries(files)) writeFileSync(join(dir, name), source);
  const port = await freePort();
  const child = spawn(process.execPath, ['--import', SHIM, entry], {
    cwd: dir,
    env: { ...process.env, ...env, PORT: String(port), FORKRAIL_CONTROL_FD: '3' },
    stdio: ['ignore', 'pipe', 'pipe', 'pipe'],
  });
  const out = { stdout: '', stderr: '', control: [] };
  child.stdout.on('data', (d) => (out.stdout += d));
  child.stderr.on('data', (d) => (out.stderr += d));
  let partial = '';
  const listening = new Promise((resolve, reject) => {
    child.stdio[3].on('data', (d) => {
      const lines = (partial + d).split('\n');
      partial = lines.pop();
      for (const line of lines) {
        const message = JSON.parse(line);
        out.control.push(message);
        if (message.type === 'listening') {
          out.replies = message.replies;
          resolve(message.port);
        }
      }
    });
    child.on('exit', (code) => reject(new Error(`app exited (${code}): ${out.stderr}`)));
  });
  let appPort;
  try {
    appPort = await listening;
  } catch (e) {
    child.kill('SIGKILL');
    rmSync(dir, { recursive: true, force: true });
    throw e;
  }
  return {
    port: appPort,
    out,
    stop() {
      child.kill('SIGKILL');
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

// Requests on pooled keep-alive connections, as the runtime sends them.
const agent = new http.Agent({ keepAlive: true, maxSockets: 40 });

function ask(port, { path, headers, body, abort }) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, path, method: body === undefined ? 'GET' : 'POST', headers, agent }, (res) => {
      if (abort) {
        // The client leaves once the answer has started.
        res.destroy();
        return resolve(null);
      }
      let text = '';
      res.setEncoding('utf8');
      res.on('data', (d) => (text += d));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, text }));
      res.on('error', reject);
    });
    req.on('error', (e) => (abort ? resolve(null) : reject(e)));
    if (body !== undefined) req.end(body);
    else req.end();
  });
}

// An upstream the app calls, which notes what each call carried.
async function upstream() {
  const seen = [];
  const server = http.createServer((req, res) => {
    seen.push(new URL(req.url, 'http://x').searchParams.get('id'));
    setTimeout(() => res.end('ok'), Math.random() * 10);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { url: `http://127.0.0.1:${server.address().port}`, seen, close: () => server.close() };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const rid = (i) => `rid${i.toString(16).padStart(6, '0')}`;
const tid = (i) => `user${i}`;

// Lines the shim tagged: [request id, text]; untagged lines as [null, text].
function tagged(text) {
  return text
    .split('\n')
    .filter((l) => l.length)
    .map((l) => {
      const m = /^\x1e([A-Za-z0-9_-]+)\x1f(.*)$/s.exec(l);
      return m ? [m[1], m[2]] : [null, l];
    });
}

// The checks on what the app reported of each request (`seen`), its lines and its calls.
function checkRun(app, answers, up, { functions = false } = {}) {
  const idOf = new Map();
  for (let i = 0; i < IN_FLIGHT; i++) idOf.set(rid(i), tid(i));
  for (let i = 0; i < IN_FLIGHT; i++) {
    const a = answers[i];
    if (i % ABORTED_EVERY === 0) continue;
    assert.equal(a.status, 200, a.text);
    const got = JSON.parse(a.text);
    assert.equal(got.id, tid(i));
    assert.equal(got.body, `body-${tid(i)}`, 'its own body');
    assert.equal(got.cookie, `sid=${tid(i)}`, 'its own cookie');
    if (functions) assert.equal(got.sawFunctionHeader, false, 'the router header is taken off before the function');
    for (const s of got.seen) {
      assert.equal(s.id, tid(i), `request ${tid(i)} saw ${s.id} at ${s.where}`);
      assert.equal(s.cookie, `sid=${tid(i)}`, `request ${tid(i)} saw cookie ${s.cookie} at ${s.where}`);
      assert.ok(s.url.includes(`/${tid(i)}?`), `request ${tid(i)} saw url ${s.url} at ${s.where}`);
    }
  }
  // Every line written while a request ran carries that request's id, and only its own words.
  const counts = new Map();
  for (const [stream, text] of [['stdout', app.out.stdout], ['stderr', app.out.stderr]]) {
    for (const [id, line] of tagged(text)) {
      if (id === null) continue;
      assert.ok(idOf.has(id), `unknown tag ${id}`);
      const words = line.split(' ');
      const kind = words[0];
      assert.ok(['line', 'half', 'wait', 'late', 'err'].includes(kind), `${stream}: ${JSON.stringify(line)}`);
      // `line X`, `err X`, `half X whole X`, `wait X <context>`, `late X <context>`.
      for (const word of words.slice(1).filter((w) => w.startsWith('user'))) {
        assert.equal(word, idOf.get(id), `${stream} line ${JSON.stringify(line)} tagged ${id}`);
      }
      counts.set(`${kind} ${id}`, (counts.get(`${kind} ${id}`) ?? 0) + 1);
    }
  }
  for (let i = 0; i < IN_FLIGHT; i++) {
    for (const kind of ['line', 'half', 'wait', 'late', 'err']) {
      assert.equal(counts.get(`${kind} ${rid(i)}`), 1, `${kind} of ${tid(i)}: ${counts.get(`${kind} ${rid(i)}`)}`);
    }
  }
  // Each outgoing call is reported with the request that made it.
  const fetches = app.out.control.filter((m) => m.type === 'fetch');
  assert.equal(fetches.length, IN_FLIGHT);
  for (const f of fetches) assert.ok(f.url.endsWith(`?id=${idOf.get(f.request)}`), `call ${f.url} reported with ${f.request}`);
  assert.deepEqual([...up.seen].sort(), [...idOf.values()].sort());
  // `waitUntil` work all settled, and nothing was uncaught.
  const waits = app.out.control.filter((m) => m.type === 'wait').reduce((n, m) => n + m.delta, 0);
  assert.equal(waits, 0);
  assert.deepEqual(app.out.control.filter((m) => m.type === 'uncaught'), []);
}

async function hammer(port, prefix = '') {
  return Promise.all(
    Array.from({ length: IN_FLIGHT }, (_, i) =>
      ask(port, {
        path: `${prefix}/${tid(i)}?n=${i}`,
        headers: { 'x-forkrail-request-id': rid(i), 'x-test-id': tid(i), cookie: `sid=${tid(i)}`, 'content-type': 'text/plain', ...(prefix ? { 'x-forkrail-function': prefix } : {}) },
        body: `body-${tid(i)}`,
        abort: i % ABORTED_EVERY === 0,
      }),
    ),
  );
}

// What each request does: everything that hops async contexts, reading the request context after
// each hop. `seen` goes back in the answer; lines and calls are checked from the outside.
const WORK = `
const context = () => globalThis[Symbol.for('@vercel/request-context')].get();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const jitter = () => sleep(Math.random() * 15);
let started = null;
export async function work(id, url, body, write, end, setStatus) {
  const seen = [];
  const look = (where) => {
    const c = context();
    seen.push({ where, id: c.headers?.['x-test-id'], cookie: c.headers?.cookie, url: c.url ?? '' });
  };
  look('start');
  await jitter(); look('timeout');
  await new Promise((r) => setImmediate(r)); look('immediate');
  await new Promise((r) => process.nextTick(r)); look('tick');
  await Promise.resolve(); look('microtask');
  await (await fetch(process.env.UPSTREAM + '/?id=' + id)).text(); look('fetch');
  // A promise another request started: awaiting it comes back to this request's context.
  const theirs = started;
  started = sleep(5);
  if (theirs) { await theirs; look('awaited-another'); }
  context().waitUntil((async () => { await sleep(20 + Math.random() * 40); console.log('wait ' + id + ' ' + context().headers?.['x-test-id']); })());
  setTimeout(() => console.log('late ' + id + ' ' + context().headers?.['x-test-id']), 30 + Math.random() * 60);
  console.log('line ' + id);
  process.stdout.write('half ' + id + ' ');
  await jitter();
  process.stdout.write('whole ' + id + '\\n');
  console.error('err ' + id);
  setStatus();
  for (let i = 0; i < 3; i++) { await jitter(); look('stream' + i); if (!(await write(' '))) break; }
  await end(JSON.stringify({ id, body, cookie: context().headers?.cookie, seen, url }));
}
`;

test('requests in flight together in one app each see only their own context, lines and calls', async () => {
  const up = await upstream();
  const app = await start(
    {
      'work.mjs': WORK,
      'server.mjs': `
        import http from 'node:http';
        import { work } from './work.mjs';
        http.createServer(async (req, res) => {
          let body = '';
          for await (const chunk of req) body += chunk;
          await work(req.headers['x-test-id'], req.url, body,
            (t) => new Promise((r) => res.write(t, () => r(!res.destroyed))),
            (t) => new Promise((r) => res.end(t, r)),
            () => res.writeHead(200, { 'content-type': 'application/json' }));
        }).listen(Number(process.env.PORT), '127.0.0.1');
      `,
    },
    'server.mjs',
    { UPSTREAM: up.url },
  );
  try {
    const answers = await hammer(app.port);
    await sleep(400);
    checkRun(app, answers, up);
  } finally {
    app.stop();
    up.close();
  }
});

test('a Build Output of two functions in one process: each request sees only its own, whichever function', async () => {
  const up = await upstream();
  const web = `
    import { work } from './work.mjs';
    export default {
      async fetch(request) {
        const body = await request.text();
        const stream = new TransformStream();
        const writer = stream.writable.getWriter();
        let status;
        const done = work(request.headers.get('x-test-id'), new URL(request.url).pathname + new URL(request.url).search, body,
          async (t) => { await writer.write(new TextEncoder().encode(t)); return true; },
          async (t) => { await writer.write(new TextEncoder().encode(t.replace('{', '{"sawFunctionHeader":' + request.headers.has('x-forkrail-function') + ','))); await writer.close(); },
          () => status());
        await new Promise((r) => (status = r));
        done.catch(() => {});
        return new Response(stream.readable, { headers: { 'content-type': 'application/json' } });
      },
    };
  `;
  const node = `
    import { work } from './work.mjs';
    export default async function (req, res) {
      await work(req.headers['x-test-id'], req.url, req.body,
        (t) => new Promise((r) => res.write(t, () => r(!res.destroyed))),
        (t) => new Promise((r) => res.end(t.replace('{', '{"sawFunctionHeader":' + ('x-forkrail-function' in req.headers) + ','), r)),
        () => res.writeHead(200, { 'content-type': 'application/json' }));
    }
  `;
  const app = await start(
    {
      'work.mjs': WORK,
      'web.mjs': web,
      'node.mjs': node,
      'functions.mjs': `
        const serve = globalThis[Symbol.for('forkrail.buildOutput')];
        serve({
          cwd: process.cwd(),
          env: { FROM_BUILD: '1' },
          functions: [
            { name: '/web', paths: ['/web'], helpers: false, load: () => import('./web.mjs') },
            { name: '/node', paths: ['/node'], helpers: true, load: () => import('./node.mjs') },
          ],
        });
      `,
    },
    'functions.mjs',
    { UPSTREAM: up.url },
  );
  try {
    // Half to each function, all at once.
    const [web1, node1] = await Promise.all([hammer(app.port, '/web'), hammer(app.port, '/node')]);
    await sleep(400);
    // Each run's ids are the same for both functions, so each half is checked on its own lines:
    // split the output by function (each request's path names it) is not possible from lines,
    // so the second run goes alone.
    assert.equal(web1.filter(Boolean).length, IN_FLIGHT - IN_FLIGHT / ABORTED_EVERY);
    assert.equal(node1.filter(Boolean).length, IN_FLIGHT - IN_FLIGHT / ABORTED_EVERY);
    for (const answers of [web1, node1]) {
      for (let i = 0; i < IN_FLIGHT; i++) {
        if (i % ABORTED_EVERY === 0) continue;
        const got = JSON.parse(answers[i].text);
        assert.equal(got.sawFunctionHeader, false);
        assert.equal(got.body, `body-${tid(i)}`);
        for (const s of got.seen) assert.equal(s.id, tid(i), `${tid(i)} saw ${s.id} at ${s.where}`);
      }
    }
  } finally {
    app.stop();
    up.close();
  }
});

test('one function of a Build Output, its lines and calls under load', async () => {
  const up = await upstream();
  const app = await start(
    {
      'work.mjs': WORK,
      'node.mjs': `
        import { work } from './work.mjs';
        export default async function (req, res) {
          await work(req.headers['x-test-id'], req.url, req.body,
            (t) => new Promise((r) => res.write(t, () => r(!res.destroyed))),
            (t) => new Promise((r) => res.end(t.replace('{', '{"sawFunctionHeader":' + ('x-forkrail-function' in req.headers) + ','), r)),
            () => res.writeHead(200, { 'content-type': 'application/json' }));
        }
      `,
      'functions.mjs': `
        globalThis[Symbol.for('forkrail.buildOutput')]({
          cwd: process.cwd(), env: {},
          functions: [{ name: '/node', paths: ['/node'], helpers: true, load: () => import('./node.mjs') }],
        });
      `,
    },
    'functions.mjs',
    { UPSTREAM: up.url },
  );
  try {
    const answers = await hammer(app.port, '/node');
    await sleep(400);
    checkRun(app, answers, up, { functions: true });
  } finally {
    app.stop();
    up.close();
  }
});

// Each answer says which request it answers (`x-forkrail-reply-to`, checked by the runtime), so
// bytes an app writes outside an answer can never pass for another request's answer.
test('every answer is stamped with the id of the request it answers', async () => {
  const app = await start(
    {
      'server.mjs': `
        import http from 'node:http';
        http.createServer((req, res) => {
          if (req.url === '/object') { res.writeHead(201, { 'x-own': '1' }); return res.end('o'); }
          if (req.url === '/array') { res.writeHead(202, ['x-own', '1']); return res.end('a'); }
          if (req.url === '/set') { res.setHeader('x-forkrail-reply-to', 'forged'); return res.end('s'); }
          if (req.url === '/later') return setTimeout(() => res.end('l'), 20);
          res.end('plain');
        }).listen(Number(process.env.PORT), '127.0.0.1');
      `,
    },
    'server.mjs',
  );
  try {
    assert.equal(app.out.replies, true, 'the shim tells the runtime it stamps its answers');
    const answers = await Promise.all(['/plain', '/object', '/array', '/later', '/set'].map((path, i) => ask(app.port, { path, headers: { 'x-forkrail-request-id': rid(i) } })));
    for (const [i, a] of answers.entries()) {
      const expected = i === 4 ? 'forged' : rid(i);
      assert.equal(a.headers['x-forkrail-reply-to'], expected, `${i}: ${JSON.stringify(a.headers)}`);
    }
  } finally {
    app.stop();
  }
  // A Build Output's own answers too (its 404).
  const fns = await start(
    { 'functions.mjs': `globalThis[Symbol.for('forkrail.buildOutput')]({ cwd: process.cwd(), env: {}, functions: [] });` },
    'functions.mjs',
  );
  try {
    const a = await ask(fns.port, { path: '/nowhere', headers: { 'x-forkrail-request-id': rid(9) } });
    assert.equal(a.status, 404);
    assert.equal(a.headers['x-forkrail-reply-to'], rid(9));
  } finally {
    fns.stop();
  }
});

// A line written in two parts outside any request (a timer's, a library's) must not swallow a
// request's line written between its parts: the request would lose it (found 2026-11-16).
test("a line started outside any request doesn't take in a request's line", async () => {
  const app = await start(
    {
      'server.mjs': `
        import http from 'node:http';
        // Outside any request: a timer made at start, which writes a line in two parts.
        let step = 0;
        setInterval(() => {
          if (step === 1) { process.stdout.write('bare'); step = 2; }
          else if (step === 3) { process.stdout.write(' end\\n'); step = 4; }
        }, 5);
        http.createServer(async (req, res) => {
          step = 1;
          while (step !== 2) await new Promise((r) => setTimeout(r, 2));
          console.log('line user1');
          step = 3;
          while (step !== 4) await new Promise((r) => setTimeout(r, 2));
          res.end('ok');
        }).listen(Number(process.env.PORT), '127.0.0.1');
      `,
    },
    'server.mjs',
  );
  try {
    const a = await ask(app.port, { path: '/', headers: { 'x-forkrail-request-id': rid(1) } });
    assert.equal(a.text, 'ok');
    await sleep(50);
    const lines = app.out.stdout.split('\n').filter(Boolean);
    assert.ok(lines.includes(`\x1e${rid(1)}\x1fline user1`), JSON.stringify(lines));
    assert.ok(lines.includes('bare end'), JSON.stringify(lines));
  } finally {
    app.stop();
  }
});

// Not isolation the platform can give: two functions of one deployment share one process, so its
// globals and process.env (on Vercel each function has instances of its own). Kept as a test so
// a change to that is noticed.
test('two functions of one deployment share globalThis and process.env (unlike Vercel)', async () => {
  const app = await start(
    {
      'a.mjs': `export default function (req, res) { globalThis.lastUser = req.headers['x-test-id']; process.env.SET_BY_A = 'yes'; res.end('a'); }`,
      'b.mjs': `export default function (req, res) { res.end(JSON.stringify({ lastUser: globalThis.lastUser ?? null, env: process.env.SET_BY_A ?? null })); }`,
      'functions.mjs': `
        globalThis[Symbol.for('forkrail.buildOutput')]({
          cwd: process.cwd(), env: {},
          functions: [
            { name: '/a', paths: ['/a'], helpers: false, load: () => import('./a.mjs') },
            { name: '/b', paths: ['/b'], helpers: false, load: () => import('./b.mjs') },
          ],
        });
      `,
    },
    'functions.mjs',
  );
  try {
    await ask(app.port, { path: '/a', headers: { 'x-forkrail-function': '/a', 'x-test-id': 'alice' } });
    const b = await ask(app.port, { path: '/b', headers: { 'x-forkrail-function': '/b', 'x-test-id': 'bob' } });
    assert.deepEqual(JSON.parse(b.text), { lastUser: 'alice', env: 'yes' });
  } finally {
    app.stop();
  }
});

// Next.js asks its platform for `waitUntil` through `@next/request-context` (`after()`, and what
// its server does behind an answer); a server calling Next's handlers itself passes
// `forkrail.waitUntil` as `ctx.waitUntil`. Both must be the tracked one: the runtime hears of the
// work as it starts and as it settles, and its lines stay its request's.
test("Next's request context and the handlers' waitUntil are the tracked one", async () => {
  const app = await start(
    {
      'server.mjs': `
        import http from 'node:http';
        import { setTimeout as sleep } from 'node:timers/promises';
        const next = globalThis[Symbol.for('@next/request-context')];
        const direct = globalThis[Symbol.for('forkrail.waitUntil')];
        http.createServer((req, res) => {
          const waitUntil = req.url === '/next' ? next.get().waitUntil : direct;
          waitUntil((async () => { await sleep(30); console.log('late ' + req.url); })());
          waitUntil(Promise.reject(new Error('failed ' + req.url)));
          res.end(typeof waitUntil);
        }).listen(Number(process.env.PORT), '127.0.0.1');
      `,
    },
    'server.mjs',
  );
  try {
    const answers = await Promise.all(['/next', '/direct'].map((path, i) => ask(app.port, { path, headers: { 'x-forkrail-request-id': rid(i) } })));
    assert.deepEqual(answers.map((a) => a.text), ['function', 'function']);
    const waits = () => app.out.control.filter((m) => m.type === 'wait');
    // Told as the work starts, before it settles: that's what keeps the worker.
    assert.equal(waits().filter((m) => m.delta === 1).length, 4);
    await new Promise((resolve) => setTimeout(resolve, 150));
    assert.equal(waits().reduce((n, m) => n + m.delta, 0), 0, 'all of it settled');
    assert.deepEqual(app.out.control.filter((m) => m.type === 'uncaught'), [], 'a failure is reported, not thrown');
    for (const [i, path] of ['/next', '/direct'].entries()) {
      assert.ok(app.out.stdout.includes(`\x1e${rid(i)}\x1flate ${path}\n`), `the late line of ${path} is its request's: ${JSON.stringify(app.out.stdout)}`);
      assert.ok(app.out.stderr.includes(`\x1e${rid(i)}\x1fwaitUntil: Error: failed ${path}`), `the failure of ${path} is its request's: ${JSON.stringify(app.out.stderr.slice(0, 300))}`);
    }
  } finally {
    app.stop();
  }
});

// Vercel's request context gives nothing outside a request, as on Vercel: `@vercel/otel` checks
// for none before it reads `headers.host`, and an empty object there crashed the app at its first
// span outside a request. Inside one it has the request's headers and URL.
test("Vercel's request context is empty outside a request and the request's inside", async () => {
  const app = await start(
    {
      'server.mjs': `
        import http from 'node:http';
        const vercel = globalThis[Symbol.for('@vercel/request-context')];
        const outside = vercel.get();
        http.createServer((req, res) => {
          const inside = vercel.get();
          res.end(JSON.stringify({ outside: outside === undefined, host: inside?.headers?.host ?? null, url: inside?.url ?? null }));
        }).listen(Number(process.env.PORT), '127.0.0.1');
      `,
    },
    'server.mjs',
  );
  try {
    const answer = await ask(app.port, { path: '/x', headers: { 'x-forkrail-request-id': rid(0), host: 'shop.example' } });
    const seen = JSON.parse(answer.text);
    assert.equal(seen.outside, true);
    assert.equal(seen.host, 'shop.example');
    assert.match(seen.url, /^https?:\/\/shop\.example\/x$/);
  } finally {
    app.stop();
  }
});

test("the loop guard's mark goes on calls to the install's own hosts, and only those", async () => {
  // Every call lands here; `localhost` is the install's own host, `127.0.0.1` a site elsewhere.
  const marks = {};
  const target = http.createServer((req, res) => {
    marks[req.url] = req.headers['x-forkrail-loop'] ?? null;
    res.end('ok');
  });
  await new Promise((resolve) => target.listen(0, '127.0.0.1', resolve));
  const { port: targetPort } = target.address();
  const hostsDir = mkdtempSync(join(tmpdir(), 'forkrail-own-hosts-'));
  const hostsFile = join(hostsDir, 'hosts');
  writeFileSync(hostsFile, 'localhost\n*.example.test\n');
  const app = await start(
    {
      'server.mjs': `
        import http from 'node:http';
        const get = (url) => new Promise((resolve, reject) => http.get(url, (r) => { r.resume(); r.on('end', resolve); }).on('error', reject));
        http.createServer(async (req, res) => {
          const tag = new URL(req.url, 'http://x').searchParams.get('tag');
          const t = process.env.TARGET_PORT;
          await fetch('http://localhost:' + t + '/fetch-own-' + tag);
          await fetch('http://127.0.0.1:' + t + '/fetch-other-' + tag);
          await get('http://localhost:' + t + '/http-own-' + tag);
          await get('http://127.0.0.1:' + t + '/http-other-' + tag);
          res.end('done');
        }).listen(process.env.PORT, '127.0.0.1');
      `,
    },
    'server.mjs',
    { TARGET_PORT: String(targetPort), FORKRAIL_OWN_HOSTS_FILE: hostsFile },
  );
  try {
    const mark = 'ab12.1.' + 'c'.repeat(64);
    const marked = await ask(app.port, { path: '/?tag=a', headers: { 'x-forkrail-request-id': 'r1', 'x-forkrail-loop': mark } });
    assert.equal(marked.text, 'done');
    const plain = await ask(app.port, { path: '/?tag=b', headers: { 'x-forkrail-request-id': 'r2' } });
    assert.equal(plain.text, 'done');
    assert.deepEqual(marks, {
      '/fetch-own-a': mark,
      '/fetch-other-a': null,
      '/http-own-a': mark,
      '/http-other-a': null,
      '/fetch-own-b': null,
      '/fetch-other-b': null,
      '/http-own-b': null,
      '/http-other-b': null,
    });
  } finally {
    app.stop();
    target.close();
    rmSync(hostsDir, { recursive: true, force: true });
  }
});
