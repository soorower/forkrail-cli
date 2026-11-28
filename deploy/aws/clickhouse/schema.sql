-- forkrail's analytics store: the routers' log, as Vector ships it (deploy/aws/vector/), in four
-- tables, and rollups that materialized views keep as rows arrive. Applied by
-- `deploy/aws/analytics.sh schema`; every statement is safe to run again.
--
-- What reads what (keep it this way, so the box stays small):
-- - A logs page, a request's details, live tail: the raw tables, always with `project` and a time
--   range, which the sorting key (project, time) turns into a read of a few granules.
-- - Charts and totals over hours to months: the rollups, never the raw tables.
-- - Usage and cost: invocations_1h (billed time) and requests_1m (requests, bytes); spend and
--   budgets: usage_1h.
--
-- Times are UTC. `time` is when the router wrote the line: for a request, when its response
-- ended (it started total_ms before); for an invocation, when it returned.

CREATE DATABASE IF NOT EXISTS forkrail;

-- One row per request the router answered, or whose client left.
CREATE TABLE IF NOT EXISTS forkrail.requests
(
    time DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)),
    request String CODEC(ZSTD(1)),                -- the id in x-forkrail-request-id
    project LowCardinality(String),
    deployment LowCardinality(String),
    host LowCardinality(String),
    entry LowCardinality(String),                 -- https, tester (8080)
    region LowCardinality(String),                -- where the function ran
    router_region LowCardinality(String),         -- where the router that took it is
    router LowCardinality(String),                -- that router's instance
    method LowCardinality(String),
    path String CODEC(ZSTD(1)),
    query String CODEC(ZSTD(3)),                  -- secret-looking parameters masked by the router
    client String CODEC(ZSTD(1)),
    proto LowCardinality(String),
    agent String CODEC(ZSTD(3)),
    accept_encoding LowCardinality(String),
    encoding LowCardinality(String),
    status UInt16,
    outcome LowCardinality(String),               -- ok, client_gone, not_found, ...
    attempts UInt8,
    queue_ms UInt32 CODEC(T64, ZSTD(1)),          -- waiting for a worker, over all attempts
    ttfb_ms Nullable(UInt32),
    total_ms UInt32 CODEC(T64, ZSTD(1)),
    worker_ms Nullable(UInt32),
    bytes_in Nullable(UInt64),
    bytes_out UInt64 CODEC(T64, ZSTD(1)),
    worker String CODEC(ZSTD(1)),
    -- Measured only for a request that had its worker to itself.
    mem_kb Nullable(UInt32),
    cpu_us Nullable(UInt32),
    busy_us Nullable(UInt32),
    cpu_profile String CODEC(ZSTD(3)),
    INDEX request_idx request TYPE bloom_filter(0.001) GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (project, time)
SETTINGS non_replicated_deduplication_window = 1000;

-- Added 2026-11-09: what Vercel's log details show about the function that served a request.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS referer String CODEC(ZSTD(3)),                -- masked like query
    ADD COLUMN IF NOT EXISTS route LowCardinality(String),                -- the app's template, /items/{id}
    ADD COLUMN IF NOT EXISTS start LowCardinality(String),                -- warm, thaw, cold: how it got its worker
    ADD COLUMN IF NOT EXISTS rss_kb Nullable(UInt32),                     -- the worker's memory at the response
    ADD COLUMN IF NOT EXISTS environment LowCardinality(String),          -- production, preview
    ADD COLUMN IF NOT EXISTS runtime LowCardinality(String),              -- python3.13, nodejs24.x
    ADD COLUMN IF NOT EXISTS entrypoint LowCardinality(String),           -- api/main.py
    ADD COLUMN IF NOT EXISTS memory_mb UInt16,
    ADD COLUMN IF NOT EXISTS max_duration_s UInt32;

-- Added 2026-11-09: where the client is, from DB-IP's free City and ASN databases on the router.
-- Empty for private addresses and for rows from before.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS country LowCardinality(String),             -- ISO 3166-1 alpha-2
    ADD COLUMN IF NOT EXISTS continent LowCardinality(String),
    ADD COLUMN IF NOT EXISTS subdivision LowCardinality(String),         -- the region's name
    ADD COLUMN IF NOT EXISTS city LowCardinality(String),
    ADD COLUMN IF NOT EXISTS latitude Nullable(Float32),
    ADD COLUMN IF NOT EXISTS longitude Nullable(Float32),
    ADD COLUMN IF NOT EXISTS asn UInt32,
    ADD COLUMN IF NOT EXISTS as_org LowCardinality(String);

-- Added 2026-11-10: lines of the app's output and outgoing calls of the request that the router
-- didn't keep: lines past what a request may log (256 lines, 1 MB, as on Vercel) when its
-- deployment's bucket for the rest was empty, and calls past 10,000 (crates/router/src/logs.rs).
-- Zero for rows from before. What was dropped after this row was written (lines can reach the
-- router after the response) is in router_events instead: message `late drops`, with the request
-- and the same two counts in `fields`.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS lines_dropped UInt32,
    ADD COLUMN IF NOT EXISTS calls_dropped UInt32;

-- Added 2026-11-13: the deployment's routing rule that decided the request, by where it came from
-- in vercel.json (`redirects[3]`, `routes[0]`, `Create React App's routes[5]`); empty when none
-- did. With `outcome` (static, redirect, external, ...) it says what answered.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS rule LowCardinality(String);

-- Added 2026-11-14: a schedule's run (crons and schedules, crates/router/src/schedules.rs): its
-- key (a schedules entry's name, or `c-<hash>` for a cron), the run's id (the same for every
-- delivery of one run; `manual-...` for Run now), and how long after its scheduled time the router
-- had it (Scheduler and the queue; empty for a manual run). Empty for visitors' requests. Runs have
-- entry `schedule`; ones that didn't start have outcome schedule_expired or schedule_gone.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS schedule LowCardinality(String),
    ADD COLUMN IF NOT EXISTS schedule_run String CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS schedule_delay_ms Nullable(UInt32);

-- Added 2026-11-14: of schedule_delay_ms, the time the run's message spent in the queue (from SQS
-- having it to a router having it); the rest is Scheduler's own delay. Empty for manual runs sent
-- from the tester's port.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS schedule_queue_ms Nullable(UInt32);

-- Added 2026-11-14: what the firewall did with a request (crates/router/src/firewall.rs): deny,
-- challenge, rate_limit, redirect, bypass, log, or held (a persistent block answered), and the
-- rule or IP block that did it, by id (`attack_mode` for attack mode). Empty when nothing matched.
-- Requests the firewall answered have outcome `firewall`.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS firewall LowCardinality(String),
    ADD COLUMN IF NOT EXISTS firewall_rule LowCardinality(String);

-- Added 2026-11-14: the TLS client's JA4 fingerprint (crates/router/src/ja4.rs); empty over plain
-- HTTP. A few thousand distinct values a day: plain String.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS ja4 String CODEC(ZSTD(1));

-- Added 2026-11-16: what skew protection made of a request that named one of its project's
-- deployments (crates/router/src/skew.rs): pinned (that deployment served it, and `deployment` is
-- it), current, document, cross_site, environment, threshold, expired or unknown. Empty when the
-- project has skew protection off or the request named no deployment.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS skew LowCardinality(String);

-- Added 2026-11-16 (C16 F6, not applied yet): what the deployment's Routing Middleware said
-- (crates/router/src/proxy/middleware.rs): next, rewrite, external, redirect, answer, or failed,
-- timeout, unavailable, too_large; and how long routing spent on it, in ms (its call, and reading
-- the request's body for it). Empty for requests no middleware route matched.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS middleware LowCardinality(String),
    ADD COLUMN IF NOT EXISTS middleware_ms Nullable(UInt32);

-- Added 2026-11-16 (C16 F13, not applied yet): what the router's response cache did
-- (crates/router/src/cache.rs), `x-vercel-cache`'s value: HIT, STALE, MISS, PRERENDER,
-- REVALIDATED or BYPASS on a client's request; `background` on the line of a page or answer the
-- router made again itself, `peer` on one home made for another router. A function's answer it
-- didn't keep says MISS; empty where the cache had no say (static files, images, redirects).
-- Answers it served from what it kept have outcome `cache`.
ALTER TABLE forkrail.requests
    ADD COLUMN IF NOT EXISTS cache LowCardinality(String);

-- One row per Lambda invocation (a worker), with what the router and the worker knew of it.
CREATE TABLE IF NOT EXISTS forkrail.invocations
(
    time DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)),
    worker String CODEC(ZSTD(1)),
    project LowCardinality(String),
    deployment LowCardinality(String),
    region LowCardinality(String),
    router_region LowCardinality(String),
    router LowCardinality(String),
    memory_mb UInt16,
    duration_ms UInt32,                           -- invoke to return: what Lambda bills, within ~2%
    result LowCardinality(String),                -- ok, failed, throttled
    hedge Bool,                                   -- a second invoke beside a cold start
    connect_ms Nullable(UInt32),                  -- invoke to tunnel up: ~20 ms quick start, 1-2 s cold
    retired LowCardinality(String),               -- idle, max_age, closing; empty if it ended itself
    idle_ms UInt32,
    died Bool,                                    -- its tunnel failed with requests in flight
    env String CODEC(ZSTD(1)),                    -- the execution environment, across invocations
    env_invocations UInt32,
    requests UInt32,
    app_start_ms UInt32,                          -- the app started in this invocation
    active_ms UInt32,
    end_reason LowCardinality(String),
    rss_peak_kb UInt32,
    error String CODEC(ZSTD(3)),
    cold Bool MATERIALIZED env_invocations = 1    -- a new environment: Lambda bills its init too
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (project, time)
SETTINGS non_replicated_deduplication_window = 1000;

ALTER TABLE forkrail.invocations
    ADD COLUMN IF NOT EXISTS max_duration_s UInt32,
    ADD COLUMN IF NOT EXISTS environment LowCardinality(String),
    ADD COLUMN IF NOT EXISTS runtime LowCardinality(String),
    ADD COLUMN IF NOT EXISTS entrypoint LowCardinality(String);

-- What crossed between the router and the worker, as the router's kernel counted it on the
-- tunnel's socket when it ended (its egress connections' too): TLS records and all inside them
-- (requests, responses, headers, the app's log lines and call reports), without TCP and IP
-- headers. `in` is what the router received. AWS bills each direction on the router's side.
-- Zero for rows from before, and for a worker that never connected.
ALTER TABLE forkrail.invocations
    ADD COLUMN IF NOT EXISTS tunnel_bytes_in UInt64,
    ADD COLUMN IF NOT EXISTS tunnel_bytes_out UInt64;

-- One row per line of an app's stdout or stderr.
CREATE TABLE IF NOT EXISTS forkrail.app_logs
(
    time DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)),     -- when the runtime read it
    received DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)), -- when the router wrote it
    project LowCardinality(String),
    deployment LowCardinality(String),
    worker String CODEC(ZSTD(1)),
    request String CODEC(ZSTD(1)),                -- the request it was written for, where known
    region LowCardinality(String),
    router_region LowCardinality(String),
    router LowCardinality(String),
    stream LowCardinality(String),
    line String CODEC(ZSTD(3)),
    level LowCardinality(String) MATERIALIZED multiIf(
        match(line, '(?i)\\b(error|exception|traceback|fatal|critical)\\b'), 'error',
        match(line, '(?i)\\bwarn(ing)?\\b'), 'warning',
        'info'),
    INDEX request_idx request TYPE bloom_filter(0.001) GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (project, time)
SETTINGS non_replicated_deduplication_window = 1000;

-- One row per outgoing HTTP call an app made (Vercel's "Outgoing Requests" and "External APIs"),
-- as the shims time them: curl_cffi, requests, httpx, aiohttp and urllib in Python; fetch and
-- node:http in Node. The router masks the URL's secrets before it logs it.
CREATE TABLE IF NOT EXISTS forkrail.outgoing
(
    time DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)),     -- when the call began
    received DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)), -- when the router wrote it
    project LowCardinality(String),
    deployment LowCardinality(String),
    worker String CODEC(ZSTD(1)),
    request String CODEC(ZSTD(1)),                -- the request it was made for; empty at start-up
    region LowCardinality(String),
    router_region LowCardinality(String),
    router LowCardinality(String),
    method LowCardinality(String),
    host LowCardinality(String),                  -- with its port, if the URL named one
    path String CODEC(ZSTD(3)),
    query String CODEC(ZSTD(3)),
    status UInt16,                                -- 0: no answer (see error)
    duration_ms Float32,                          -- to the end of the body (to the headers for aiohttp and node:http)
    bytes Nullable(UInt64),
    error String CODEC(ZSTD(3)),
    INDEX request_idx request TYPE bloom_filter(0.001) GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (project, time)
SETTINGS non_replicated_deduplication_window = 1000;

-- Everything else the routers log: warnings, certificates, pools, learned costs. The fields of
-- each event are in `fields`, as JSON.
CREATE TABLE IF NOT EXISTS forkrail.router_events
(
    time DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)),
    level LowCardinality(String),
    target LowCardinality(String),
    message LowCardinality(String),
    router_region LowCardinality(String),
    router LowCardinality(String),
    deployment LowCardinality(String),
    worker String CODEC(ZSTD(1)),
    request String CODEC(ZSTD(1)),
    fields String CODEC(ZSTD(3))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (time)
SETTINGS non_replicated_deduplication_window = 1000;

-- Each router instance's host every 15 seconds, from Vector's host_metrics: CPU (cumulative
-- seconds by mode), memory, load, the root filesystem, disk and network I/O (cumulative), TCP
-- connections by state, and CPU and memory of the forkrail services (router, Vector) by cgroup.
-- For the infrastructure pages: status, usage over time, and when a router needs a bigger
-- instance. Counters are cumulative: take differences between samples for rates.
CREATE TABLE IF NOT EXISTS forkrail.host_metrics
(
    time DateTime64(3, 'UTC') CODEC(Delta, ZSTD(1)),
    router_region LowCardinality(String),
    router LowCardinality(String),
    name LowCardinality(String),                  -- cpu_seconds_total, memory_available_bytes, ...
    tags Map(LowCardinality(String), String),     -- cpu, mode, device, mountpoint, cgroup, state ...
    kind LowCardinality(String),                  -- absolute or incremental
    value Float64 CODEC(Gorilla, ZSTD(1))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (router, name, time)
SETTINGS non_replicated_deduplication_window = 1000;

-- Requests per minute, for charts over any range: counts, bytes and latency by deployment, host,
-- region and status. A minute with traffic is a handful of rows per project.
CREATE TABLE IF NOT EXISTS forkrail.requests_1m
(
    minute DateTime('UTC'),
    project LowCardinality(String),
    deployment LowCardinality(String),
    host LowCardinality(String),
    entry LowCardinality(String),
    region LowCardinality(String),
    router_region LowCardinality(String),
    status UInt16,
    requests SimpleAggregateFunction(sum, UInt64),
    bytes_in SimpleAggregateFunction(sum, UInt64),
    bytes_out SimpleAggregateFunction(sum, UInt64),
    total_ms SimpleAggregateFunction(sum, UInt64),
    queue_ms SimpleAggregateFunction(sum, UInt64),
    retried SimpleAggregateFunction(sum, UInt64),
    waited_cold SimpleAggregateFunction(sum, UInt64),     -- waited 300 ms or more for a worker
    client_gone SimpleAggregateFunction(sum, UInt64),
    duration AggregateFunction(quantilesTDigest(0.5, 0.75, 0.95, 0.99), UInt32)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(minute)
ORDER BY (project, minute, deployment, host, entry, region, router_region, status);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.requests_1m_mv TO forkrail.requests_1m AS
-- Source columns are qualified (r.), since the aliases reuse their names.
SELECT
    toStartOfMinute(r.time) AS minute,
    r.project AS project, r.deployment AS deployment, r.host AS host, r.entry AS entry,
    r.region AS region, r.router_region AS router_region, r.status AS status,
    count() AS requests,
    sum(ifNull(r.bytes_in, 0)) AS bytes_in,
    sum(r.bytes_out) AS bytes_out,
    sum(r.total_ms) AS total_ms,
    sum(r.queue_ms) AS queue_ms,
    countIf(r.attempts > 1) AS retried,
    countIf(r.queue_ms >= 300) AS waited_cold,
    countIf(r.outcome = 'client_gone') AS client_gone,
    quantilesTDigestState(0.5, 0.75, 0.95, 0.99)(r.total_ms) AS duration
FROM forkrail.requests AS r
GROUP BY minute, project, deployment, host, entry, region, router_region, status;

-- Requests per hour by path, for the busiest and slowest paths over long ranges.
CREATE TABLE IF NOT EXISTS forkrail.requests_paths_1h
(
    hour DateTime('UTC'),
    project LowCardinality(String),
    method LowCardinality(String),
    path String,
    status UInt16,
    requests SimpleAggregateFunction(sum, UInt64),
    bytes_out SimpleAggregateFunction(sum, UInt64),
    total_ms SimpleAggregateFunction(sum, UInt64),
    cpu_us SimpleAggregateFunction(sum, UInt64),          -- over the requests that measured it
    cpu_measured SimpleAggregateFunction(sum, UInt64),
    duration AggregateFunction(quantilesTDigest(0.5, 0.75, 0.95, 0.99), UInt32)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (project, hour, path, method, status);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.requests_paths_1h_mv TO forkrail.requests_paths_1h AS
SELECT
    toStartOfHour(r.time) AS hour,
    r.project AS project, r.method AS method, r.path AS path, r.status AS status,
    count() AS requests,
    sum(r.bytes_out) AS bytes_out,
    sum(r.total_ms) AS total_ms,
    sum(ifNull(r.cpu_us, 0)) AS cpu_us,
    countIf(r.cpu_us IS NOT NULL) AS cpu_measured,
    quantilesTDigestState(0.5, 0.75, 0.95, 0.99)(r.total_ms) AS duration
FROM forkrail.requests AS r
GROUP BY hour, project, method, path, status;

-- Public requests per hour by country and network (autonomous system), for "where do requests
-- come from" over long ranges. A project's hour has a row per (country, network) seen.
CREATE TABLE IF NOT EXISTS forkrail.requests_geo_1h
(
    hour DateTime('UTC'),
    project LowCardinality(String),
    country LowCardinality(String),
    asn UInt32,
    as_org LowCardinality(String),
    router_region LowCardinality(String),
    requests SimpleAggregateFunction(sum, UInt64),
    errors SimpleAggregateFunction(sum, UInt64),          -- 5xx
    bytes_out SimpleAggregateFunction(sum, UInt64),
    duration AggregateFunction(quantilesTDigest(0.5, 0.75, 0.95, 0.99), UInt32)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (project, hour, country, asn, as_org, router_region);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.requests_geo_1h_mv TO forkrail.requests_geo_1h AS
SELECT
    toStartOfHour(r.time) AS hour,
    r.project AS project, r.country AS country, r.asn AS asn, r.as_org AS as_org, r.router_region AS router_region,
    count() AS requests,
    countIf(r.status >= 500) AS errors,
    sum(r.bytes_out) AS bytes_out,
    quantilesTDigestState(0.5, 0.75, 0.95, 0.99)(r.total_ms) AS duration
FROM forkrail.requests AS r
WHERE r.entry = 'https'
GROUP BY hour, project, country, asn, as_org, router_region;

-- Requests the firewall acted on, per hour, by action and rule: the firewall's traffic over long
-- ranges. Only rows with an action; one per (project, action, rule, country) an hour.
CREATE TABLE IF NOT EXISTS forkrail.firewall_1h
(
    hour DateTime('UTC'),
    project LowCardinality(String),
    action LowCardinality(String),
    rule LowCardinality(String),
    country LowCardinality(String),
    requests SimpleAggregateFunction(sum, UInt64),
    clients AggregateFunction(uniq, String)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (project, hour, action, rule, country);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.firewall_1h_mv TO forkrail.firewall_1h AS
SELECT
    toStartOfHour(r.time) AS hour,
    r.project AS project, r.firewall AS action, r.firewall_rule AS rule, r.country AS country,
    count() AS requests,
    uniqState(r.client) AS clients
FROM forkrail.requests AS r
WHERE r.firewall != ''
GROUP BY hour, project, action, rule, country;

-- Invocations per hour: billed time and cold starts, by deployment, region and memory size.
CREATE TABLE IF NOT EXISTS forkrail.invocations_1h
(
    hour DateTime('UTC'),
    project LowCardinality(String),
    deployment LowCardinality(String),
    region LowCardinality(String),
    memory_mb UInt16,
    invocations SimpleAggregateFunction(sum, UInt64),
    cold SimpleAggregateFunction(sum, UInt64),
    hedges SimpleAggregateFunction(sum, UInt64),
    failed SimpleAggregateFunction(sum, UInt64),
    throttled SimpleAggregateFunction(sum, UInt64),
    died SimpleAggregateFunction(sum, UInt64),
    served_nothing SimpleAggregateFunction(sum, UInt64),
    duration_ms SimpleAggregateFunction(sum, UInt64),
    app_start_ms SimpleAggregateFunction(sum, UInt64),
    requests SimpleAggregateFunction(sum, UInt64),
    connect_ms AggregateFunction(quantilesTDigest(0.5, 0.95), UInt32)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (project, hour, deployment, region, memory_mb);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.invocations_1h_mv TO forkrail.invocations_1h AS
SELECT
    toStartOfHour(i.time) AS hour,
    i.project AS project, i.deployment AS deployment, i.region AS region, i.memory_mb AS memory_mb,
    count() AS invocations,
    countIf(i.env_invocations = 1) AS cold,
    countIf(i.hedge) AS hedges,
    countIf(i.result = 'failed') AS failed,
    countIf(i.result = 'throttled') AS throttled,
    countIf(i.died) AS died,
    countIf(i.result = 'ok' AND i.requests = 0) AS served_nothing,
    sum(i.duration_ms) AS duration_ms,
    sum(i.app_start_ms) AS app_start_ms,
    sum(i.requests) AS requests,
    quantilesTDigestState(0.5, 0.95)(ifNull(i.connect_ms, 0)) AS connect_ms
FROM forkrail.invocations AS i
GROUP BY hour, project, deployment, region, memory_mb;

-- The tunnel's bytes (see `invocations`), counted in the hour the invocation ended. The view is
-- changed in place, so no row is missed; hours from before stay at zero.
ALTER TABLE forkrail.invocations_1h
    ADD COLUMN IF NOT EXISTS tunnel_bytes_in SimpleAggregateFunction(sum, UInt64),
    ADD COLUMN IF NOT EXISTS tunnel_bytes_out SimpleAggregateFunction(sum, UInt64);

ALTER TABLE forkrail.invocations_1h_mv MODIFY QUERY
SELECT
    toStartOfHour(i.time) AS hour,
    i.project AS project, i.deployment AS deployment, i.region AS region, i.memory_mb AS memory_mb,
    count() AS invocations,
    countIf(i.env_invocations = 1) AS cold,
    countIf(i.hedge) AS hedges,
    countIf(i.result = 'failed') AS failed,
    countIf(i.result = 'throttled') AS throttled,
    countIf(i.died) AS died,
    countIf(i.result = 'ok' AND i.requests = 0) AS served_nothing,
    sum(i.duration_ms) AS duration_ms,
    sum(i.app_start_ms) AS app_start_ms,
    sum(i.requests) AS requests,
    quantilesTDigestState(0.5, 0.95)(ifNull(i.connect_ms, 0)) AS connect_ms,
    sum(i.tunnel_bytes_in) AS tunnel_bytes_in,
    sum(i.tunnel_bytes_out) AS tunnel_bytes_out
FROM forkrail.invocations AS i
GROUP BY hour, project, deployment, region, memory_mb;

-- Outgoing calls per hour by host and status, for Vercel's "External APIs": calls, failures and
-- latency per upstream.
CREATE TABLE IF NOT EXISTS forkrail.outgoing_1h
(
    hour DateTime('UTC'),
    project LowCardinality(String),
    deployment LowCardinality(String),
    host LowCardinality(String),
    status UInt16,
    calls SimpleAggregateFunction(sum, UInt64),
    failed SimpleAggregateFunction(sum, UInt64),      -- no answer: timeouts, refused, proxy errors
    duration_ms SimpleAggregateFunction(sum, Float64),
    bytes SimpleAggregateFunction(sum, UInt64),
    duration AggregateFunction(quantilesTDigest(0.5, 0.75, 0.95, 0.99), Float32)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (project, hour, host, deployment, status);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.outgoing_1h_mv TO forkrail.outgoing_1h AS
SELECT
    toStartOfHour(o.time) AS hour,
    o.project AS project, o.deployment AS deployment, o.host AS host, o.status AS status,
    count() AS calls,
    countIf(o.status = 0) AS failed,
    sum(o.duration_ms) AS duration_ms,
    sum(ifNull(o.bytes, 0)) AS bytes,
    quantilesTDigestState(0.5, 0.75, 0.95, 0.99)(o.duration_ms) AS duration
FROM forkrail.outgoing AS o
GROUP BY hour, project, deployment, host, status;

-- App log lines per minute by level, for the logs page's histogram and level filters.
CREATE TABLE IF NOT EXISTS forkrail.app_logs_1m
(
    minute DateTime('UTC'),
    project LowCardinality(String),
    deployment LowCardinality(String),
    level LowCardinality(String),
    stream LowCardinality(String),
    lines SimpleAggregateFunction(sum, UInt64),
    bytes SimpleAggregateFunction(sum, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(minute)
ORDER BY (project, minute, deployment, level, stream);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.app_logs_1m_mv TO forkrail.app_logs_1m AS
SELECT
    toStartOfMinute(a.time) AS minute,
    a.project AS project, a.deployment AS deployment, a.level AS level, a.stream AS stream,
    count() AS lines,
    sum(length(a.line)) AS bytes
FROM forkrail.app_logs AS a
GROUP BY minute, project, deployment, level, stream;

-- The routing table's deployments and custom domains (state/routes.json), loaded by
-- `analytics.sh meta`: names projects for rows that came without one (the backfill), and memory
-- sizes for usage. Pruned deployments drop out of routes.json but stay here.
CREATE TABLE IF NOT EXISTS forkrail.meta_deployments
(
    deployment String,
    project LowCardinality(String),
    region LowCardinality(String),
    memory_mb UInt16,
    loaded DateTime('UTC') DEFAULT now()
)
ENGINE = ReplacingMergeTree(loaded)
ORDER BY deployment;

CREATE TABLE IF NOT EXISTS forkrail.meta_domains
(
    domain String,
    project LowCardinality(String),
    loaded DateTime('UTC') DEFAULT now()
)
ENGINE = ReplacingMergeTree(loaded)
ORDER BY domain;

-- When Vector took over on each router: the backfill loads only what came before.
CREATE TABLE IF NOT EXISTS forkrail.backfill_cutover
(
    router String,
    cutover DateTime64(6, 'UTC')
)
ENGINE = ReplacingMergeTree
ORDER BY router;

-- One row per line of a deployment's build log (crates/cli/src/report.rs): forkrail's own lines
-- (`forkrail`), the project's commands' output (`stdout`, `stderr`), and where each step starts
-- (`step`). The dashboard's builds endpoint writes them as the build runs, as `buildlog`, with env
-- var values already hidden; a line sent twice (a request retried) has the same seq, so readers
-- take one of each (`LIMIT 1 BY seq`). Small (Vercel's 4 MB a build at most), kept for good, as
-- Vercel keeps build logs; a removed deployment's lines stay, unread.
CREATE TABLE IF NOT EXISTS forkrail.build_logs
(
    deployment String,
    project LowCardinality(String),
    seq UInt32,
    time DateTime64(3, 'UTC') CODEC(Delta, ZSTD(1)),
    stream LowCardinality(String),
    text String CODEC(ZSTD(3)),
    received DateTime64(3, 'UTC') DEFAULT now64(3)
)
ENGINE = ReplacingMergeTree
ORDER BY (deployment, seq);

-- The spend ledger (C15, crates/router/src/spend.rs): what each project cost, as the routers count
-- it while it's spent, at AWS's list price (forkrail_common::prices). Every router sends what it
-- counted in each 10 s, one row per project, deployment, region, meter and visitor zone: `quantity`
-- in the meter's unit (MB-ms, requests, bytes on the wire), `cost_n` in nano-dollars. A batch sent
-- twice (its first answer lost) is the same bytes, which deduplication drops; rows carry their
-- second, so two batches never look alike otherwise. Kept 35 days; usage_1h keeps the sums.
CREATE TABLE IF NOT EXISTS forkrail.usage
(
    time DateTime('UTC') CODEC(Delta, ZSTD(1)),
    project LowCardinality(String),               -- empty: the install's own
    deployment LowCardinality(String),
    region LowCardinality(String),                -- the function's for its time, the router's for bytes
    router_region LowCardinality(String),
    router LowCardinality(String),
    meter LowCardinality(String),                 -- fn_mb_ms, fn_requests, egress_bytes, ...
    zone LowCardinality(String),                  -- the visitor's (Global Accelerator's price zones); empty if none
    quantity UInt64,
    cost_n Int64                                  -- may be negative: ingress's premium given back as egress caught up within the hour
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (project, time)
TTL time + INTERVAL 35 DAY
SETTINGS non_replicated_deduplication_window = 1000;

-- Spend by hour: what budgets, the usage page and month-to-date read. A month's spend is
-- sum(cost_n) of its hours, unmerged parts included.
CREATE TABLE IF NOT EXISTS forkrail.usage_1h
(
    hour DateTime('UTC'),
    project LowCardinality(String),
    deployment LowCardinality(String),
    region LowCardinality(String),
    meter LowCardinality(String),
    zone LowCardinality(String),
    quantity UInt64,
    cost_n Int64
)
ENGINE = SummingMergeTree((quantity, cost_n))
PARTITION BY toYYYYMM(hour)
ORDER BY (project, hour, meter, zone, region, deployment);

CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.usage_1h_mv TO forkrail.usage_1h AS
SELECT
    toStartOfHour(time) AS hour,
    project,
    deployment,
    region,
    meter,
    zone,
    sum(quantity) AS quantity,
    sum(cost_n) AS cost_n
FROM forkrail.usage
GROUP BY hour, project, deployment, region, meter, zone;

-- Added 2026-11-22: Web Analytics (crates/router/src/insights.rs): one row per page view or custom
-- event that a page (or a function's server `track`) sent, for projects that have it on. Nothing
-- names a visitor: `visitor` is a number made from their address and agent under a salt that
-- changes every UTC day, so it's the same visitor all day and another the next. The analytics
-- page reads this table itself, always with `project` and a time range (the sorting key), as a
-- logs page reads `requests`. Kept 13 months: a month beside the same month a year before.
CREATE TABLE IF NOT EXISTS forkrail.web_events
(
    time DateTime64(3, 'UTC') CODEC(Delta, ZSTD(1)),
    kind LowCardinality(String),                  -- view, event
    project LowCardinality(String),
    deployment LowCardinality(String),
    environment LowCardinality(String),           -- production, preview
    host LowCardinality(String),                  -- the page's
    path String CODEC(ZSTD(1)),                   -- decoded, without the query
    route LowCardinality(String),                 -- the framework's, /blog/[slug]; empty if none was sent
    referrer LowCardinality(String),              -- another site's host, without www.; empty for none
    utm_source LowCardinality(String),
    utm_medium LowCardinality(String),
    utm_campaign LowCardinality(String),
    utm_term String CODEC(ZSTD(1)),
    utm_content String CODEC(ZSTD(1)),
    country LowCardinality(String),               -- ISO 3166-1 alpha-2; empty when unknown
    device LowCardinality(String),                -- Desktop, Mobile, Tablet
    browser LowCardinality(String),
    os LowCardinality(String),
    visitor UInt64,
    event LowCardinality(String),                 -- a custom event's name; empty for a view
    properties Map(LowCardinality(String), String) CODEC(ZSTD(1)),
    flags Map(LowCardinality(String), String) CODEC(ZSTD(1)),
    sdk LowCardinality(String),                   -- @vercel/analytics/next, ...
    -- What the beacon itself was, for usage (requests_1m below): the host it was sent to, and
    -- what answering it took.
    request_host LowCardinality(String),
    entry LowCardinality(String),
    region LowCardinality(String),
    router_region LowCardinality(String),
    router LowCardinality(String),
    status UInt16,
    bytes_in UInt64 CODEC(T64, ZSTD(1)),
    bytes_out UInt64 CODEC(T64, ZSTD(1)),
    total_ms UInt32 CODEC(T64, ZSTD(1))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(time)
ORDER BY (project, time)
TTL toDateTime(time) + INTERVAL 13 MONTH
SETTINGS non_replicated_deduplication_window = 1000, ttl_only_drop_parts = 1;

-- A beacon is written as its web_events row instead of a requests row, so its request counts here
-- for usage as a requests row would (requests_1m_mv).
CREATE MATERIALIZED VIEW IF NOT EXISTS forkrail.requests_1m_web_mv TO forkrail.requests_1m AS
SELECT
    toStartOfMinute(w.time) AS minute,
    w.project AS project, w.deployment AS deployment, w.request_host AS host, w.entry AS entry,
    w.region AS region, w.router_region AS router_region, w.status AS status,
    count() AS requests,
    sum(w.bytes_in) AS bytes_in,
    sum(w.bytes_out) AS bytes_out,
    sum(w.total_ms) AS total_ms,
    toUInt64(0) AS queue_ms,
    toUInt64(0) AS retried,
    toUInt64(0) AS waited_cold,
    toUInt64(0) AS client_gone,
    quantilesTDigestState(0.5, 0.75, 0.95, 0.99)(w.total_ms) AS duration
FROM forkrail.web_events AS w
GROUP BY minute, project, deployment, host, entry, region, router_region, status;
