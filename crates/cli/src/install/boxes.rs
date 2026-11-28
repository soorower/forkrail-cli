//! The install's two machines, the router and the analytics box: where they go in the region's
//! default VPC, and setting them up through SSM Run Command with the scripts in
//! deploy/install/box/, whose output comes back here, so a failure on a machine says what failed.

use std::time::{Duration, Instant};

use aws_config::SdkConfig;
use aws_sdk_ec2::types::{Filter, LocationType};

use super::problem::aws_text;

/// The machines' sizes (2 vCPUs each).
pub const ROUTER_TYPE: &str = "t4g.small";
pub const ANALYTICS_TYPE: &str = "t4g.medium";
/// Amazon Linux 2023 on arm64, newest, as AWS publishes it in SSM.
pub const IMAGE_PARAM: &str = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64";
/// The ClickHouse release the box runs (deploy/aws/analytics.sh's CLICKHOUSE_VERSION).
pub const CLICKHOUSE_VERSION: &str = "26.8.9.10";

/// The scripts and files the machines' setup uses, by the name they get on the machine.
pub const ANALYTICS_FILES: &[(&str, &str)] = &[
    ("analytics.sh", include_str!("../../../../deploy/install/box/analytics.sh")),
    ("config.xml", include_str!("../../../../deploy/aws/clickhouse/config.xml")),
    ("users.xml", include_str!("../../../../deploy/aws/clickhouse/users.xml")),
    ("schema.sql", include_str!("../../../../deploy/aws/clickhouse/schema.sql")),
];
pub const ROUTER_FILES: &[(&str, &str)] = &[
    ("router.sh", include_str!("../../../../deploy/install/box/router.sh")),
    ("geoip.sh", include_str!("../../../../deploy/install/box/geoip.sh")),
    ("tcp-tuning.sh", include_str!("../../../../deploy/aws/tcp-tuning.sh")),
    ("log-export.sh", include_str!("../../../../deploy/aws/log-export.sh")),
    ("log-export.py", include_str!("../../../../deploy/aws/log-export.py")),
    ("vector.yaml", include_str!("../../../../deploy/aws/vector/vector.yaml")),
];

/// Where the machines go: the default VPC, and a zone that offers both sizes, with its default
/// subnet. Both machines share the zone, so the router's logs reach the box for nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub vpc: String,
    pub zone: String,
    pub subnet: String,
}

pub async fn placement(config: &SdkConfig) -> Result<Placement, String> {
    let ec2 = aws_sdk_ec2::Client::new(config);
    let vpcs = ec2.describe_vpcs().filters(Filter::builder().name("is-default").values("true").build()).send().await.map_err(|e| aws_text(&e))?;
    let vpc = vpcs.vpcs().first().and_then(|v| v.vpc_id()).map(str::to_owned).ok_or_else(|| "NoDefaultVpc: the region has no default VPC".to_owned())?;
    let mut zones: Option<Vec<String>> = None;
    for kind in [ROUTER_TYPE, ANALYTICS_TYPE] {
        let offered = ec2
            .describe_instance_type_offerings()
            .location_type(LocationType::AvailabilityZone)
            .filters(Filter::builder().name("instance-type").values(kind).build())
            .send()
            .await
            .map_err(|e| aws_text(&e))?;
        let here: Vec<String> = offered.instance_type_offerings().iter().filter_map(|o| o.location().map(str::to_owned)).collect();
        zones = Some(match zones {
            None => here,
            Some(before) => before.into_iter().filter(|z| here.contains(z)).collect(),
        });
    }
    let mut zones = zones.unwrap_or_default();
    zones.sort();
    let subnets = ec2
        .describe_subnets()
        .filters(Filter::builder().name("vpc-id").values(&vpc).build())
        .filters(Filter::builder().name("default-for-az").values("true").build())
        .send()
        .await
        .map_err(|e| aws_text(&e))?;
    for zone in &zones {
        if let Some(subnet) = subnets.subnets().iter().find(|s| s.availability_zone() == Some(zone.as_str())).and_then(|s| s.subnet_id()) {
            return Ok(Placement { vpc, zone: zone.clone(), subnet: subnet.to_owned() });
        }
    }
    Err(format!("NoSubnet: no zone of the default VPC {vpc} has a default subnet and offers both {ROUTER_TYPE} and {ANALYTICS_TYPE}"))
}

/// The newest Amazon Linux 2023 image for arm64 in the region.
pub async fn image(config: &SdkConfig) -> Result<String, String> {
    let ssm = aws_sdk_ssm::Client::new(config);
    let out = ssm.get_parameter().name(IMAGE_PARAM).send().await.map_err(|e| aws_text(&e))?;
    out.parameter().and_then(|p| p.value()).map(str::to_owned).ok_or_else(|| format!("SSM has no {IMAGE_PARAM}"))
}

/// Puts `files` in the bucket under `tools/install/<dir>/`, where the machine's role may read.
pub async fn upload(s3: &aws_sdk_s3::Client, bucket: &str, dir: &str, files: &[(&str, String)]) -> Result<(), String> {
    for (name, body) in files {
        s3.put_object()
            .bucket(bucket)
            .key(format!("tools/install/{dir}/{name}"))
            .body(body.clone().into_bytes().into())
            .send()
            .await
            .map_err(|e| format!("uploading {name}: {}", aws_text(&e)))?;
    }
    Ok(())
}

/// How long a new machine may take to reach SSM (it boots, then its agent signs in).
const SSM_WAIT: Duration = Duration::from_secs(8 * 60);

/// Waits until `instance` answers SSM; an error says what it is waiting for.
pub async fn wait_for_ssm(config: &SdkConfig, instance: &str) -> Result<(), String> {
    let ssm = aws_sdk_ssm::Client::new(config);
    let started = Instant::now();
    loop {
        let out = ssm
            .describe_instance_information()
            .filters(aws_sdk_ssm::types::InstanceInformationStringFilter::builder().key("InstanceIds").values(instance).build().expect("filter"))
            .send()
            .await;
        match out {
            Ok(out) if out.instance_information_list().iter().any(|i| i.ping_status().is_some_and(|p| p.as_str() == "Online")) => return Ok(()),
            Ok(_) => {}
            Err(e) => {
                let text = aws_text(&e);
                // A new role's first minutes: SSM may not see the machine's profile yet.
                if !text.contains("InvalidInstanceId") {
                    return Err(text);
                }
            }
        }
        if started.elapsed() > SSM_WAIT {
            return Err(format!(
                "SsmNotReady: {instance} didn't reach AWS Systems Manager within {} minutes (it needs outbound internet from the default VPC and its role forkrail-router or forkrail-analytics)",
                SSM_WAIT.as_secs() / 60
            ));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Runs `script` as root on `instance`, waits for it (up to `timeout`), and returns what it
/// printed. A script that fails comes back as an error holding the end of its output.
pub async fn run(config: &SdkConfig, instance: &str, script: &str, timeout: Duration) -> Result<String, String> {
    let ssm = aws_sdk_ssm::Client::new(config);
    let sent = ssm
        .send_command()
        .instance_ids(instance)
        .document_name("AWS-RunShellScript")
        .comment("forkrail install")
        .parameters("commands", vec![script.to_owned()])
        .parameters("executionTimeout", vec![timeout.as_secs().to_string()])
        .send()
        .await
        .map_err(|e| aws_text(&e))?;
    let id = sent.command().and_then(|c| c.command_id()).ok_or("SSM gave no command id")?.to_owned();
    let started = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let invocation = match ssm.get_command_invocation().command_id(&id).instance_id(instance).send().await {
            Ok(out) => out,
            // Right after sending, SSM may not know the command yet.
            Err(e) if aws_text(&e).contains("InvocationDoesNotExist") && started.elapsed() < Duration::from_secs(60) => continue,
            Err(e) => return Err(aws_text(&e)),
        };
        let status = invocation.status().map(|s| s.as_str().to_owned()).unwrap_or_default();
        match status.as_str() {
            "Pending" | "InProgress" | "Delayed" => {
                if started.elapsed() > timeout + Duration::from_secs(60) {
                    return Err(format!("the setup on {instance} took over {} minutes", timeout.as_secs() / 60));
                }
            }
            "Success" => return Ok(invocation.standard_output_content().unwrap_or_default().to_owned()),
            _ => {
                let out = invocation.standard_output_content().unwrap_or_default();
                let err = invocation.standard_error_content().unwrap_or_default();
                return Err(format!("the setup on {instance} ended {status}:\n{}", tail(&format!("{out}\n{err}"), 40)));
            }
        }
    }
}

/// The last `lines` lines of `text`, trimmed.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim().lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Shell lines for the machine that fetch `dir`'s files from the bucket into `$FILES` and run
/// `script` with `env`, each value quoted.
pub fn command(bucket: &str, region: &str, dir: &str, script: &str, env: &[(&str, String)]) -> String {
    let vars: Vec<String> = env.iter().map(|(k, v)| format!("{k}={}", quote(v))).collect();
    format!(
        "set -e\n\
         echo 1000 > /proc/self/oom_score_adj\n\
         export AWS_REGION={region}\n\
         FILES=$(mktemp -d)\n\
         trap 'rm -rf \"$FILES\"' EXIT\n\
         aws s3 cp --quiet --recursive s3://{bucket}/tools/install/{dir}/ \"$FILES/\"\n\
         env FILES=\"$FILES\" BUCKET={bucket} REGION={region} {vars} bash \"$FILES/{script}\"",
        vars = vars.join(" ")
    )
}

/// `value` in single quotes for the shell.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_quoted_for_the_shell() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        let c = command("b", "us-east-2", "router", "router.sh", &[("X", "1 2".into())]);
        assert!(c.contains("s3://b/tools/install/router/"));
        assert!(c.ends_with(r#"env FILES="$FILES" BUCKET=b REGION=us-east-2 X='1 2' bash "$FILES/router.sh""#), "{c}");
    }

    #[test]
    fn the_tail_is_the_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }

    #[test]
    fn the_box_scripts_are_bash() {
        for (name, body) in ANALYTICS_FILES.iter().chain(ROUTER_FILES) {
            if name.ends_with(".sh") {
                assert!(body.starts_with("#!/bin/bash") || body.starts_with("# "), "{name}");
            }
        }
    }
}
