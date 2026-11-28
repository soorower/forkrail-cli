//! The installer's own record, `state/install.json` in the install's bucket: what was asked for
//! (region, domain), and each step that has finished with what it made. Running the installer
//! again reads it, skips the finished steps and does the rest, so an account never gets a second
//! install. It holds no secrets: the Cloudflare token is in SSM, like the install's other secrets.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const RECORD_KEY: &str = "state/install.json";
/// The routing table: an install made by hand, before the installer, has one and no record.
pub const ROUTES_KEY: &str = forkrail_common::install::ROUTES_KEY;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The forkrail version that last ran the installer here.
    pub forkrail: String,
    pub account: String,
    pub home_region: String,
    /// The base domain: projects are `<project>.<domain>`, the dashboard `dashboard.<domain>`.
    pub domain: String,
    /// The Cloudflare zone holding it.
    pub zone_id: String,
    pub zone_name: String,
    /// Where alerts are mailed, if anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    /// The most memory (MB) the account lets a function have, when it's less than Lambda's usual
    /// 10,240: new accounts start low. The install's functions are made to fit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_cap_mb: Option<u32>,
    /// Finished steps, by id.
    #[serde(default)]
    pub steps: BTreeMap<String, Done>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    pub at_ms: u64,
    /// What the step made that later steps (or uninstall) need: a stack's outputs, an address.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
}

impl Record {
    pub fn done(&self, step: &str) -> bool {
        self.steps.contains_key(step)
    }

    /// A value a finished step left, as `step.key`.
    pub fn output(&self, step: &str, key: &str) -> Option<&str> {
        self.steps.get(step)?.outputs.get(key).map(String::as_str)
    }

    pub fn finish(&mut self, step: &str, at_ms: u64, outputs: BTreeMap<String, String>) {
        self.steps.insert(step.to_owned(), Done { at_ms, outputs });
    }

    pub fn dashboard_url(&self) -> String {
        format!("https://dashboard.{}", self.domain)
    }
}

/// What the account holds, before anything is changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// No forkrail here: a new install.
    Nothing,
    /// An install the installer started or finished.
    Ours(Box<Record>),
    /// forkrail runs here, set up without the installer: never touched.
    ByHand,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_answers_outputs() {
        let mut record = Record { account: "1".into(), home_region: "us-east-2".into(), domain: "example.com".into(), ..Default::default() };
        record.finish("bucket", 5, BTreeMap::from([("name".to_owned(), "forkrail-artifacts-1".to_owned())]));
        let text = serde_json::to_string(&record).unwrap();
        let back: Record = serde_json::from_str(&text).unwrap();
        assert!(back.done("bucket") && !back.done("router"));
        assert_eq!(back.output("bucket", "name"), Some("forkrail-artifacts-1"));
        assert_eq!(back.dashboard_url(), "https://dashboard.example.com");
        assert!(!text.contains("email"));
    }
}
