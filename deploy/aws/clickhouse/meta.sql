-- The routing table into forkrail.meta_deployments and meta_domains. @BUCKET@ and @REGION@ are
-- filled in by analytics.sh. The box's instance role reads the bucket; ClickHouse lets a query use
-- it only when asked, and only the admin user, local to the box, runs this.
SET s3_allow_server_credentials_in_user_queries = 1;
INSERT INTO forkrail.meta_deployments (deployment, project, region, memory_mb)
SELECT kv.1, JSONExtractString(kv.2, 'project'), JSONExtractString(kv.2, 'region'), JSONExtractUInt(kv.2, 'memory_mb')
FROM s3('https://@BUCKET@.s3.@REGION@.amazonaws.com/state/routes.json', 'JSONAsString', 'json String')
ARRAY JOIN JSONExtractKeysAndValuesRaw(json, 'deployments') AS kv;

INSERT INTO forkrail.meta_domains (domain, project)
SELECT kv.1, JSONExtractString(kv.2, 'project')
FROM s3('https://@BUCKET@.s3.@REGION@.amazonaws.com/state/routes.json', 'JSONAsString', 'json String')
ARRAY JOIN JSONExtractKeysAndValuesRaw(json, 'domains') AS kv;
