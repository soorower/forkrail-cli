// Test app for the Node runtime: plain node:http, no dependencies. It knows nothing about
// forkrail and listens on PORT like any Node server.
//
// - GET /sleep?ms=200          waits, then answers, like an I/O-bound request
// - GET /cpu?ms=50             keeps the CPU (and the event loop) busy
// - GET /work/cpu:5,io:100,cpu:20  runs the steps in order: cpu:N uses N ms of CPU time (however
//                              long that takes where CPU is throttled), io:N waits N ms, like a call
//                              to a database or an API. In the path, so each mix is its own route.
// - GET /alloc?mb=300&ms=1000  holds 300 MB outside the JS heap (Buffers) for a second
// - GET /heap?mb=300&ms=1000   holds 300 MB on the JS heap; past --max-old-space-size, Node dies
// - GET /crash?ms=500          throws from a timer, which kills the process
// - GET /stream?chunks=10&ms=100  sends a line every ms
// - GET /bytes?mb=5            answers with 5 MB
// - GET /log?n=3&msg=hi        prints n lines to stdout and one to stderr, for checking app logs;
//                              with later=2000, one more line 2 s after answering
// - GET /fetch?url=URL&n=2     calls URL n times with fetch (or node:http with via=http), and
//                              prints a line each time: for checking outgoing calls
// - any other GET or HEAD      a JSON summary of the process and the request
// - other methods              stream the request body back

import http from 'node:http';
import https from 'node:https';
import { setTimeout as sleep } from 'node:timers/promises';

const started = Date.now();
let served = 0;
let inflight = 0;

const server = http.createServer(async (req, res) => {
  served += 1;
  inflight += 1;
  res.on('close', () => { inflight -= 1; });
  const url = new URL(req.url, 'http://app');
  const num = (key) => Number(url.searchParams.get(key) ?? 0);
  const wait = Math.min(num('ms'), 60_000);

  const route = url.pathname.startsWith('/work/') ? '/work' : url.pathname;
  switch (route) {
    case '/sleep':
      await sleep(wait);
      break;
    case '/cpu': {
      const until = Date.now() + wait;
      while (Date.now() < until);
      break;
    }
    case '/work':
      for (const step of decodeURIComponent(url.pathname.slice('/work/'.length)).split(',')) {
        const [kind, value] = step.split(':');
        const ms = Math.min(Number(value ?? 0), 60_000);
        if (kind === 'cpu') {
          const start = process.cpuUsage();
          for (let used = 0; used < ms * 1000;) {
            const { user, system } = process.cpuUsage(start);
            used = user + system;
          }
        } else if (kind === 'io') {
          await sleep(ms);
        }
      }
      break;
    case '/alloc': {
      const held = Buffer.alloc(num('mb') << 20, 1);
      await sleep(wait);
      held.at(0);
      break;
    }
    case '/heap': {
      // 1 MB per array: 2^17 doubles, stored unboxed. (Repeated strings would share memory.)
      const held = Array.from({ length: num('mb') }, (_, i) => new Array(1 << 17).fill(i + 0.5));
      await sleep(wait);
      held.at(0);
      break;
    }
    case '/crash':
      setTimeout(() => { throw new Error('crash requested'); }, wait);
      return;
    case '/stream':
      res.writeHead(200, { 'content-type': 'text/plain' });
      for (let line = 1; line <= num('chunks'); line++) {
        res.write(`line ${line}\n`);
        await sleep(wait);
      }
      res.end();
      return;
    case '/bytes':
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end(Buffer.alloc(num('mb') << 20, 'x'));
      return;
    case '/fetch': {
      // `egress://NAME/path` goes through the router to its private target NAME (FORKRAIL_EGRESS_<NAME>).
      const target = (url.searchParams.get('url') ?? 'https://checkip.amazonaws.com')
        .replace(/^egress:\/\/([a-z0-9-]+)/, (_, name) => `http://${process.env[`FORKRAIL_EGRESS_${name.toUpperCase().replaceAll('-', '_')}`]}`);
      const statuses = [];
      for (let i = 1; i <= Math.min(num('n') || 1, 20); i++) {
        const status = url.searchParams.get('via') === 'http'
          ? await new Promise((resolve, reject) => {
            (target.startsWith('https:') ? https : http).get(target, (answer) => {
              answer.resume();
              answer.on('end', () => resolve(answer.statusCode));
            }).on('error', reject);
          })
          : await fetch(target).then(async (answer) => { await answer.arrayBuffer(); return answer.status; });
        console.log(`call ${i}: ${status}`);
        statuses.push(status);
      }
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({ url: target, statuses }));
      return;
    }
    case '/log': {
      const msg = url.searchParams.get('msg') ?? 'hello';
      const n = Math.min(num('n') || 1, 10_000);
      for (let i = 1; i <= n; i++) console.log(`${msg} ${i}/${n}`);
      console.error(`${msg} on stderr`);
      const later = num('later');
      if (later > 0) setTimeout(() => console.log(`${msg} ${later} ms later`), later);
      res.end(`logged ${n + 1} lines\n`);
      return;
    }
  }

  if (req.method !== 'GET' && req.method !== 'HEAD') {
    const headers = req.headers['content-type'] ? { 'content-type': req.headers['content-type'] } : {};
    res.writeHead(200, headers);
    req.pipe(res);
    return;
  }

  const memory = process.memoryUsage();
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(JSON.stringify({
    pid: process.pid,
    node: process.version,
    uptime_ms: Date.now() - started,
    request_no: served,
    inflight,
    method: req.method,
    path: url.pathname,
    query: url.search,
    host: req.headers.host,
    forwarded_for: req.headers['x-forwarded-for'],
    forwarded_proto: req.headers['x-forwarded-proto'],
    request_id: req.headers['x-forkrail-request-id'],
    rss_mb: Math.round(memory.rss / 2 ** 20),
    heap_mb: Math.round(memory.heapUsed / 2 ** 20),
    heap_limit_mb: Math.round((await import('node:v8')).getHeapStatistics().heap_size_limit / 2 ** 20),
  }));
});

server.listen(process.env.PORT || 3000, () => {
  console.log(`listening on ${server.address().port}`);
});
