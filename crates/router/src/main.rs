//! forkrail router: public HTTP entry point and concurrent compute scheduler.
//!
//! Configuration (environment variables):
//! - `FORKRAIL_LISTEN`: plain HTTP for the tester (load tests, the default pool), default
//!   `0.0.0.0:8080`. On AWS the security group opens it to the tester's IP only
//! - `FORKRAIL_TLS_LISTEN`: HTTPS for everyone, e.g. `0.0.0.0:443`; off by default. TLS 1.2 and
//!   1.3, h2 and http/1.1, with the base domain's wildcard certificate (see `certs.rs`). It starts
//!   once there's a certificate. Hosts the table doesn't have get 404 `DEPLOYMENT_NOT_FOUND`
//! - `FORKRAIL_REDIRECT_LISTEN`: plain HTTP that answers 308 to the same URL over HTTPS, e.g.
//!   `0.0.0.0:80`; off by default
//! - `FORKRAIL_BASE_DOMAIN`: deployments' hosts are one label under it, e.g. `forkrail.com`. Only
//!   such hosts make the router read the table again when it doesn't know them, and it's the name
//!   on the certificate. Required with `FORKRAIL_TLS_LISTEN`
//! - `FORKRAIL_TLS_CERTS`: where the certificate is kept, `s3://bucket/prefix` or a directory, under
//!   `<base>/` (`<base>/staging/` with staging): `cert.pem`, `key.pem` and the ACME `account.json`
//! - `FORKRAIL_CLOUDFLARE_ZONE`: the base domain's Cloudflare zone. With it (or with
//!   `FORKRAIL_ACME=1`, which finds the zone by name), this router is the issuer: it issues and
//!   renews certificates itself through Let's Encrypt (see `acme.rs`); without, it only loads what
//!   the store has: with several routers, one issues, and the others pick its certificates up from
//!   the store (a missing one within a minute)
//! - `FORKRAIL_CLOUDFLARE_TOKEN_PARAM`: the SSM SecureString holding the Cloudflare API token,
//!   default `/forkrail/cloudflare-api-token`. Read when it's needed, kept in memory for at most
//!   10 minutes, never logged. Without it only HTTP-01 works
//! - `FORKRAIL_ACME_STAGING`: `1` uses Let's Encrypt's staging server, for tests
//! - Custom domains in the routing table (`domains`, `*.example.com` included) get their own
//!   certificate, under `<domain>/` in the same store, with `status.json` beside it saying how it
//!   stands. The issuer proves each one ours the first way that works: its own Cloudflare zone
//!   (found by name with the token), its `_acme-challenge` CNAME delegated to `<hash>._acme.<base>`
//!   in the base domain's zone, or HTTP-01 once it points here. HTTPS serves them as soon as their
//!   certificate is in use; there's no default pool for them. A domain whose entry `redirect`s
//!   answers every request with that redirect
//! - `FORKRAIL_ACME_CHALLENGES`: where HTTP-01 key authorizations are kept, `s3://bucket/prefix`
//!   or a directory; by default `acme/http-01` beside `FORKRAIL_TLS_CERTS` (`state/acme/http-01`).
//!   The issuer writes them; every router's port 80 answers
//!   `/.well-known/acme-challenge/<token>` from there for the table's custom domains
//! - `FORKRAIL_PUBLIC_ADDRESSES`: the install's public IPv4 addresses, comma-separated, which a
//!   domain pointing here resolves to; by default what the base domain's wildcard resolves to
//! - SIGHUP renews the wildcard now, without a restart (a duplicate certificate: Let's Encrypt
//!   allows 5 a week). SIGUSR1 has custom domains check theirs now, issuing only what's missing or
//!   due: after giving the token a new zone, say. A new `checked_at` on a domain's entry does the
//!   same for that domain
//! - Deployment protection (`protection.rs`): the table's `protection` (verifying keys, the
//!   dashboard's address, the default scope) and each project's `protection` decide which hosts
//!   need access. HTTPS only: the tester's port is never protected. A browser without access goes
//!   to the dashboard; anything else gets 401 `DEPLOYMENT_PROTECTED`. `/_forkrail/auth` takes the
//!   dashboard's token back and keeps it as a cookie for that host
//! - Skew protection (`skew.rs`): with a project's `skew` on, a request naming one of its
//!   deployments (`?dpl=`, `x-deployment-id`, the `__vdpl` cookie) is served by it while it's young
//!   enough; `skew` in the access log says what happened (`pinned` when it was)
//! - `FORKRAIL_GEOIP_DIR`: DB-IP's City and ASN databases (`city.mmdb`, `asn.mmdb`; default
//!   `/var/lib/forkrail/geoip`), for each client's location in the log and the app's
//!   `x-vercel-ip-*` headers; reopened within an hour of being replaced. Without them, no location
//! - `FORKRAIL_LOG_DIR`: write the log to hourly files there (`router.YYYY-MM-DD-HH.log`) from a
//!   background thread, with warnings also on stderr; without it, everything goes to stdout
//! - `GET /_forkrail/health` on any host is answered by the router itself (200 `ok`), for health
//!   checks: it reaches no app and isn't in the access log. Port 80 answers it too, without the
//!   redirect, for Global Accelerator's health checks
//! - Every request's access log line (`request`) has the query string (secret-looking parameters
//!   masked), the client's address, `h1.1` or `h2`, and the user agent. Failed TLS handshakes are
//!   summed up once a minute
//! - `FORKRAIL_EVENTS_QUEUE`: the install's events queue's URL (`forkrail-events`, see
//!   `events.rs`), where the router says an attack is being refused and a custom domain's
//!   certificate failed to renew, for the team's webhooks and owners; unset sends none
//! - `FORKRAIL_SCHEDULE_QUEUE`: the SQS queue's URL that EventBridge Scheduler sends schedules'
//!   runs to (`forkrail-schedules`, see `schedules.rs`); unset polls none. Every router polls it,
//!   and runs each message it takes on the project's production deployment (`<project>.<base>`, so
//!   it needs `FORKRAIL_BASE_DOMAIN`) like a visitor's request, sent to a loopback listener of the
//!   process's own and logged with entry `schedule`. The tester's port takes a message from this
//!   machine at `POST /_forkrail/schedule-run`, for tests
//! - `FORKRAIL_TUNNEL_LISTEN`: listener workers dial into, default `0.0.0.0:7443`; a comma-separated
//!   list takes the first that's free, so two routers can run at once during a restart
//! - `FORKRAIL_TUNNEL_ADDR`: tunnel address as reachable from workers, default `127.0.0.1:7443`;
//!   workers get its host with the port the tunnel listener took
//! - `FORKRAIL_ROUTES`: the routing table, `s3://bucket/key` or a local file (see
//!   `forkrail_common::routes`): which deployment serves each hostname. Read at start-up, when a
//!   request names an unknown host (at most once a second), and every `FORKRAIL_ROUTES_POLL_MS`
//!   (default 1000, so a firewall change is in force within about a second) with a conditional GET
//! - `FORKRAIL_STATE_REGION`: the region of the bucket holding the routing table and the
//!   certificates, and of the Cloudflare token; the router's own region when unset. A router in
//!   another region reads the same table, so it knows every host
//! - Lambda runs in the router's own region (`AWS_REGION`). A deployment published there too
//!   (`replicas` in its table entry) runs its version there; one that isn't runs in the nearest
//!   region it was published in (its project's function regions), with workers dialling back
//!   across regions
//! - `FORKRAIL_HOME_REGION`: the region of deployments whose table entry names none (those from
//!   before regions were recorded); the router's own region when unset
//! - `FORKRAIL_FUNCTION`: a Lambda function for the default pool, which serves hosts the table
//!   doesn't have. Deployments run on Lambda, each at its own version.
//! - `FORKRAIL_RUNTIME_BIN`: path to the runtime binary, to run workers as local processes instead:
//!   the default pool runs the app the router's environment names (`FORKRAIL_APP_DIR`,
//!   `FORKRAIL_APP_ENTRY`, or echo), and each deployment the app its table entry's `env` names
//! - `FORKRAIL_DEFAULT_POOL`: `0` turns the default pool off, so unknown hosts get 404
//!   `DEPLOYMENT_NOT_FOUND`. Without `FORKRAIL_FUNCTION` or `FORKRAIL_RUNTIME_BIN` there's none
//! - `FORKRAIL_MAX_WORKERS`: workers open at once across all pools, or `none` (the default): as
//!   many as the account's Lambda concurrency allows. Past that, Lambda throttles and the router
//!   backs off
//! - `FORKRAIL_POOL_MAX_WORKERS`: workers open at once per pool, or `none` (the default); a table
//!   entry's `max_workers` overrides it. Requests beyond a cap wait for a slot, then get 503
//! - `FORKRAIL_MAX_DURATION_SECS`: the default pool's maxDuration, default 300: a request
//!   unanswered by then gets 504 `FUNCTION_INVOCATION_TIMEOUT`, and its stream is reset.
//!   Deployments set their own (`max_duration_s`)
//! - `FORKRAIL_MAX_INFLIGHT`: concurrent requests per worker, default 10
//! - `FORKRAIL_IDLE_MS`: retire a worker after this long idle; `auto` (the default) keeps it only as
//!   long as that's cheaper than a new invocation, from the deployment's memory size (24 ms at
//!   640 MB, see `economic_idle_ms`). Once the function has had nothing in flight for this long,
//!   every worker retires, spares included. 0 retires a worker the moment its last request ends,
//!   unless a request is waiting for it, which makes a busy API start a worker every few requests;
//!   requests that overlap still share it, and the next one after a gap thaws a frozen environment
//! - `FORKRAIL_MAX_AGE_SECS`: stop routing to a worker after this long, default 780 (13 min)
//! - `FORKRAIL_HEADROOM_PCT`: free slots to keep ahead of demand, as a percentage of demand
//!   (at least 30% of one worker), default 10; 0 turns pre-spawning off
//! - `FORKRAIL_BUFFER_BYTES`: request bodies up to this size are read in full before dispatch, and
//!   responses are pulled up to this far ahead of the client, default 8 MiB
//! - `FORKRAIL_BUFFER_TOTAL_BYTES`: what all requests and responses together may hold that way,
//!   default 512 MiB; past it they wait for room
//! - `FORKRAIL_CPU_TARGET_PCT`: CPU budget per worker, as a percentage of the CPU its memory size
//!   buys (at most one core): a request joins a busy worker only if the learned CPU shares of its
//!   requests stay within it, default 0 (off). `tools/tune-memory.py` suggests a value
//! - `FORKRAIL_PROFILE_PCT`: share of requests admitted to an empty worker whose CPU use is sampled
//!   over time and written to the access log (`cpu_profile`), for the tuner, default 0
//! - Static files: a deployment whose table entry names a manifest (`static`) has its files served
//!   by the router from S3 in its own region (see `statics.rs`), before the function, logged with
//!   outcome `static`. `FORKRAIL_STATIC_BUCKET` is that bucket; by default the routing table's
//!   bucket in its own region, `<bucket>-<region>` elsewhere, as the CLI names them.
//!   `FORKRAIL_STATIC_CACHE_MB` (default 64) keeps recently served files in memory.
//!   `FORKRAIL_STATIC_DIR` reads the same layout from a directory instead, for local runs.
//!   `FORKRAIL_STATIC=0` sends every request to the function instead
//! - Image Optimization (see `images.rs`): a deployment whose table entry names image settings
//!   (`images`) answers `/_vercel/image` and `/_next/image` itself, as Vercel does, from optimized
//!   images kept in memory (`FORKRAIL_IMAGE_CACHE_MB`, default 64) and once for the whole install
//!   under `images/` in the home bucket (the routing table's, in `FORKRAIL_STATE_REGION`;
//!   `FORKRAIL_IMAGE_BUCKET` names another), logged with outcome `image_hit`, `image_miss`, `image_stale`,
//!   `image_refused` or `image_failed`. Misses are made by the optimizer function,
//!   `FORKRAIL_IMAGE_FUNCTION` (default `forkrail-image`) in the router's own region, invoked
//!   directly; `FORKRAIL_IMAGE_URL` posts to a local one instead (`deploy/image/index.mjs serve`).
//!   Needs static files (`FORKRAIL_STATIC` not `0`)
//! - Web Analytics (see `insights.rs`): `/_vercel/insights/*` and `/_forkrail/insights/*` on every
//!   deployment: the script, and page views' and events' beacons, each kept as a `web event` line
//!   for projects that have it on. Visitors' numbers come from a salt a day, shared by the routers
//!   under `state/insights/salts/` in the routing table's bucket (`FORKRAIL_INSIGHTS_SALTS`,
//!   `s3://bucket/prefix`, names another place; `local` keeps this router's own)
//! - Response cache (see `cache.rs`): a Build Output's prerenders (Vercel's ISR) and function
//!   answers that ask for the CDN (`s-maxage`, `CDN-Cache-Control`, `Vercel-CDN-Cache-Control`)
//!   are kept and served again, logged with outcome `cache` and `cache` (`x-vercel-cache`'s
//!   value; `background` for what the router makes again itself, `peer` for what home makes for
//!   another router). CDN answers in `FORKRAIL_RESPONSE_CACHE_MB` (default 64) of memory; pages in
//!   `FORKRAIL_PAGES_MEMORY_MB` (default 32) and on disk in `FORKRAIL_PAGES_DIR` (default
//!   `/var/lib/forkrail/pages`), at most `FORKRAIL_PAGES_DISK_MB` (default 2048) and never leaving
//!   the filesystem less than `FORKRAIL_PAGES_FREE_MB` (default 1024) free; least used out, unused
//!   31 days deleted. `FORKRAIL_RESPONSE_CACHE=0` keeps nothing. `FORKRAIL_CACHE_DEFAULTS=0` leaves
//!   out Vercel's headers on answers not kept (`age: 0`, `x-vercel-cache: MISS`, and
//!   `cache-control: public, max-age=0, must-revalidate` when the app sets none).
//!   `FORKRAIL_PAGES_VISITOR_HEADERS`: `strip` (default) makes a page without the visitor's
//!   `Cookie` and `Authorization` (it's kept for everyone); `pass` sends them as they came.
//! - Next.js's shared cache (`cache/next.rs`): a Next.js deployment's ISR pages, data cache and
//!   `'use cache'` entries are kept in the page store, one copy for every worker and region, and
//!   reached over each worker's tunnel (or `FORKRAIL-CACHE/1` on the tunnel port once its tunnel
//!   is gone). `FORKRAIL_NEXT_DATA_MB` (default 256): the data cache each project and environment
//!   may keep on disk. The build's prerenders are read from `static/next-seed/<deployment>/` among the
//!   static files when first asked for. Pages of deployments that left the table are deleted by
//!   the hourly sweep.
//! - Cache tags (`cache/tags.rs`): kept answers and pages keep their `Vercel-Cache-Tag`; purges by
//!   tag come from functions (the runtime dials the tunnel port as its worker: `FORKRAIL-PURGE/1`,
//!   logged `purge`), from home over the peer link, and from the routing table
//!   (`projects.<name>.purges`, the CLI's `forkrail cache`). Kept in `tags.json` beside the pages
//! - The install's routers (`routers.rs`), which every router knows, in memory: `FORKRAIL_PEERS`
//!   (`region=host:port`, `us-east-1=3.3.3.3:7445,us-west-1=4.4.4.4:7445`), and what each router
//!   says of itself in the state bucket (`state/routers/<region>/<host:port>`, from
//!   `FORKRAIL_PEER_ADDR`, else its `FORKRAIL_PEERS` entry, else its tunnel host with the peer
//!   port), read again every 5 minutes. This router's region is `AWS_REGION`.
//! - Pages between routers (`cache/peers.rs`): the router in `FORKRAIL_STATE_REGION` (home) makes
//!   pages and pushes them to the others, which ask it for theirs. Routers listen for each other on
//!   `FORKRAIL_PEER_LISTEN` (default `0.0.0.0:7445`, beside the other process during a restart),
//!   and prove they're the install's with `FORKRAIL_PEER_KEY` (hex, local runs) or the shared key
//!   in SSM (`FORKRAIL_PEER_KEY_PARAM`, default `/forkrail/router/peer-key`). Without a key, or
//!   (a local run) without `FORKRAIL_PEERS`, a router keeps its own pages
//! - Requests between routers (`relay.rs`): a deployment with no version in this router's region
//!   whose nearest version is in another router's region runs there: its requests go to that
//!   router over the same link, which runs them on its own workers, rather than this router
//!   starting workers there that dial back across regions. One whose version is in a region with
//!   no router is invoked there from here, its workers dialing back. Which way is settled when
//!   the deployment's pool is made, from the routers known then, not per request.
//!   `FORKRAIL_RELAY=0` turns it off (other routers' requests are still run here); while the
//!   link is down, workers start there from here as before
//! - Firewall (see `firewall.rs`): the install's configuration (`firewall` in the routing table) and
//!   each project's (`projects.<name>.firewall`), read from the same place as static files and in
//!   force before the listeners open (up to 10 s); requests it answers have outcome `firewall`, and
//!   every request it acted on has `firewall` (deny, rate_limit, redirect, bypass, log, held) and
//!   `firewall_rule` in the access log. Rate limits are counted here (`ratelimit.rs`), in at most
//!   `FORKRAIL_RATE_LIMIT_BUCKETS` buckets at once (default 1,000,000, about 30 MB). Apps ask
//!   about their own rate limits at `/.well-known/forkrail/rate-limit-api/<id>` (outcome
//!   `rate_limit_api`). The challenge (`challenge.rs`): `FORKRAIL_CHALLENGE_KEY` (hex, local runs)
//!   or the shared key in SSM (`FORKRAIL_CHALLENGE_KEY_PARAM`), `FORKRAIL_CHALLENGE_BITS` (default
//!   18). Attack mode lets verified crawlers by (`bots.rs`; `FORKRAIL_BOTS=0` fetches no lists,
//!   `FORKRAIL_BOTS_DIR` keeps them)
//! - JA4 (see `ja4.rs`): every HTTPS connection's ClientHello is fingerprinted before rustls reads
//!   it (`ja4` in the access log, `ja4_digest` conditions, `ja4` rate-limit keys)
//! - System mitigations (see `shield.rs`), before everything for every project:
//!   `FORKRAIL_SHIELD_CONNECTIONS` (default 256 open per address), `FORKRAIL_SHIELD_REQUESTS`
//!   (default 5,000 per address in 10 s, then a block of `FORKRAIL_SHIELD_BLOCK_S`, default 600);
//!   the routing table's `mitigations` set the install's own limits in their place, pause them,
//!   or leave addresses alone
//! - Bulk redirects (a deployment's `bulkRedirectsPath`, and a project's own from `projects` in the
//!   routing table) are tables read from the same place; one of 16 MB or more is written to
//!   `FORKRAIL_REDIRECTS_DIR` (default `/var/lib/forkrail/redirects`) and mapped from there
//! - `FORKRAIL_CLICKHOUSE_URL`: send app log lines and outgoing calls straight to ClickHouse
//!   (`http://host:8123` or `https://host:8443`) instead of writing them to the router's log for
//!   Vector, which costs about ten times the CPU (see `sink.rs`). With it:
//!   `FORKRAIL_CLICKHOUSE_DATABASE` (default `forkrail`), `FORKRAIL_CLICKHOUSE_USER` (default
//!   `ingest`), `FORKRAIL_CLICKHOUSE_PASSWORD_FILE` (default `/etc/forkrail/ch_password`),
//!   `FORKRAIL_CLICKHOUSE_CA` (a PEM file: the server's certificate must be one of these),
//!   `FORKRAIL_CLICKHOUSE_GZIP` (default `1` for https, `0` for http), `FORKRAIL_ROUTER_ID` (this
//!   router's name on every row, the instance id), and for when ClickHouse can't be reached
//!   `FORKRAIL_LOG_SPILL_DIR` (default `spill` under `FORKRAIL_LOG_DIR`) and
//!   `FORKRAIL_LOG_SPILL_MB` (default 2048)
//! - App logs: each request keeps what Vercel allows it (256 lines, 1 MB of text, all of its
//!   outgoing call reports), with no limit per second; lines outside any request share a bucket
//!   per deployment. What isn't kept is counted on the request's access log line
//!   (`lines_dropped`, `calls_dropped`). See `logs.rs`
//! - Keep-warm (see `keepwarm.rs`): a project that asks for it (`projects.<name>.keep_warm`) has
//!   its production deployment's environments kept alive in this router's region by a warm
//!   invocation every `FORKRAIL_KEEP_WARM_EVERY_S` (default 120), so a request after a quiet spell
//!   doesn't start cold. `FORKRAIL_KEEP_WARM` is how many for a project that doesn't say (default
//!   0, none: the author's decision of 2026-11-18, off unless a project turns it on; `auto` as many
//!   as it had in use at once at its busiest in the last day). A round
//!   sends them all at once. Real traffic counts in a round's place when it had as many open at
//!   once, and a deployment with no request for `FORKRAIL_KEEP_WARM_UNUSED_S` (default 604800, a
//!   week; 0 = never) is left alone until its next one. Of two routers on one machine (a restart)
//!   only the one holding `FORKRAIL_KEEP_WARM_LOCK` sends (a loopback listener, default
//!   `127.0.0.1:7446`; `none` for no lock); the busiest hours pass from one to the other in
//!   `FORKRAIL_KEEP_WARM_STATE` (default `/var/lib/forkrail/keepwarm/busiest.json`; `none` keeps
//!   them in memory). Needs `FORKRAIL_BASE_DOMAIN` to know production by (`<project>.<base>`),
//!   else only production's custom domains count
//! - `FORKRAIL_HANDSHAKE_START`: `1` starts a worker at a visitor's TLS handshake when the host's
//!   deployment would otherwise start cold for the request that follows (see `early.rs`); off by
//!   default
//! - Spend budgets (see `budgets.rs`): checked every second against each project's month to date;
//!   a paused project answers 503 `DEPLOYMENT_PAUSED` and its workers are stopped. Pauses go to
//!   control's queue, `FORKRAIL_SPEND_QUEUE`, which records them and sets the function's reserved
//!   concurrency to 0; thresholds and fast spending to the events queue (`FORKRAIL_EVENTS_QUEUE`).
//!   `FORKRAIL_SPEND_BLIND_FACTOR` (default 2): how many times over this router's own spend counts
//!   while ClickHouse can't be read. A router with a base domain publishes its month to date
//!   every 5 minutes to `state/spend/<region>.json` in the routing table's bucket, for the guard
//!   (crates/guard) to hold AWS's own numbers against; every router reads `state/spend/own.json`
//!   as often, what the guard counted of forkrail's own costs (storage, the tables, control), and
//!   adds it to the team's spend
//! - The spend ledger (see `spend.rs`): what each project costs, counted as it's spent at AWS's
//!   list price (`forkrail_common::prices`): function time accrued every second while a worker is
//!   open, invocations, visitors' bytes as the kernel counted them on each connection (by the
//!   visitor's zone), tunnel bytes, S3 requests. Sent to ClickHouse's `usage` table every 10 s
//!   with the sink (spilled to disk while it's away). The image optimizer's invocations are billed
//!   at `FORKRAIL_IMAGE_MEMORY_MB` (default 2048, its stack's default)
//! - The loop guard (see `loops.rs`): a function's calls back to its own install carry the mark of
//!   the request they were made for, and past `FORKRAIL_LOOP_MAX_DEPTH` such calls in a chain
//!   (default 5) the router answers 508 `INFINITE_LOOP_DETECTED` (outcome `loop`), as Vercel does;
//!   so it does past `FORKRAIL_LOOP_MAX_CHAIN` requests at depth 2 and deeper in one chain (default
//!   1,000, counted per router). The key is derived from the challenge's, so both regions read
//!   each other's marks
//! - `FORKRAIL_EGRESS_TARGETS`: private addresses apps may reach through this router, as comma-separated
//!   `name=host:port` (`analytics=172.31.9.153:8443`); a deployment allows names in its table entry
//!   (`egress`). See `tunnel.rs`
//!
//! `FORKRAIL_MAX_INFLIGHT`, `FORKRAIL_IDLE_MS`, `FORKRAIL_HEADROOM_PCT`, `FORKRAIL_CPU_TARGET_PCT`
//! and the worker caps are defaults a table entry may override per deployment. A deployment with
//! concurrent compute off (`"fluid": false`) runs one request per worker, idle 0 and no spares,
//! whatever the defaults.
//!
//! HTTP/1 clients must send a request's headers within `HEADER_READ_TIMEOUT`, which also closes
//! idle keep-alive connections after that long; TLS handshakes must finish within
//! `TLS_HANDSHAKE_TIMEOUT`.
//!
//! On SIGTERM or Ctrl-C the router stops accepting, lets in-flight requests finish
//! (up to `DRAIN_TIMEOUT`), then exits, which closes every tunnel.
//!
//! Restarts without a gap: the public listeners are bound with `SO_REUSEPORT`, so the new router
//! binds them beside the old one, and tells systemd it's ready (`Type=notify`) once all of them are
//! up, HTTPS included. Only then is the old one stopped; its listeners close at once and it drains
//! in the background. `deploy/aws/test-env.sh` runs the two as `forkrail-router@blue` and
//! `forkrail-router@green`.

mod acme;
mod bots;
mod budgets;
mod cache;
mod certs;
mod challenge;
mod early;
mod events;
mod firewall;
mod geo;
mod images;
mod insights;
mod ja4;
mod keepwarm;
mod logs;
mod loops;
mod pool;
mod protection;
mod proxy;
mod ratelimit;
mod registry;
mod relay;
mod routers;
mod schedules;
mod shield;
mod sink;
mod skew;
mod spawner;
mod spend;
mod statics;
mod tls;
mod tunnel;
mod visitor;
mod wire;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use aws_config::{BehaviorVersion, ConfigLoader, Region};
use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use forkrail_common::routes::{DEFAULT_MEMORY_MB, economic_idle_ms, normalize_host};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::certs::Certs;
use crate::pool::{Pool, PoolConfig, WorkerBudget};
use crate::proxy::{Entry, Proxy};
use crate::registry::{Backend, Registry, Source};
use crate::spawner::{INVOKE_TIMEOUT, Lambdas, Spawner};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// On shutdown, how long in-flight requests get to finish.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// An HTTP/1 client must send a request's headers within this, idle keep-alive time included.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(20);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// At start-up, how long HTTPS waits for custom domains' stored certificates.
const DOMAINS_LOAD_WAIT: Duration = Duration::from_secs(5);
/// How long start-up waits for the firewall's configurations before serving.
const FIREWALL_LOAD_WAIT: Duration = Duration::from_secs(10);
/// Connections the kernel queues for a listener before the router accepts them.
const LISTEN_BACKLOG: i32 = 4096;
/// The log writer's buffer: written out whenever the lines waiting for it run out.
const LOG_BUFFER: usize = 256 << 10;
/// How often failed TLS handshakes are summed up in the log.
const TLS_SUMMARY_EVERY: Duration = Duration::from_secs(60);

/// Set once the router is shutting down: HTTP/1 responses then carry `Connection: close`.
static DRAINING: AtomicBool = AtomicBool::new(false);

/// Whether this router is stopping: other routers' requests are refused then (`relay.rs`).
pub(crate) fn draining() -> bool {
    DRAINING.load(Ordering::Relaxed)
}
/// On shutdown, how long connections get to close by themselves before the rest are shut: HTTP/1
/// clients close theirs after a response that says so, so none sends a request on a keep-alive
/// connection just as the router closes it. As long as the idle timeout, so an idle connection is
/// closed no sooner than it would have been anyway. The new router is serving meanwhile.
const CLOSE_GRACE: Duration = HEADER_READ_TIMEOUT;

/// TLS handshakes that failed or timed out since the last summary.
static TLS_FAILED: AtomicU64 = AtomicU64::new(0);
static TLS_TIMED_OUT: AtomicU64 = AtomicU64::new(0);

/// Once a minute, if any handshake failed: how many, and how many asked for a name there's no
/// certificate for. Scanners cause most; a jump after a DNS change means real clients can't connect.
async fn tls_summary(resolver: Arc<certs::Resolver>) {
    let mut tick = tokio::time::interval(TLS_SUMMARY_EVERY);
    tick.tick().await;
    loop {
        tick.tick().await;
        let (failed, timed_out) = (TLS_FAILED.swap(0, Ordering::Relaxed), TLS_TIMED_OUT.swap(0, Ordering::Relaxed));
        let (unknown_name, last_unknown) = resolver.take_unknown();
        if failed + timed_out > 0 {
            info!(failed, timed_out, unknown_name, last_unknown, "TLS handshakes failed in the last minute");
        }
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// A worker cap from the environment: a number from 1 up, or unset or `none` for no cap.
fn env_cap(name: &str) -> Result<usize, BoxError> {
    parse_cap(std::env::var(name).ok().as_deref()).map_err(|e| format!("{name}: {e}").into())
}

fn parse_cap(value: Option<&str>) -> Result<usize, &'static str> {
    match value {
        None | Some("" | "none") => Ok(usize::MAX),
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => Err("a number from 1 up, or `none`"),
        },
    }
}

fn env_num(name: &str, default: u64) -> Result<u64, BoxError> {
    match std::env::var(name) {
        Ok(v) => v.parse().map_err(|e| format!("{name}: {e}").into()),
        Err(_) => Ok(default),
    }
}

/// The challenge's key: `FORKRAIL_CHALLENGE_KEY` (hex, for local runs), else the routers' shared
/// one from SSM (`FORKRAIL_CHALLENGE_KEY_PARAM`, default `/forkrail/router/challenge-key`), else
/// one of this process's own (passes then hold on this router only, until it restarts).
/// `FORKRAIL_CHALLENGE_BITS` (default 18) is how hard the work is.
async fn challenger_from_env() -> Result<challenge::Challenger, BoxError> {
    let bits = env_num("FORKRAIL_CHALLENGE_BITS", challenge::DEFAULT_BITS.into())?.clamp(challenge::MIN_BITS.into(), challenge::MAX_BITS.into()) as u8;
    let hex = match std::env::var("FORKRAIL_CHALLENGE_KEY") {
        Ok(hex) => Some(hex),
        Err(_) if std::env::var("FORKRAIL_ROUTES").is_ok_and(|r| r.starts_with("s3://")) => {
            let name = env_or("FORKRAIL_CHALLENGE_KEY_PARAM", forkrail_common::install::CHALLENGE_KEY_PARAM);
            match ssm_client().await.get_parameter().name(&name).with_decryption(true).send().await {
                Ok(out) => out.parameter.and_then(|p| p.value),
                Err(e) => {
                    warn!(param = %name, error = %aws_sdk_ssm::error::DisplayErrorContext(&e), "no shared challenge key: passes hold on this router only");
                    None
                }
            }
        }
        Err(_) => None,
    };
    let key = hex.map(|h| h.trim().to_owned()).filter(|h| h.len() >= 32 && h.len() % 2 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(match key {
        Some(h) => {
            let bytes: Vec<u8> = (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect();
            challenge::Challenger::new(&bytes, bits)
        }
        None => challenge::Challenger::local(bits),
    })
}

/// Settings for Lambda clients, in the router's own region.
async fn lambda_config() -> aws_config::SdkConfig {
    aws_config::defaults(BehaviorVersion::latest())
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .read_timeout(INVOKE_TIMEOUT)
                    .operation_timeout(INVOKE_TIMEOUT)
                    .build(),
            )
            // A retried invoke would start a second worker; the scheduler spawns again if needed.
            .retry_config(RetryConfig::disabled())
            .load()
            .await
}

/// In `FORKRAIL_STATE_REGION`, where the routing table, the certificates and the token are kept.
fn in_state_region(loader: ConfigLoader) -> ConfigLoader {
    match std::env::var("FORKRAIL_STATE_REGION") {
        Ok(region) if !region.is_empty() => loader.region(Region::new(region)),
        _ => loader,
    }
}

async fn ssm_client() -> aws_sdk_ssm::Client {
    let config = in_state_region(aws_config::defaults(BehaviorVersion::latest()))
        .timeout_config(TimeoutConfig::builder().operation_timeout(Duration::from_secs(10)).build())
        .load()
        .await;
    aws_sdk_ssm::Client::new(&config)
}

async fn s3_client() -> aws_sdk_s3::Client {
    let config = in_state_region(aws_config::defaults(BehaviorVersion::latest()))
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(3))
                .operation_timeout(Duration::from_secs(10))
                .build(),
        )
        .load()
        .await;
    aws_sdk_s3::Client::new(&config)
}

/// The routing table's source, how workers start, and the default pool, from the environment.
async fn registry_from_env(template: PoolConfig, base_domain: Option<String>) -> Result<Arc<Registry>, BoxError> {
    let function = std::env::var("FORKRAIL_FUNCTION").ok();
    let runtime_bin = std::env::var("FORKRAIL_RUNTIME_BIN").ok().map(PathBuf::from);
    let routes = std::env::var("FORKRAIL_ROUTES").ok();
    if function.is_none() && runtime_bin.is_none() && routes.is_none() {
        return Err("set FORKRAIL_ROUTES, FORKRAIL_FUNCTION or FORKRAIL_RUNTIME_BIN".into());
    }
    let backend = match (&function, runtime_bin) {
        (None, Some(bin)) => Backend::Local(bin),
        _ => Backend::Lambda(Lambdas::new(lambda_config().await, std::env::var("FORKRAIL_HOME_REGION").ok())),
    };
    let default_pool = match (&backend, function) {
        _ if env_or("FORKRAIL_DEFAULT_POOL", "1") == "0" => None,
        (Backend::Lambda(lambdas), Some(function)) => {
            Some(Spawner::Lambda { client: lambdas.client(&lambdas.here), function, qualifier: None })
        }
        (Backend::Local(bin), _) => Some(Spawner::Local { bin: bin.clone(), env: Vec::new(), isolated: false }),
        (Backend::Lambda(_), None) => None,
    }
    .map(|spawner| Pool::new(template.clone(), spawner));
    if let Some(pool) = &default_pool {
        tokio::spawn(pool.clone().reap());
    }
    let source = match routes {
        Some(spec) if spec.starts_with("s3://") => {
            let client = s3_client().await;
            Some(Source::parse(&spec, || client)?)
        }
        Some(spec) => Some(Source::parse(&spec, || unreachable!("a file needs no S3 client"))?),
        None => None,
    };
    let polled = source.is_some();
    let registry = Registry::new(source, backend, template, default_pool, base_domain);
    if polled {
        registry.refresh().await;
        let every = Duration::from_millis(env_num("FORKRAIL_ROUTES_POLL_MS", 1000)?.max(100));
        tokio::spawn(registry.clone().poll(every));
    }
    Ok(registry)
}

/// Where static files are read from: `FORKRAIL_STATIC_BUCKET`, or the routing table's bucket named
/// for this router's region, as the CLI's `code_bucket` names it. `None` with `FORKRAIL_STATIC=0`,
/// or without a bucket to read.
async fn statics_from_env() -> Result<Option<Arc<statics::Statics>>, BoxError> {
    if env_or("FORKRAIL_STATIC", "1") == "0" {
        return Ok(None);
    }
    let cache = env_num("FORKRAIL_STATIC_CACHE_MB", 64)? as usize * (1 << 20);
    if let Ok(dir) = std::env::var("FORKRAIL_STATIC_DIR") {
        info!(%dir, "static files from a directory");
        return Ok(Some(Arc::new(statics::Statics::new(statics::Source::Dir(PathBuf::from(dir)), cache))));
    }
    let config = aws_config::defaults(BehaviorVersion::latest())
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(3))
                .operation_timeout(Duration::from_secs(10))
                .build(),
        )
        .load()
        .await;
    let here = config.region().map(|r| r.to_string()).unwrap_or_default();
    let bucket = match std::env::var("FORKRAIL_STATIC_BUCKET") {
        Ok(bucket) if !bucket.is_empty() => bucket,
        _ => {
            let Some(routes_bucket) = std::env::var("FORKRAIL_ROUTES")
                .ok()
                .and_then(|r| r.strip_prefix("s3://").and_then(|r| r.split_once('/')).map(|(b, _)| b.to_owned()))
            else {
                return Ok(None);
            };
            let state_region = std::env::var("FORKRAIL_STATE_REGION").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| here.clone());
            if here.is_empty() { routes_bucket } else { forkrail_common::install::code_bucket(&routes_bucket, &state_region, &here) }
        }
    };
    info!(%bucket, region = %here, cache_mb = cache >> 20, "static files from S3");
    let source = statics::Source::S3 { client: aws_sdk_s3::Client::new(&config), bucket };
    Ok(Some(Arc::new(statics::Statics::new(source, cache))))
}

/// Image Optimization, when there are static files to make images of: the optimizer function in
/// this region, or a local one at `FORKRAIL_IMAGE_URL`, keeping what it makes in the install's one
/// store (`image_store`).
async fn images_from_env(statics: Option<Arc<statics::Statics>>) -> Result<Option<Arc<images::Images>>, BoxError> {
    let Some(statics) = statics else { return Ok(None) };
    let kept = image_store(&statics).await;
    let memory = env_num("FORKRAIL_IMAGE_CACHE_MB", 64)? as usize * (1 << 20);
    let optimizer = match std::env::var("FORKRAIL_IMAGE_URL").ok().filter(|u| !u.is_empty()) {
        Some(url) => {
            info!(%url, "image optimizer: a local one");
            let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http();
            images::Optimizer::Http { url: url.parse().map_err(|e| format!("FORKRAIL_IMAGE_URL: {e}"))?, client }
        }
        None => {
            let function = env_or("FORKRAIL_IMAGE_FUNCTION", "forkrail-image");
            let config = aws_config::defaults(BehaviorVersion::latest())
                .timeout_config(TimeoutConfig::builder().connect_timeout(Duration::from_secs(5)).operation_timeout(Duration::from_secs(30)).build())
                // A retried invoke would resize the same image twice; the next request asks again.
                .retry_config(RetryConfig::disabled())
                .load()
                .await;
            info!(%function, region = config.region().map(|r| r.to_string()).unwrap_or_default(), "image optimizer: Lambda");
            images::Optimizer::Lambda { client: aws_sdk_lambda::Client::new(&config), function }
        }
    };
    Ok(Some(Arc::new(images::Images::new(statics, kept, optimizer, memory))))
}

/// Where optimized images are kept: the home bucket's `images/`, one copy for every region (Vercel's
/// shared cache), read from here across regions on a memory miss. `FORKRAIL_IMAGE_BUCKET` names the
/// bucket; by default it's the routing table's, in `FORKRAIL_STATE_REGION`. Local runs
/// (`FORKRAIL_STATIC_DIR`), and a table not in S3, keep them beside the static files.
async fn image_store(statics: &Arc<statics::Statics>) -> Arc<statics::Statics> {
    if std::env::var("FORKRAIL_STATIC_DIR").is_ok() {
        return statics.clone();
    }
    let bucket = std::env::var("FORKRAIL_IMAGE_BUCKET").ok().filter(|b| !b.is_empty()).or_else(|| {
        std::env::var("FORKRAIL_ROUTES").ok().and_then(|r| r.strip_prefix("s3://").and_then(|r| r.split_once('/')).map(|(b, _)| b.to_owned()))
    });
    let Some(bucket) = bucket else { return statics.clone() };
    info!(%bucket, region = std::env::var("FORKRAIL_STATE_REGION").unwrap_or_default(), "optimized images kept in the home bucket");
    // Nothing kept in memory here: `Images` keeps the images themselves.
    Arc::new(statics::Statics::new(statics::Source::S3 { client: s3_client().await, bucket }, 0))
}

/// The response cache (`cache.rs`), from the environment, with its page store on disk and its link
/// to the other routers.
async fn cache_from_env(routers: &routers::Routers) -> Result<Option<Arc<cache::ResponseCache>>, BoxError> {
    if env_or("FORKRAIL_RESPONSE_CACHE", "1") == "0" {
        info!("FORKRAIL_RESPONSE_CACHE=0: no function answer is kept");
        return Ok(None);
    }
    let memory = env_num("FORKRAIL_RESPONSE_CACHE_MB", 64)? as usize * (1 << 20);
    let page_memory = env_num("FORKRAIL_PAGES_MEMORY_MB", 32)? as usize * (1 << 20);
    let defaults = env_or("FORKRAIL_CACHE_DEFAULTS", "1") != "0";
    let dir = PathBuf::from(env_or("FORKRAIL_PAGES_DIR", cache::DEFAULT_PAGES_DIR));
    let disk_max = env_num("FORKRAIL_PAGES_DISK_MB", 2048)? << 20;
    let free_floor = env_num("FORKRAIL_PAGES_FREE_MB", 1024)? << 20;
    let disk = match cache::Disk::open(dir.clone(), disk_max, free_floor) {
        Ok(disk) => Some(disk),
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "the page store can't be opened: pages are kept in memory only");
            None
        }
    };
    let link = link_from(routers).await?;
    let cache = cache::ResponseCache::new(memory, page_memory, disk, link.clone(), defaults);
    info!(
        memory_mb = memory >> 20,
        page_memory_mb = page_memory >> 20,
        pages = %cache.describe(),
        disk_mb = disk_max >> 20,
        free_mb = free_floor >> 20,
        home = link.as_ref().map(|l| l.home.as_str()),
        defaults,
        tag_marks = cache.tags.len(),
        "response cache"
    );
    Ok(Some(Arc::new(cache)))
}

/// The install's routers (`routers.rs`): `FORKRAIL_PEERS`, and what the routers say of themselves
/// in the state bucket (when the table is in S3), this one included, read once now.
async fn routers_from_env(registry: &Registry, tunnel_addr: &str) -> Result<Arc<routers::Directory>, BoxError> {
    let pinned = cache::peers::parse_peers(&env_or("FORKRAIL_PEERS", "")).map_err(|e| format!("FORKRAIL_PEERS: {e}"))?;
    let me = std::env::var("AWS_REGION").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| registry.here());
    let peer_listen = env_or("FORKRAIL_PEER_LISTEN", cache::peers::DEFAULT_LISTEN);
    let own = routers::own_addr(std::env::var("FORKRAIL_PEER_ADDR").ok().as_deref(), &me, &pinned, tunnel_addr, &peer_listen);
    let bucket = match state_bucket() {
        Some(name) if !me.is_empty() => Some(routers::Bucket { client: s3_client().await, name }),
        _ => None,
    };
    let directory = routers::Directory::new(me, own, pinned, bucket);
    directory.load().await;
    Ok(directory)
}

/// The state bucket, when the routing table is in S3 (`FORKRAIL_ROUTES=s3://bucket/key`).
fn state_bucket() -> Option<String> {
    std::env::var("FORKRAIL_ROUTES").ok().and_then(|r| r.strip_prefix("s3://").and_then(|r| r.split_once('/')).map(|(b, _)| b.to_owned()))
}

/// The link to the other routers (`cache/peers.rs`): the routers known now (`routers.rs`), this
/// router's region (`AWS_REGION`), home's (`FORKRAIL_STATE_REGION`), and the routers' shared key.
/// Every router of an install whose table is in S3 has one, alone or not, so a router added later
/// links to it; a local run only with `FORKRAIL_PEERS`.
async fn link_from(routers: &routers::Routers) -> Result<Option<Arc<cache::Link>>, BoxError> {
    let named = !env_or("FORKRAIL_PEERS", "").trim().is_empty();
    if !named && state_bucket().is_none() {
        return Ok(None);
    }
    let me = routers.me.clone();
    if me.is_empty() {
        if named {
            return Err("FORKRAIL_PEERS needs AWS_REGION, this router's region".into());
        }
        warn!("this router's region isn't known (AWS_REGION): no link to the other routers");
        return Ok(None);
    }
    let home = std::env::var("FORKRAIL_STATE_REGION").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| me.clone());
    if !routers.has(&home) {
        warn!(%home, "no router is known in the home region");
    }
    let Some(key) = peer_key().await else {
        warn!("no routers' key: this router keeps its own pages, and runs other regions' functions from here");
        return Ok(None);
    };
    let peers = routers.peers();
    info!(%me, %home, peers = ?peers.keys().collect::<Vec<_>>(), "linked with the other routers");
    Ok(Some(Arc::new(cache::Link::new(me, home, peers, &key)?)))
}

/// The routers' shared key for their link: `FORKRAIL_PEER_KEY` (hex, for local runs), else the
/// one in SSM (`FORKRAIL_PEER_KEY_PARAM`, default `/forkrail/router/peer-key`).
async fn peer_key() -> Option<Vec<u8>> {
    let hex = match std::env::var("FORKRAIL_PEER_KEY") {
        Ok(hex) => hex,
        // A local run has no SSM to ask.
        Err(_) if !std::env::var("FORKRAIL_ROUTES").is_ok_and(|r| r.starts_with("s3://")) => return None,
        Err(_) => {
            let name = env_or("FORKRAIL_PEER_KEY_PARAM", forkrail_common::install::PEER_KEY_PARAM);
            match ssm_client().await.get_parameter().name(&name).with_decryption(true).send().await {
                Ok(out) => out.parameter.and_then(|p| p.value)?,
                Err(e) => {
                    warn!(param = %name, error = %aws_sdk_ssm::error::DisplayErrorContext(&e), "no routers' key");
                    return None;
                }
            }
        }
    };
    let hex = hex.trim();
    if hex.len() < 32 || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        warn!("the routers' key isn't hex of 16 bytes or more");
        return None;
    }
    Some((0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect())
}

/// The certificate for the TLS listener, from the environment.
async fn certs_from_env(base: &str) -> Result<Arc<Certs>, BoxError> {
    let spec = std::env::var("FORKRAIL_TLS_CERTS").map_err(|_| "FORKRAIL_TLS_LISTEN needs FORKRAIL_TLS_CERTS")?;
    let store = if spec.starts_with("s3://") {
        let client = s3_client().await;
        certs::Store::parse(&spec, || client)?
    } else {
        certs::Store::parse(&spec, || unreachable!("a directory needs no S3 client"))?
    };
    let zone = std::env::var("FORKRAIL_CLOUDFLARE_ZONE").ok().filter(|z| !z.is_empty());
    let acme = if zone.is_some() || env_or("FORKRAIL_ACME", "0") == "1" {
        let addresses = env_or("FORKRAIL_PUBLIC_ADDRESSES", "")
            .split(',')
            .filter(|a| !a.trim().is_empty())
            .map(|a| a.trim().parse::<std::net::Ipv4Addr>().map_err(|e| format!("FORKRAIL_PUBLIC_ADDRESSES: {a}: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        Some(acme::Acme {
            staging: env_or("FORKRAIL_ACME_STAGING", "0") == "1",
            base: base.to_owned(),
            zone,
            token_param: env_or("FORKRAIL_CLOUDFLARE_TOKEN_PARAM", forkrail_common::install::CLOUDFLARE_TOKEN_PARAM),
            ssm: ssm_client().await,
            http01: Some(challenge_store(&store).await?),
            addresses,
            cache: Default::default(),
        })
    } else {
        None
    };
    Ok(Certs::new(base, store, acme))
}

/// Where HTTP-01 key authorizations are kept: `FORKRAIL_ACME_CHALLENGES`, or `acme/http-01` beside
/// the certificates.
async fn challenge_store(certs: &certs::Store) -> Result<certs::Store, BoxError> {
    match std::env::var("FORKRAIL_ACME_CHALLENGES") {
        Ok(spec) if spec.starts_with("s3://") => {
            let client = s3_client().await;
            certs::Store::parse(&spec, || client)
        }
        Ok(spec) => certs::Store::parse(&spec, || unreachable!("a directory needs no S3 client")),
        Err(_) => Ok(certs.sibling("acme/http-01")),
    }
}

/// Where Web Analytics' daily salts are shared (`insights.rs`): `FORKRAIL_INSIGHTS_SALTS`
/// (`s3://bucket/prefix`, or `local` for this router's own), else `state/insights/salts/` in the
/// routing table's bucket, else this router's own.
async fn salts_from_env() -> insights::Salts {
    let place = match std::env::var("FORKRAIL_INSIGHTS_SALTS") {
        Ok(v) if v == "local" => None,
        Ok(v) => v.strip_prefix("s3://").and_then(|r| r.split_once('/')).map(|(b, p)| (b.to_owned(), p.to_owned())),
        Err(_) => std::env::var("FORKRAIL_ROUTES")
            .ok()
            .and_then(|r| r.strip_prefix("s3://").and_then(|r| r.split_once('/')).map(|(b, _)| b.to_owned()))
            .map(|bucket| (bucket, insights::SALTS_PREFIX.to_owned())),
    };
    match place {
        Some((bucket, prefix)) => {
            info!(%bucket, %prefix, "web analytics: the days' salts are shared");
            insights::Salts::shared(s3_client().await, bucket, prefix)
        }
        None => insights::Salts::local(),
    }
}

/// What port 80 answers HTTP-01 challenges from: the challenge store, when there are certificates.
async fn challenges_from_env() -> Result<Option<proxy::Challenges>, BoxError> {
    let Ok(spec) = std::env::var("FORKRAIL_TLS_CERTS") else { return Ok(None) };
    let certs = if spec.starts_with("s3://") {
        let client = s3_client().await;
        certs::Store::parse(&spec, || client)?
    } else {
        certs::Store::parse(&spec, || unreachable!("a directory needs no S3 client"))?
    };
    let challenges = proxy::Challenges::new(challenge_store(&certs).await?);
    info!(store = %challenges.describe(), "HTTP challenges answered from");
    Ok(Some(challenges))
}

fn tls_acceptor(certs: &Certs) -> Result<TlsAcceptor, BoxError> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_no_client_auth()
        .with_cert_resolver(certs.resolver());
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[derive(Clone, Copy)]
enum Listener {
    Plain(Entry),
    Tls,
    Redirect,
}

/// Accept connections on `listener` until `stop`, then let them finish.
async fn serve(listener: TcpListener, kind: Listener, proxy: Arc<Proxy>, tls: Option<TlsAcceptor>, mut stop: watch::Receiver<bool>) {
    let mut server = auto::Builder::new(TokioExecutor::new());
    // As many header fields as Vercel takes (500, the request's Host among them; past it HTTP/1's
    // parser answers 431), and an HTTP/2 header block big enough for the router's own check
    // (`proxy::oversized_head`) to be what refuses one, as on HTTP/1.
    server.http1().timer(TokioTimer::new()).header_read_timeout(HEADER_READ_TIMEOUT).max_headers(500);
    server.http2().max_header_list_size(256 << 10);
    let server = Arc::new(server);
    let graceful = GracefulShutdown::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stop.wait_for(|stop| *stop) => break,
        };
        let (tcp, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!(error = %e, "accept failed");
                // Out of descriptors, say: tried again a little later, not in a busy loop.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        // The system mitigations first: a blocked address, or one with too many connections open,
        // is closed before anything is spent on it (no handshake, no log line).
        let admitted = match kind {
            Listener::Plain(Entry::Schedule) => None,
            _ => match proxy.shield.admit(peer.ip()) {
                Ok(conn) => Some(conn),
                Err(_) => continue,
            },
        };
        let _ = tcp.set_nodelay(true);
        // The spend ledger reads what the kernel counted on it (`spend.rs`): a schedule's run, from
        // this machine to itself, costs nothing.
        let meter = match kind {
            Listener::Plain(Entry::Schedule) => None,
            _ => {
                let zone = proxy.geo.lookup(peer.ip().to_canonical()).country.as_deref().and_then(forkrail_common::prices::Zone::of_country);
                Some(spend::ledger().client(wire::hold(&tcp), zone))
            }
        };
        let watcher = graceful.watcher();
        let (server, proxy, tls) = (server.clone(), proxy.clone(), tls.clone());
        tokio::spawn(async move {
            let _admitted = admitted;
            match (kind, tls) {
                (Listener::Tls, Some(tls)) => {
                    // In the connection's task, so a slow handshake never holds up accepting. The
                    // ClientHello is read first for the client's JA4 fingerprint, then handed to
                    // rustls as it came.
                    // The host it names may start a worker for its first request now, so the
                    // rest of the handshake runs alongside the start (`early.rs`).
                    let at_hello = proxy.clone();
                    let handshake = async move {
                        let mut tcp = tcp;
                        let (hello, ja4) = ja4::read_hello(&mut tcp).await?;
                        let early = early::start(&at_hello, &hello, peer.ip());
                        let stream = tls.accept(ja4::Rewind::new(hello, tcp)).await?;
                        Ok::<_, std::io::Error>((stream, ja4.map(Arc::<str>::from), early))
                    };
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, handshake).await {
                        Ok(Ok((stream, ja4, early))) => connection(stream, peer, Listener::Tls, &server, proxy, watcher, ja4, early, meter.clone()).await,
                        Ok(Err(e)) => {
                            TLS_FAILED.fetch_add(1, Ordering::Relaxed);
                            debug!(%peer, error = %e, "TLS handshake failed");
                        }
                        Err(_) => {
                            TLS_TIMED_OUT.fetch_add(1, Ordering::Relaxed);
                            debug!(%peer, "TLS handshake timed out");
                        }
                    }
                }
                (kind, _) => connection(tcp, peer, kind, &server, proxy, watcher, None, None, meter.clone()).await,
            }
            // Its last counts, a failed handshake's included (they're the install's).
            if let Some(meter) = meter {
                meter.close(spend::ledger());
            }
        });
    }
    drop(listener);
    let until = tokio::time::Instant::now() + CLOSE_GRACE;
    while graceful.count() > 0 && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    graceful.shutdown().await;
}

/// While the router drains, an HTTP/1 response asks the client to close the connection after it.
fn closing(mut response: hyper::Response<proxy::ResponseBody>, version: hyper::Version) -> hyper::Response<proxy::ResponseBody> {
    if version <= hyper::Version::HTTP_11 && DRAINING.load(Ordering::Relaxed) {
        response.headers_mut().insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("close"));
    }
    response
}

async fn connection<IO>(
    io: IO,
    peer: SocketAddr,
    kind: Listener,
    server: &auto::Builder<TokioExecutor>,
    proxy: Arc<Proxy>,
    watcher: hyper_util::server::graceful::Watcher,
    ja4: Option<Arc<str>>,
    early: Option<early::Expected>,
    meter: Option<Arc<spend::Conn>>,
) where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(io);
    let result = match kind {
        Listener::Redirect => {
            let meter = meter.clone();
            let service = service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                if let Some(meter) = &meter {
                    req.extensions_mut().insert(meter.clone());
                }
                let version = req.version();
                let response = proxy::port80(req, proxy.clone(), peer);
                async move { Ok::<_, Infallible>(closing(response.await, version)) }
            });
            watcher.watch(server.serve_connection(io, service).into_owned()).await
        }
        Listener::Plain(entry) => {
            let meter = meter.clone();
            let service = service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                if let Some(meter) = &meter {
                    req.extensions_mut().insert(meter.clone());
                }
                let version = req.version();
                let response = proxy::handle(req, proxy.clone(), peer, entry);
                async move { response.await.map(|r| closing(r, version)) }
            });
            watcher.watch(server.serve_connection(io, service).into_owned()).await
        }
        Listener::Tls => {
            // A worker started at the handshake is kept for the connection's first request until
            // it has its answer (or the connection ends).
            let early = std::sync::Mutex::new(early);
            let meter = meter.clone();
            let service = service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                if let Some(ja4) = &ja4 {
                    req.extensions_mut().insert(proxy::Ja4(ja4.clone()));
                }
                if let Some(meter) = &meter {
                    req.extensions_mut().insert(meter.clone());
                }
                let version = req.version();
                let expected = early.lock().unwrap().take();
                let response = proxy::handle(req, proxy.clone(), peer, Entry::Https);
                async move {
                    let response = response.await;
                    drop(expected);
                    response.map(|r| closing(r, version))
                }
            });
            watcher.watch(server.serve_connection(io, service).into_owned()).await
        }
    };
    if let Err(e) = result {
        debug!(%peer, error = %e, "client connection ended");
    }
}

/// A listener another router can bind beside this one (`SO_REUSEPORT`), so a restart starts the new
/// process before the old one stops. Once the old one closes its listeners, every new connection
/// goes to the new one, and with `net.ipv4.tcp_migrate_req` so do those already queued on the old.
/// Linux lets only processes of the same user share a port this way.
fn shared_listener(addr: &str) -> Result<TcpListener, BoxError> {
    let addr: SocketAddr = addr.parse().map_err(|e| format!("{addr}: {e}"))?;
    let socket = socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into()).map_err(|e| format!("binding {addr}: {e}"))?;
    socket.listen(LISTEN_BACKLOG)?;
    Ok(TcpListener::from_std(socket.into())?)
}

/// The tunnel listener: the first of `listen`'s comma-separated addresses that's free. Each router
/// takes its own, so during a restart a worker always reaches the router that spawned it, the one
/// holding its token. Workers are told `advertise`'s host with the port that was free.
async fn tunnel_listener(listen: &str, advertise: &str) -> Result<(TcpListener, String), BoxError> {
    let mut last = None;
    for addr in listen.split(',').map(str::trim).filter(|a| !a.is_empty()) {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                let port = listener.local_addr()?.port();
                let host = advertise.rsplit_once(':').map_or(advertise, |(host, _)| host);
                return Ok((listener, format!("{host}:{port}")));
            }
            Err(e) => last = Some(format!("{addr}: {e}")),
        }
    }
    Err(format!("no tunnel address free in {listen:?} ({})", last.unwrap_or_default()).into())
}

/// Tell systemd the router is serving (`Type=notify`), if it's watching. A restart waits for this
/// before it stops the old router.
fn notify_ready() {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else { return };
    let sent = (|| {
        let socket = std::os::unix::net::UnixDatagram::unbound()?;
        #[cfg(target_os = "linux")]
        if let Some(name) = path.strip_prefix('@') {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            return socket.send_to_addr(b"READY=1", &addr);
        }
        socket.send_to(b"READY=1", &path)
    })();
    match sent {
        Ok(_) => info!("told systemd the router is ready"),
        Err(e) => warn!(error = %e, "telling systemd the router is ready failed"),
    }
}

/// Without `FORKRAIL_LOG_DIR`, everything goes to stdout. With it, to hourly files there
/// (`router.YYYY-MM-DD-HH.log`), written by a background thread through a buffer, so a request
/// never waits on a write; warnings also go to stderr, which systemd keeps in the journal. The
/// journal can't keep up: at 1,000 requests a second it cost as much CPU as the router, and it
/// dropped lines (2026-11-06). Returns the guard that flushes the files on exit.
///
/// The files hold one JSON object per line (since 2026-11-09; text before): `timestamp`,
/// `level`, `message` (the event: `request`, `invocation`, `app log`, ...), its fields, and
/// `target`. Vector ships them to ClickHouse as they're written, and the log export to S3.
fn init_logging() -> Result<Option<tracing_appender::non_blocking::WorkerGuard>, BoxError> {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::prelude::*;
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let Ok(dir) = std::env::var("FORKRAIL_LOG_DIR") else {
        tracing_subscriber::fmt().with_env_filter(filter()).init();
        return Ok(None);
    };
    let files = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::HOURLY)
        .filename_prefix("router")
        .filename_suffix("log")
        .build(&dir)?;
    // Lines wait in memory rather than being dropped if the disk falls behind.
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(false)
        .finish(std::io::BufWriter::with_capacity(LOG_BUFFER, files));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(false)
                .with_span_list(false)
                .with_writer(writer)
                .with_filter(filter()),
        )
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::io::stderr).with_filter(LevelFilter::WARN))
        .init();
    Ok(Some(guard))
}

/// The page store's index and sweeps, and the link to the other routers: answering them, and
/// (an edge) following home. A store that's new with no router to copy from is lost.
fn start_pages(proxy: &Arc<Proxy>, cache: Arc<cache::ResponseCache>, routers: tokio::sync::watch::Receiver<Arc<routers::Routers>>) {
    let registry = proxy.registry.clone();
    let deployments = move || registry.deployment_ids();
    // A port that can't be had leaves this router unable to answer the others, not to serve: it
    // still follows home (as an edge) and serves its own pages.
    let listen = env_or("FORKRAIL_PEER_LISTEN", cache::peers::DEFAULT_LISTEN);
    let listener = cache.link.as_ref().and_then(|_| match shared_listener(&listen) {
        Ok(listener) => Some(listener),
        Err(e) => {
            warn!(%listen, error = %e, "the peer port can't be had: other routers can't ask this one for pages");
            None
        }
    });
    let renders = proxy::peer_renders(proxy.clone());
    let runs = proxy::peer_runs(proxy.clone());
    // Next.js's caches live in the same store (`cache/next.rs`), their seeds beside the statics.
    let data_max = env_num("FORKRAIL_NEXT_DATA_MB", 256).unwrap_or(256) << 20;
    let next = cache::next::start(cache.clone(), proxy.statics.clone(), data_max);
    let table = proxy.registry.clone();
    tokio::spawn(async move {
        if let Some(disk) = cache.disk().cloned() {
            disk.scan().await;
            tokio::spawn(disk.sweep_every(cache::SWEEP_EVERY));
            // What's kept for deployments and projects no longer in the table goes with them.
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(cache::SWEEP_EVERY).await;
                    let (deployments, projects) = table.deployment_projects().into_iter().unzip();
                    next.sweep(&deployments, &projects).await;
                }
            });
        }
        match cache.link.clone() {
            Some(link) => {
                if let Some(listener) = listener {
                    tokio::spawn(link.clone().serve(listener, cache.clone(), renders, runs));
                }
                if env_or("FORKRAIL_RELAY", "1") == "0" {
                    info!("FORKRAIL_RELAY=0: deployments that run in another region start their workers there from here");
                } else {
                    relay::start(link.clone(), routers);
                }
                link.run(cache, deployments).await;
            }
            None if cache.store_is_new() => cache.mark_lost(deployments()),
            None => {}
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let _log_guard = init_logging()?;
    // The AWS SDK brings aws-lc-rs too; make ring the default for clients that don't pick one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Some(queue) = std::env::var("FORKRAIL_EVENTS_QUEUE").ok().filter(|q| !q.is_empty()) {
        events::start(queue).await;
    }
    if let Some(queue) = std::env::var("FORKRAIL_SPEND_QUEUE").ok().filter(|q| !q.is_empty()) {
        events::start_spend(queue).await;
    }

    // A sink that can't start isn't worth a router that can't: lines then go to the log, as before.
    let log_sink = match sink_config().and_then(|cfg| cfg.map(sink::start).transpose()) {
        Ok(sink) => sink,
        Err(e) => {
            error!(error = %e, "the ClickHouse log sink didn't start; app logs go to the router's log");
            None
        }
    };

    let listen = env_or("FORKRAIL_LISTEN", "0.0.0.0:8080");
    let tunnel_listen = env_or("FORKRAIL_TUNNEL_LISTEN", "0.0.0.0:7443");
    let (tunnel_listener, tunnel_addr) =
        tunnel_listener(&tunnel_listen, &env_or("FORKRAIL_TUNNEL_ADDR", "127.0.0.1:7443")).await?;
    let tls_listen = std::env::var("FORKRAIL_TLS_LISTEN").ok();
    let redirect_listen = std::env::var("FORKRAIL_REDIRECT_LISTEN").ok();
    let base_domain = std::env::var("FORKRAIL_BASE_DOMAIN").ok().map(|b| normalize_host(&b)).filter(|b| !b.is_empty());
    // Registered before anything else, since SIGHUP's default action is to exit.
    let mut hangup = signal(SignalKind::hangup())?;
    let mut user1 = signal(SignalKind::user_defined1())?;
    let mut terminate = signal(SignalKind::terminate())?;

    let tls = tls::self_signed()?;
    let idle_auto = env_or("FORKRAIL_IDLE_MS", "auto") == "auto";
    let registry = registry_from_env(
        PoolConfig {
            name: String::new(),
            max_inflight: env_num("FORKRAIL_MAX_INFLIGHT", 10)? as u32,
            idle: Duration::from_millis(match idle_auto {
                true => economic_idle_ms(DEFAULT_MEMORY_MB),
                false => env_num("FORKRAIL_IDLE_MS", 0)?,
            }),
            idle_auto,
            max_age: Duration::from_secs(env_num("FORKRAIL_MAX_AGE_SECS", 780)?),
            headroom_pct: env_num("FORKRAIL_HEADROOM_PCT", 10)? as u32,
            tunnel_addr: tunnel_addr.clone(),
            cert_sha256: tls.cert_sha256,
            acquire_timeout: ACQUIRE_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            cpu_target_pct: env_num("FORKRAIL_CPU_TARGET_PCT", 0)? as u32,
            profile_pct: env_num("FORKRAIL_PROFILE_PCT", 0)?.min(100) as u32,
            max_workers: env_cap("FORKRAIL_POOL_MAX_WORKERS")?,
            budget: WorkerBudget::new(env_cap("FORKRAIL_MAX_WORKERS")?),
            max_duration: Duration::from_secs(env_num("FORKRAIL_MAX_DURATION_SECS", 300)?.max(1)),
            project: String::new(),
            region: String::new(),
            memory_mb: 0,
            relay: None,
            environment: String::new(),
            runtime: String::new(),
            entry: String::new(),
            egress: Vec::new(),
        },
        base_domain.clone(),
    )
    .await?;
    // The spend ledger, before any worker starts: it prices bytes by this router's region.
    tokio::spawn(spend::init(&registry.here()).clone().run());
    // Every router with its table in S3 reads forkrail's own costs as the guard counted them; a
    // public one (with a base domain) also publishes its month to date for the guard.
    let files = match state_bucket() {
        Some(bucket) => Some(budgets::Files { client: s3_client().await, bucket, publish: base_domain.is_some().then(|| format!("state/spend/{}.json", registry.here())) }),
        None => None,
    };
    tokio::spawn(budgets::budgets().run(registry.clone(), files));

    let egress = tunnel::parse_egress(&env_or("FORKRAIL_EGRESS_TARGETS", "")).map_err(|e| format!("FORKRAIL_EGRESS_TARGETS: {e}"))?;
    if !egress.is_empty() {
        info!(targets = ?egress.keys().collect::<Vec<_>>(), "egress targets");
    }
    // The install's routers, known before any pool is made or the link opens: how a deployment's
    // requests reach its function is settled with its pool (`routers.rs`).
    let directory = routers_from_env(&registry, &tunnel_addr).await?;
    registry.set_routers(directory.get());
    // The response cache, made before the tunnel so functions' purges reach it.
    let cache = cache_from_env(&directory.get()).await?;
    {
        let registry = registry.clone();
        let link = cache.as_ref().and_then(|c| c.link.clone());
        let mut known = directory.subscribe();
        tokio::spawn(async move {
            while known.changed().await.is_ok() {
                let routers = known.borrow_and_update().clone();
                registry.set_routers(routers.clone());
                if let Some(link) = &link {
                    link.set_peers(routers.peers());
                }
            }
        });
    }
    tokio::spawn(directory.clone().run());
    if let Some(cache) = cache.clone() {
        let mut purges = registry.table_purges();
        tokio::spawn(async move {
            loop {
                let table = purges.borrow_and_update().clone();
                let changed = cache.table_purges(&table);
                if changed > 0 {
                    info!(marks = changed, "purges from the routing table");
                }
                if purges.changed().await.is_err() {
                    return;
                }
            }
        });
    }
    tokio::spawn(tunnel::serve(tunnel_listener, tls.acceptor, registry.clone(), Arc::new(egress), cache.clone()));

    let keep_warm = keepwarm::Settings {
        keep: match env_or("FORKRAIL_KEEP_WARM", "0").parse()? {
            forkrail_common::routes::KeepWarm::Count(n) => forkrail_common::routes::KeepWarm::Count(n.min(forkrail_common::routes::KEEP_WARM_MAX)),
            auto => auto,
        },
        every: Duration::from_secs(env_num("FORKRAIL_KEEP_WARM_EVERY_S", keepwarm::EVERY.as_secs())?.max(10)),
        lock: Some(env_or("FORKRAIL_KEEP_WARM_LOCK", keepwarm::DEFAULT_LOCK)).filter(|l| l != "none" && !l.is_empty()),
        unused_after: Duration::from_secs(env_num("FORKRAIL_KEEP_WARM_UNUSED_S", keepwarm::UNUSED_AFTER.as_secs())?),
        state: Some(env_or("FORKRAIL_KEEP_WARM_STATE", keepwarm::DEFAULT_STATE)).filter(|p| p != "none" && !p.is_empty()).map(PathBuf::from),
    };
    early::set(matches!(env_or("FORKRAIL_HANDSHAKE_START", "0").as_str(), "1" | "on" | "true"));
    tokio::spawn(keepwarm::run(registry.clone(), keep_warm));

    let default_pool = registry.default_pool().is_some();
    let mut proxy = Proxy::new(
        registry,
        env_num("FORKRAIL_BUFFER_BYTES", 8 << 20)? as usize,
        env_num("FORKRAIL_BUFFER_TOTAL_BYTES", 512 << 20)? as usize,
        {
            let dir = std::env::var("FORKRAIL_GEOIP_DIR").unwrap_or_else(|_| geo::DEFAULT_DIR.to_owned());
            let geo = geo::Geo::open(PathBuf::from(dir));
            geo.watch();
            geo
        },
        statics_from_env().await?,
    );
    proxy.cache = cache;
    proxy.salts = Arc::new(salts_from_env().await);
    tokio::spawn(proxy.salts.clone().tend());
    proxy.challenges = challenges_from_env().await?;
    proxy.images = images_from_env(proxy.statics.clone()).await?;
    proxy.firewalls = firewall::Firewalls::with_buckets(env_num("FORKRAIL_RATE_LIMIT_BUCKETS", ratelimit::DEFAULT_BUCKETS as u64)? as usize);
    tokio::spawn(proxy.firewalls.counters.clone().sweep_every(ratelimit::SWEEP_EVERY));
    let defaults = shield::Settings::default();
    proxy.shield.set_settings(shield::Settings {
        connections: env_num("FORKRAIL_SHIELD_CONNECTIONS", defaults.connections.into())? as u32,
        requests: env_num("FORKRAIL_SHIELD_REQUESTS", defaults.requests.into())? as u32,
        block_for: Duration::from_secs(env_num("FORKRAIL_SHIELD_BLOCK_S", defaults.block_for.as_secs())?),
    });
    tokio::spawn(proxy.shield.clone().follow(proxy.registry.shield_policy()));
    tokio::spawn(proxy.shield.clone().maintain());
    proxy.challenger = Arc::new(challenger_from_env().await?);
    proxy.vouches = Arc::new(visitor::Vouches::new(proxy.challenger.derive(b"forkrail-visitor")));
    proxy.loops = Arc::new(loops::Marks::new(
        proxy.challenger.derive(b"forkrail-loop"),
        env_num("FORKRAIL_LOOP_MAX_DEPTH", loops::DEFAULT_MAX_DEPTH.into())? as u32,
        env_num("FORKRAIL_LOOP_MAX_CHAIN", loops::DEFAULT_MAX_CHAIN.into())? as u32,
    ));
    info!(shared_key = proxy.challenger.shared, bits = proxy.challenger.bits(), "firewall challenge ready");
    if env_or("FORKRAIL_BOTS", "1") != "0" {
        tokio::spawn(proxy.bots.clone().follow(PathBuf::from(env_or("FORKRAIL_BOTS_DIR", bots::DEFAULT_DIR))));
    }
    // The firewall follows the routing table; the first configurations are in force before any
    // listener opens, so a restart never lets through what the firewall refuses.
    if let Some(statics) = proxy.statics.clone() {
        let refs = proxy.registry.firewall_refs();
        tokio::spawn(proxy.firewalls.clone().follow(refs.clone(), statics));
        if !proxy.firewalls.loaded(&refs, FIREWALL_LOAD_WAIT).await {
            warn!(wait_s = FIREWALL_LOAD_WAIT.as_secs(), "firewall configurations not all loaded yet; starting anyway");
        }
    } else if proxy.registry.firewall_refs().borrow().install.is_some() || !proxy.registry.firewall_refs().borrow().projects.is_empty() {
        warn!("FORKRAIL_STATIC=0: the routing table names firewall configurations, but there's nowhere to read them; no firewall");
    }
    // Schedules' runs come back in through a loopback listener of this process's own (see
    // `schedules.rs`), never shared with another router during a restart.
    let schedule_listener = TcpListener::bind("127.0.0.1:0").await?;
    let runs = schedules::Runs::new(schedule_listener.local_addr()?, proxy.registry.clone());
    proxy.schedules = Some(runs.clone());
    let proxy = Arc::new(proxy);
    if let Some(cache) = proxy.cache.clone() {
        start_pages(&proxy, cache, directory.subscribe());
    }
    let (stop_tx, stop) = watch::channel(false);
    let mut servers = Vec::new();
    servers.push(tokio::spawn(serve(schedule_listener, Listener::Plain(Entry::Schedule), proxy.clone(), None, stop.clone())));
    match std::env::var("FORKRAIL_SCHEDULE_QUEUE").ok().filter(|q| !q.is_empty()) {
        Some(queue) => {
            if base_domain.is_none() {
                warn!("FORKRAIL_SCHEDULE_QUEUE without FORKRAIL_BASE_DOMAIN: no run can find its production deployment");
            }
            let client = schedules::sqs_client(&queue).await;
            tokio::spawn(runs.clone().poll(client, queue, stop.clone()));
        }
        None => info!("FORKRAIL_SCHEDULE_QUEUE unset: schedules run only from the tester's port"),
    }

    // HTTPS starts once there's a certificate; the first issuance can take a minute. The router
    // tells systemd it's ready only once HTTPS is up, so a restart never stops the old router for a
    // new one without HTTPS; if it can't bind, it never says so, and the restart fails instead.
    let (renew_tx, renew_rx) = mpsc::channel(1);
    let mut all_certs = None;
    let (https_up_tx, https_up) = tokio::sync::oneshot::channel::<()>();
    let mut https_up = tls_listen.is_some().then_some(https_up);
    if let Some(tls_listen) = tls_listen.clone() {
        let base = base_domain.clone().ok_or("FORKRAIL_TLS_LISTEN needs FORKRAIL_BASE_DOMAIN")?;
        let certs = certs_from_env(&base).await?;
        all_certs = Some(certs.clone());
        let acceptor = tls_acceptor(&certs)?;
        tokio::spawn(certs.clone().maintain(renew_rx));
        tokio::spawn(certs.clone().maintain_domains(proxy.registry.domains()));
        tokio::spawn(tls_summary(certs.resolver()));
        let (proxy, mut stop) = (proxy.clone(), stop.clone());
        servers.push(tokio::spawn(async move {
            // The wildcard, and the custom domains' stored certificates, so a restart never
            // serves a domain without its own; a store that doesn't answer delays only so long.
            let loaded = async {
                certs.ready().await;
                if tokio::time::timeout(DOMAINS_LOAD_WAIT, certs.domains_loaded()).await.is_err() {
                    warn!(wait_s = DOMAINS_LOAD_WAIT.as_secs(), "custom domain certificates not loaded yet; starting HTTPS anyway");
                }
            };
            tokio::select! {
                _ = loaded => {}
                _ = stop.wait_for(|stop| *stop) => return,
            }
            match shared_listener(&tls_listen) {
                Ok(listener) => {
                    info!(%tls_listen, %base, "HTTPS up");
                    let _ = https_up_tx.send(());
                    serve(listener, Listener::Tls, proxy, Some(acceptor), stop).await;
                }
                Err(e) => warn!(%tls_listen, error = %e, "binding the HTTPS listener failed"),
            }
        }));
    }
    if let Some(redirect_listen) = &redirect_listen {
        let listener = shared_listener(redirect_listen)?;
        servers.push(tokio::spawn(serve(listener, Listener::Redirect, proxy.clone(), None, stop.clone())));
    }
    let listener = shared_listener(&listen)?;
    info!(%listen, %tunnel_listen, %tunnel_addr, tls_listen, redirect_listen, base_domain, default_pool, "router up");
    servers.push(tokio::spawn(serve(listener, Listener::Plain(Entry::Tester), proxy, None, stop)));
    if https_up.is_none() {
        notify_ready();
    }

    loop {
        tokio::select! {
            up = async { https_up.as_mut().expect("guarded").await }, if https_up.is_some() => {
                https_up = None;
                if up.is_ok() {
                    notify_ready();
                }
            }
            _ = hangup.recv() => {
                if tls_listen.is_none() || renew_tx.try_send(()).is_err() {
                    info!("SIGHUP: nothing to renew");
                }
            }
            _ = user1.recv() => match &all_certs {
                Some(certs) => certs.check_domains_now(),
                None => info!("SIGUSR1: no HTTPS listener, so no custom domains"),
            },
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    info!("shutting down: draining in-flight requests");
    DRAINING.store(true, Ordering::Relaxed);
    let _ = stop_tx.send(true);
    let drained = async {
        for server in servers {
            let _ = server.await;
        }
        // And the requests other routers sent to run here, which refuse new ones meanwhile.
        while relay::running() > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    match tokio::time::timeout(DRAIN_TIMEOUT, drained).await {
        Ok(()) => info!("drained"),
        Err(_) => warn!(timeout_s = DRAIN_TIMEOUT.as_secs(), "drain timed out; dropping what's left"),
    }
    // The last of the spend ledger's counts go with the sink's last batch.
    spend::ledger().finish();
    if let Some(log_sink) = log_sink {
        log_sink.finish().await;
    }
    Ok(())
}

/// Where app logs go when not into the router's own log: see `sink.rs`.
fn sink_config() -> Result<Option<sink::Config>, BoxError> {
    let Some(url) = std::env::var("FORKRAIL_CLICKHOUSE_URL").ok().filter(|url| !url.is_empty()) else {
        return Ok(None);
    };
    let password_file = env_or("FORKRAIL_CLICKHOUSE_PASSWORD_FILE", "/etc/forkrail/ch_password");
    let password = std::fs::read_to_string(&password_file).map_err(|e| format!("FORKRAIL_CLICKHOUSE_PASSWORD_FILE {password_file}: {e}"))?;
    let ca_pem = match std::env::var("FORKRAIL_CLICKHOUSE_CA") {
        Ok(path) => Some(std::fs::read(&path).map_err(|e| format!("FORKRAIL_CLICKHOUSE_CA {path}: {e}"))?),
        Err(_) => None,
    };
    let spill_dir = match std::env::var("FORKRAIL_LOG_SPILL_DIR") {
        Ok(dir) => Some(dir.into()),
        Err(_) => std::env::var("FORKRAIL_LOG_DIR").ok().map(|dir| std::path::Path::new(&dir).join("spill")),
    };
    Ok(Some(sink::Config {
        gzip: env_or("FORKRAIL_CLICKHOUSE_GZIP", if url.starts_with("https://") { "1" } else { "0" }) == "1",
        url,
        database: env_or("FORKRAIL_CLICKHOUSE_DATABASE", "forkrail"),
        user: env_or("FORKRAIL_CLICKHOUSE_USER", "ingest"),
        password: password.trim().to_owned(),
        ca_pem,
        spill_dir,
        spill_limit: env_num("FORKRAIL_LOG_SPILL_MB", 2048)? << 20,
        router: env_or("FORKRAIL_ROUTER_ID", "unknown"),
        router_region: env_or("AWS_REGION", "unknown"),
    }))
}

#[cfg(test)]
mod tests {
    use super::parse_cap;

    #[test]
    fn worker_caps() {
        assert_eq!(parse_cap(None), Ok(usize::MAX));
        assert_eq!(parse_cap(Some("none")), Ok(usize::MAX));
        assert_eq!(parse_cap(Some("")), Ok(usize::MAX));
        assert_eq!(parse_cap(Some("200")), Ok(200));
        for bad in ["0", "-1", "lots"] {
            assert!(parse_cap(Some(bad)).is_err(), "{bad}");
        }
    }
}
