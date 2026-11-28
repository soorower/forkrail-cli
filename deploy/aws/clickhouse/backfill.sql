-- One day of the S3 log export (logs/router/@DAY@/) into the tables, up to when Vector took over
-- on each router (forkrail.backfill_cutover). Run by `analytics.sh backfill`, which fills in:
--   @BUCKET@, @REGION@   the bucket and its region
--   @DAY@                YYYY/MM/DD
--   @BASE@               the base domain, for projects named by host (<project>[-<id>].<base>)
--   @INSTANCES@, @INSTANCE_REGIONS@   router instances and their regions, as arrays
--
-- The export has what the routers wrote: before 2026-11-09, text lines turned into JSON objects
-- ({"ts", "level", "target", "msg", fields...}); after, the JSON lines, the same way. Old lines
-- have no project or region: a project comes from the line, else the routing table, else the
-- host; the region is the router's. Old invocations are put together from their worker's
-- lifecycle lines (spawning, connected, retiring, ended), new ones are one `invocation` line.
--
-- The export's converter read bare numbers as numbers, so ids made only of digits arrive as
-- numbers; they're read as strings (and lose leading zeros, which is rare and left alone).

-- The box's instance role reads the bucket, which a query may use only when it asks (see meta.sql).
SET s3_allow_server_credentials_in_user_queries = 1,
    input_format_skip_unknown_fields = 1,
    input_format_json_read_numbers_as_strings = 1,
    input_format_json_read_objects_as_strings = 1,
    input_format_json_read_bools_as_numbers = 1,
    input_format_null_as_default = 1,
    input_format_allow_errors_ratio = 0.01,
    max_threads = 2,
    max_memory_usage = 2000000000;

-- Requests.
INSERT INTO forkrail.requests (time, request, project, deployment, host, entry, region, router_region, router,
    method, path, query, client, proto, agent, accept_encoding, encoding, status, outcome, attempts, queue_ms,
    ttfb_ms, total_ms, worker_ms, bytes_in, bytes_out, worker, mem_kb, cpu_us, busy_us, cpu_profile)
SELECT
    fromUnixTimestamp64Micro(toInt64(e.ts * 1000000), 'UTC') AS t,
    e.request,
    multiIf(e.project != '', e.project, d.project != '', d.project, m.project != '', m.project,
        -- <project>.<base> or <project>-<id>.<base>, for a project that exists (not www.<base>, say).
        replaceRegexpOne(substring(e.host, 1, length(e.host) - length('.@BASE@')), '-[0-9a-f]{8}$', '')
            IN (SELECT project FROM forkrail.meta_deployments) AND endsWith(e.host, '.@BASE@'),
        replaceRegexpOne(substring(e.host, 1, length(e.host) - length('.@BASE@')), '-[0-9a-f]{8}$', ''),
        ''),
    e.deployment, e.host, e.entry,
    if(e.region != '', e.region, transform(extract(e._file, 'i-[0-9a-f]+'), @INSTANCES@, @INSTANCE_REGIONS@, '')),
    transform(extract(e._file, 'i-[0-9a-f]+'), @INSTANCES@, @INSTANCE_REGIONS@, ''),
    extract(e._file, 'i-[0-9a-f]+'),
    e.method, e.path, e.query, e.client, e.proto, e.agent, e.accept_encoding, e.encoding, e.status, e.outcome,
    e.attempts, e.queue_ms, e.ttfb_ms, e.total_ms, e.worker_ms, e.bytes_in, e.bytes_out, e.worker, e.mem_kb,
    e.cpu_us, e.busy_us, e.cpu_profile
FROM s3('https://@BUCKET@.s3.@REGION@.amazonaws.com/logs/router/@DAY@/*.jsonl.gz', 'JSONEachRow',
    'ts Float64, msg String, request String, deployment String, project String, region String, host String,
     entry String, method String, path String, query String, client String, proto String, agent String,
     accept_encoding String, encoding String, status UInt16, outcome String, attempts UInt8, queue_ms UInt32,
     ttfb_ms Nullable(UInt32), total_ms UInt32, worker_ms Nullable(UInt32), bytes_in Nullable(UInt64),
     bytes_out UInt64, worker String, mem_kb Nullable(UInt32), cpu_us Nullable(UInt32),
     busy_us Nullable(UInt32), cpu_profile String') AS e
LEFT JOIN (SELECT deployment, argMax(project, loaded) AS project FROM forkrail.meta_deployments GROUP BY deployment) AS d
    ON d.deployment = e.deployment
LEFT JOIN (SELECT domain, argMax(project, loaded) AS project FROM forkrail.meta_domains GROUP BY domain) AS m
    ON m.domain = e.host
INNER JOIN (SELECT router, max(cutover) AS cutover FROM forkrail.backfill_cutover GROUP BY router) AS c
    ON c.router = extract(e._file, 'i-[0-9a-f]+')
WHERE e.msg = 'request' AND t < c.cutover;

-- App output. A deployment no longer in the routing table gets its project from its requests.
INSERT INTO forkrail.app_logs (time, received, project, deployment, worker, request, region, router_region, router,
    stream, line)
SELECT
    fromUnixTimestamp64Micro(toInt64(if(e.line_ts > 0, e.line_ts, e.ts) * 1000000), 'UTC'),
    fromUnixTimestamp64Micro(toInt64(e.ts * 1000000), 'UTC') AS received,
    multiIf(e.project != '', e.project, d.project != '', d.project, q.project),
    e.deployment, e.worker, e.request,
    if(e.region != '', e.region, transform(extract(e._file, 'i-[0-9a-f]+'), @INSTANCES@, @INSTANCE_REGIONS@, '')),
    transform(extract(e._file, 'i-[0-9a-f]+'), @INSTANCES@, @INSTANCE_REGIONS@, ''),
    extract(e._file, 'i-[0-9a-f]+'),
    e.stream, e.line
FROM s3('https://@BUCKET@.s3.@REGION@.amazonaws.com/logs/router/@DAY@/*.jsonl.gz', 'JSONEachRow',
    'ts Float64, msg String, line_ts Float64, project String, region String, deployment String, worker String,
     request String, stream String, line String') AS e
LEFT JOIN (SELECT deployment, argMax(project, loaded) AS project FROM forkrail.meta_deployments GROUP BY deployment) AS d
    ON d.deployment = e.deployment
LEFT JOIN (SELECT r.deployment AS deployment, any(r.project) AS project FROM forkrail.requests AS r
           WHERE r.project != '' AND r.deployment != '' GROUP BY r.deployment) AS q
    ON q.deployment = e.deployment
INNER JOIN (SELECT router, max(cutover) AS cutover FROM forkrail.backfill_cutover GROUP BY router) AS c
    ON c.router = extract(e._file, 'i-[0-9a-f]+')
WHERE e.msg = 'app log' AND received < c.cutover;

-- Invocations logged as one line (the router's JSON era, before Vector).
INSERT INTO forkrail.invocations (time, worker, project, deployment, region, router_region, router, memory_mb,
    duration_ms, result, hedge, connect_ms, retired, idle_ms, died, env, env_invocations, requests, app_start_ms,
    active_ms, end_reason, rss_peak_kb, error)
SELECT
    fromUnixTimestamp64Micro(toInt64(e.ts * 1000000), 'UTC') AS t,
    e.worker, e.project, e.deployment, e.region,
    transform(extract(e._file, 'i-[0-9a-f]+'), @INSTANCES@, @INSTANCE_REGIONS@, ''),
    extract(e._file, 'i-[0-9a-f]+'),
    e.memory_mb, e.duration_ms, e.result, e.hedge, e.connect_ms, e.retired, e.idle_ms, e.died, e.env,
    e.env_invocations, e.requests, e.app_start_ms, e.active_ms, e.end_reason, e.rss_peak_kb, e.error
FROM s3('https://@BUCKET@.s3.@REGION@.amazonaws.com/logs/router/@DAY@/*.jsonl.gz', 'JSONEachRow',
    'ts Float64, msg String, worker String, project String, deployment String, region String, memory_mb UInt16,
     duration_ms UInt32, result String, hedge UInt8, connect_ms Nullable(UInt32), retired String, idle_ms UInt32,
     died UInt8, env String, env_invocations UInt32, requests UInt32, app_start_ms UInt32, active_ms UInt32,
     end_reason String, rss_peak_kb UInt32, error String') AS e
INNER JOIN (SELECT router, max(cutover) AS cutover FROM forkrail.backfill_cutover GROUP BY router) AS c
    ON c.router = extract(e._file, 'i-[0-9a-f]+')
WHERE e.msg = 'invocation' AND t < c.cutover;

-- Invocations from before that, put together from their worker's lines. A worker whose life
-- crossed midnight is counted on the day it ended, without what was logged the day before.
INSERT INTO forkrail.invocations (time, worker, project, deployment, region, router_region, router, memory_mb,
    duration_ms, result, hedge, connect_ms, retired, idle_ms, died, env, env_invocations, requests, app_start_ms,
    active_ms, end_reason, rss_peak_kb, error)
SELECT
    fromUnixTimestamp64Micro(toInt64(w.end_ts * 1000000), 'UTC') AS t,
    w.worker,
    multiIf(d.project != '', d.project, q.project),
    w.deployment,
    transform(w.instance, @INSTANCES@, @INSTANCE_REGIONS@, ''),
    transform(w.instance, @INSTANCES@, @INSTANCE_REGIONS@, ''),
    w.instance,
    if(w.memory_mb > 0, w.memory_mb, d.memory_mb),
    toUInt32(round(w.secs * 1000)),
    w.result, w.hedge, w.connect_ms, w.retired, w.idle_ms, w.died,
    JSONExtractString(w.report, 'env_id'),
    JSONExtractUInt(w.report, 'env_invocations'),
    JSONExtractUInt(w.report, 'requests'),
    JSONExtractUInt(w.report, 'app_start_ms'),
    JSONExtractUInt(w.report, 'active_ms'),
    JSONExtractString(w.report, 'end_reason'),
    JSONExtractUInt(w.report, 'rss_peak_kb'),
    w.error
FROM
(
    SELECT
        e.worker AS worker,
        extract(e._file, 'i-[0-9a-f]+') AS instance,
        if(position(e.worker, '.') > 0, splitByChar('.', e.worker)[1], '') AS deployment,
        maxIf(e.ts, e.msg IN ('worker ended', 'worker failed')) AS end_ts,
        anyIf(e.secs, e.msg IN ('worker ended', 'worker failed')) AS secs,
        anyIf(e.report, e.msg = 'worker ended') AS report,
        multiIf(countIf(e.msg = 'worker ended') > 0, 'ok',
                maxIf(e.throttled, e.msg = 'worker failed') = 1, 'throttled', 'failed') AS result,
        anyIf(e.error, e.msg = 'worker failed') AS error,
        maxIf(e.hedge, e.msg = 'spawning worker') AS hedge,
        if(countIf(e.msg = 'worker connected') > 0, anyIf(e.spawn_ms, e.msg = 'worker connected'), NULL) AS connect_ms,
        anyIf(e.limit_mb, e.msg = 'worker connected') AS memory_mb,
        multiIf(countIf(e.msg = 'retiring worker') = 0, '',
                maxIf(e.age_s, e.msg = 'retiring worker') >= 780, 'max_age', 'idle') AS retired,
        maxIf(e.idle_ms, e.msg = 'retiring worker') AS idle_ms,
        countIf(e.msg = 'worker died with requests in flight') > 0 AS died,
        countIf(e.msg = 'invocation') AS logged
    FROM s3('https://@BUCKET@.s3.@REGION@.amazonaws.com/logs/router/@DAY@/*.jsonl.gz', 'JSONEachRow',
        'ts Float64, msg String, worker String, hedge UInt8, spawn_ms UInt32, limit_mb UInt16, idle_ms UInt32,
         age_s UInt32, secs Float64, report String, throttled UInt8, error String') AS e
    WHERE e.msg IN ('spawning worker', 'worker connected', 'retiring worker', 'worker ended', 'worker failed',
                    'worker died with requests in flight', 'invocation')
      AND e.worker != ''
    GROUP BY worker, instance
    HAVING end_ts > 0 AND logged = 0
) AS w
LEFT JOIN (SELECT deployment, argMax(project, loaded) AS project, argMax(memory_mb, loaded) AS memory_mb
           FROM forkrail.meta_deployments GROUP BY deployment) AS d
    ON d.deployment = w.deployment
LEFT JOIN (SELECT r.deployment AS deployment, any(r.project) AS project FROM forkrail.requests AS r
           WHERE r.project != '' AND r.deployment != '' GROUP BY r.deployment) AS q
    ON q.deployment = w.deployment
INNER JOIN (SELECT router, max(cutover) AS cutover FROM forkrail.backfill_cutover GROUP BY router) AS c
    ON c.router = w.instance
WHERE t < c.cutover;

-- Everything else, as router_events, with the whole line in `fields`.
INSERT INTO forkrail.router_events (time, level, target, message, router_region, router, deployment, worker,
    request, fields)
SELECT
    fromUnixTimestamp64Micro(toInt64(JSONExtractFloat(e.json, 'ts') * 1000000), 'UTC') AS t,
    JSONExtractString(e.json, 'level'),
    JSONExtractString(e.json, 'target'),
    JSONExtractString(e.json, 'msg') AS message,
    transform(extract(e._file, 'i-[0-9a-f]+'), @INSTANCES@, @INSTANCE_REGIONS@, ''),
    extract(e._file, 'i-[0-9a-f]+'),
    trim(BOTH '"' FROM JSONExtractRaw(e.json, 'deployment')),
    trim(BOTH '"' FROM JSONExtractRaw(e.json, 'worker')),
    trim(BOTH '"' FROM JSONExtractRaw(e.json, 'request')),
    e.json
FROM s3('https://@BUCKET@.s3.@REGION@.amazonaws.com/logs/router/@DAY@/*.jsonl.gz', 'JSONAsString', 'json String') AS e
INNER JOIN (SELECT router, max(cutover) AS cutover FROM forkrail.backfill_cutover GROUP BY router) AS c
    ON c.router = extract(e._file, 'i-[0-9a-f]+')
WHERE message NOT IN ('request', 'app log', 'invocation', 'spawning worker', 'worker connected', 'retiring worker',
                      'worker disconnected', 'worker ended')
  AND t < c.cutover;
