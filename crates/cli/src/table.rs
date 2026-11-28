//! Project files and deployment records in the install's table (deploy/install/state.yaml), one
//! item each, where `state/store.json` says they're kept there (`forkrail state mode`):
//! - `pk`: the file's bucket key without `state/` and `.json` (`projects/pyapp`)
//! - `doc`: the file's JSON, as the bucket holds it, so what reads it is unchanged
//! - `v`: a number every write adds one to: what an ETag is to the bucket, for conditional writes
//! - `kind` (`project`, `deployment`) and `at`, when it was last written (ms)
//! - a deployment's `project` and `created` too, for the index of a project's deployments
//!
//! Nothing but the control function, the dashboard and the install's administrators can read or
//! write the table: its resource policy refuses everyone else, whatever their own role allows.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_sdk_dynamodb::error::{DisplayErrorContext, SdkError};
use aws_sdk_dynamodb::types::AttributeValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use forkrail_common::install::{BY_KIND_INDEX, STATE_TABLE};

/// How long a process goes by what `state/store.json` said before reading it again: a switch
/// reaches every running control function within this.
pub const STORE_TTL: Duration = Duration::from_secs(30);
/// DynamoDB's limit on one item is 400 KB; a state file anywhere near it is a mistake.
const MAX_DOC: usize = 350 * 1024;

/// Where project files and deployment records are kept (`state/store.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreConfig {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_table")]
    pub table: String,
    /// While the table is the one read: every write is also copied to the bucket, so going back
    /// (`forkrail state mode s3`) loses nothing.
    #[serde(default = "yes")]
    pub s3_copy: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The bucket only, as before the table.
    #[default]
    S3,
    /// The bucket is read and written as before, and every write is copied to the table: how the
    /// table is filled before it's read.
    Mirror,
    /// The table is read and written; the bucket gets a copy of each write while `s3_copy`.
    Dynamodb,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::S3 => "s3",
            Mode::Mirror => "mirror",
            Mode::Dynamodb => "dynamodb",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "s3" => Ok(Mode::S3),
            "mirror" => Ok(Mode::Mirror),
            "dynamodb" => Ok(Mode::Dynamodb),
            _ => Err(format!("{s:?}: s3, mirror or dynamodb")),
        }
    }
}

fn default_table() -> String {
    STATE_TABLE.to_owned()
}

fn yes() -> bool {
    true
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig { mode: Mode::S3, table: default_table(), s3_copy: true }
    }
}

/// One item as it's read: the file's JSON and its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub doc: String,
    pub v: u64,
}

/// The items, in DynamoDB, or in memory for tests.
pub enum Items {
    Dynamo(aws_sdk_dynamodb::Client),
    Memory(std::sync::Mutex<BTreeMap<String, Stored>>),
}

/// An item in memory, as the table would hold it.
#[derive(Debug, Clone)]
pub struct Stored {
    kind: String,
    doc: String,
    v: u64,
    at: u64,
}

fn s(text: impl Into<String>) -> AttributeValue {
    AttributeValue::S(text.into())
}

fn n(number: impl ToString) -> AttributeValue {
    AttributeValue::N(number.to_string())
}

/// The attributes beside `doc` that a deployment's item carries for the index of a project's
/// deployments: its project and when it was made, from the record itself.
fn indexed(kind: &str, doc: &str) -> Vec<(&'static str, String)> {
    if kind != "deployment" {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(doc) else { return Vec::new() };
    ["project", "created"].into_iter().filter_map(|field| Some((field, value.get(field)?.as_str()?.to_owned()))).filter(|(_, v)| !v.is_empty()).collect()
}

fn check_size(pk: &str, doc: &str) -> Result<()> {
    if doc.len() > MAX_DOC {
        bail!("{pk} is {} KB, more than the table's items may be ({} KB)", doc.len() / 1024, MAX_DOC / 1024);
    }
    Ok(())
}

/// Whether a failed request was a condition that didn't hold: someone else wrote first.
fn lost<E: aws_sdk_dynamodb::error::ProvideErrorMetadata, R>(e: &SdkError<E, R>) -> bool {
    e.as_service_error().and_then(|e| e.code()) == Some("ConditionalCheckFailedException")
}

fn error<E: std::error::Error + Send + Sync + 'static, R: std::fmt::Debug>(what: &str, e: SdkError<E, R>) -> anyhow::Error {
    anyhow::anyhow!("{what}: {}", DisplayErrorContext(&e))
}

impl Items {
    pub fn memory() -> Self {
        Items::Memory(std::sync::Mutex::new(BTreeMap::new()))
    }

    /// The item, read consistently: what was last written, never older.
    pub async fn get(&self, table: &str, pk: &str) -> Result<Option<Item>> {
        match self {
            Items::Dynamo(client) => {
                let out = client
                    .get_item()
                    .table_name(table)
                    .key("pk", s(pk))
                    .consistent_read(true)
                    .projection_expression("#doc, #v")
                    .expression_attribute_names("#doc", "doc")
                    .expression_attribute_names("#v", "v")
                    .send()
                    .await
                    .map_err(|e| error(&format!("reading {pk} from {table}"), e))?;
                out.item.map(|item| item_of(pk, &item)).transpose()
            }
            Items::Memory(map) => Ok(map.lock().expect("memory table").get(pk).map(|i| Item { doc: i.doc.clone(), v: i.v })),
        }
    }

    /// Write the item if it's still at version `expect` (or doesn't exist, for `None`); false if
    /// someone else wrote it first. The new version is one more.
    pub async fn put_if(&self, table: &str, pk: &str, kind: &str, doc: &str, expect: Option<u64>) -> Result<bool> {
        check_size(pk, doc)?;
        let v = expect.map_or(1, |v| v + 1);
        let at = crate::state::now_ms();
        match self {
            Items::Dynamo(client) => {
                let mut put = client
                    .put_item()
                    .table_name(table)
                    .item("pk", s(pk))
                    .item("kind", s(kind))
                    .item("doc", s(doc))
                    .item("v", n(v))
                    .item("at", n(at));
                for (name, value) in indexed(kind, doc) {
                    put = put.item(name, s(value));
                }
                // DynamoDB refuses a name or value the expression doesn't use: each branch has its own.
                let put = match expect {
                    Some(expect) => put.condition_expression("#v = :v").expression_attribute_names("#v", "v").expression_attribute_values(":v", n(expect)),
                    None => put.condition_expression("attribute_not_exists(#pk)").expression_attribute_names("#pk", "pk"),
                };
                match put.send().await {
                    Ok(_) => Ok(true),
                    Err(e) if lost(&e) => Ok(false),
                    Err(e) => Err(error(&format!("writing {pk} to {table}"), e)),
                }
            }
            Items::Memory(map) => {
                let mut map = map.lock().expect("memory table");
                if map.get(pk).map(|i| i.v) != expect {
                    return Ok(false);
                }
                map.insert(pk.to_owned(), Stored { kind: kind.to_owned(), doc: doc.to_owned(), v, at });
                Ok(true)
            }
        }
    }

    /// Copy a file written to the bucket into the table, unless a later write is there already:
    /// what keeps the table level with the bucket while the bucket is the one read (`mirror`).
    pub async fn copy(&self, table: &str, pk: &str, kind: &str, doc: &str, at: u64) -> Result<()> {
        check_size(pk, doc)?;
        match self {
            Items::Dynamo(client) => {
                let mut set = String::from("SET #doc = :doc, #kind = :kind, #at = :at");
                let mut update = client
                    .update_item()
                    .table_name(table)
                    .key("pk", s(pk))
                    .expression_attribute_names("#pk", "pk")
                    .expression_attribute_names("#doc", "doc")
                    .expression_attribute_names("#kind", "kind")
                    .expression_attribute_names("#at", "at")
                    .expression_attribute_names("#v", "v")
                    .expression_attribute_values(":doc", s(doc))
                    .expression_attribute_values(":kind", s(kind))
                    .expression_attribute_values(":at", n(at))
                    .expression_attribute_values(":one", n(1));
                for (name, value) in indexed(kind, doc) {
                    set.push_str(&format!(", #{name} = :{name}"));
                    update = update.expression_attribute_names(format!("#{name}"), name).expression_attribute_values(format!(":{name}"), s(value));
                }
                let update = update.update_expression(format!("{set} ADD #v :one")).condition_expression("attribute_not_exists(#pk) OR #at <= :at");
                match update.send().await {
                    Ok(_) => Ok(()),
                    // A later write is in the table already.
                    Err(e) if lost(&e) => Ok(()),
                    Err(e) => Err(error(&format!("copying {pk} to {table}"), e)),
                }
            }
            Items::Memory(map) => {
                let mut map = map.lock().expect("memory table");
                let v = match map.get(pk) {
                    Some(i) if i.at > at => return Ok(()),
                    Some(i) => i.v + 1,
                    None => 1,
                };
                map.insert(pk.to_owned(), Stored { kind: kind.to_owned(), doc: doc.to_owned(), v, at });
                Ok(())
            }
        }
    }

    pub async fn delete(&self, table: &str, pk: &str) -> Result<()> {
        match self {
            Items::Dynamo(client) => {
                client.delete_item().table_name(table).key("pk", s(pk)).send().await.map_err(|e| error(&format!("deleting {pk} from {table}"), e))?;
            }
            Items::Memory(map) => {
                map.lock().expect("memory table").remove(pk);
            }
        }
        Ok(())
    }

    /// Every item of `kind`, by key, with its version. The index is eventually consistent: an item
    /// written a moment ago may not be listed yet.
    pub async fn list(&self, table: &str, kind: &str) -> Result<Vec<(String, u64)>> {
        match self {
            Items::Dynamo(client) => {
                let mut found = Vec::new();
                let mut pages = client
                    .query()
                    .table_name(table)
                    .index_name(BY_KIND_INDEX)
                    .key_condition_expression("#kind = :kind")
                    .expression_attribute_names("#kind", "kind")
                    .expression_attribute_values(":kind", s(kind))
                    .into_paginator()
                    .send();
                while let Some(page) = pages.next().await {
                    let page = page.map_err(|e| error(&format!("listing {kind} items in {table}"), e))?;
                    for item in page.items() {
                        let pk = item.get("pk").and_then(|v| v.as_s().ok()).context("an item without pk")?;
                        let v = item.get("v").and_then(|v| v.as_n().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
                        found.push((pk.clone(), v));
                    }
                }
                found.sort();
                Ok(found)
            }
            Items::Memory(map) => Ok(map.lock().expect("memory table").iter().filter(|(_, i)| i.kind == kind).map(|(pk, i)| (pk.clone(), i.v)).collect()),
        }
    }

    /// Every item's file, by key: for comparing the table with the bucket (`forkrail state check`).
    pub async fn all(&self, table: &str) -> Result<BTreeMap<String, Item>> {
        match self {
            Items::Dynamo(client) => {
                let mut found = BTreeMap::new();
                let mut pages = client
                    .scan()
                    .table_name(table)
                    .consistent_read(true)
                    .projection_expression("#pk, #doc, #v")
                    .expression_attribute_names("#pk", "pk")
                    .expression_attribute_names("#doc", "doc")
                    .expression_attribute_names("#v", "v")
                    .into_paginator()
                    .send();
                while let Some(page) = pages.next().await {
                    let page = page.map_err(|e| error(&format!("reading {table}"), e))?;
                    for item in page.items() {
                        let pk = item.get("pk").and_then(|v| v.as_s().ok()).context("an item without pk")?.clone();
                        let read = item_of(&pk, item)?;
                        found.insert(pk, read);
                    }
                }
                Ok(found)
            }
            Items::Memory(map) => Ok(map.lock().expect("memory table").iter().map(|(pk, i)| (pk.clone(), Item { doc: i.doc.clone(), v: i.v })).collect()),
        }
    }
}

fn item_of(pk: &str, item: &HashMap<String, AttributeValue>) -> Result<Item> {
    let doc = item.get("doc").and_then(|v| v.as_s().ok()).with_context(|| format!("{pk} has no doc"))?.clone();
    let v = item.get("v").and_then(|v| v.as_n().ok()).and_then(|v| v.parse().ok()).with_context(|| format!("{pk} has no version"))?;
    Ok(Item { doc, v })
}

/// The same JSON, however it's laid out: the bucket keeps files pretty-printed, the table compact.
pub fn same_json(a: &str, b: &str) -> bool {
    match (serde_json::from_str::<Value>(a), serde_json::from_str::<Value>(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_json_reads_with_defaults() {
        let cfg: StoreConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, StoreConfig::default());
        assert_eq!((cfg.mode, cfg.table.as_str(), cfg.s3_copy), (Mode::S3, "forkrail-state", true));
        let cfg: StoreConfig = serde_json::from_str(r#"{"mode":"dynamodb","s3_copy":false,"table":"forkrail-state-test"}"#).unwrap();
        assert_eq!((cfg.mode, cfg.table.as_str(), cfg.s3_copy), (Mode::Dynamodb, "forkrail-state-test", false));
        assert_eq!(serde_json::to_string(&Mode::Mirror).unwrap(), r#""mirror""#);
        assert_eq!("dynamodb".parse::<Mode>(), Ok(Mode::Dynamodb));
        assert!("ddb".parse::<Mode>().is_err());
    }

    #[test]
    fn deployments_carry_their_project_and_time() {
        let doc = r#"{"id":"a1b2c3d4","project":"pyapp","created":"2026-11-13T10:00:00Z","target":"preview"}"#;
        assert_eq!(indexed("deployment", doc), vec![("project", "pyapp".to_owned()), ("created", "2026-11-13T10:00:00Z".to_owned())]);
        assert!(indexed("project", r#"{"name":"pyapp","created":"x"}"#).is_empty());
        assert!(indexed("deployment", "not json").is_empty());
    }

    #[tokio::test]
    async fn writes_are_conditional_on_the_version() {
        let items = Items::memory();
        assert!(items.put_if("t", "projects/a", "project", r#"{"n":1}"#, None).await.unwrap());
        // Created once only.
        assert!(!items.put_if("t", "projects/a", "project", r#"{"n":2}"#, None).await.unwrap());
        let read = items.get("t", "projects/a").await.unwrap().unwrap();
        assert_eq!(read, Item { doc: r#"{"n":1}"#.to_owned(), v: 1 });
        assert!(items.put_if("t", "projects/a", "project", r#"{"n":2}"#, Some(1)).await.unwrap());
        // A writer that read version 1 lost to the one above.
        assert!(!items.put_if("t", "projects/a", "project", r#"{"n":3}"#, Some(1)).await.unwrap());
        assert_eq!(items.get("t", "projects/a").await.unwrap().unwrap().v, 2);
        items.delete("t", "projects/a").await.unwrap();
        assert_eq!(items.get("t", "projects/a").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_copy_never_replaces_a_later_write() {
        let items = Items::memory();
        items.copy("t", "deployments/x", "deployment", r#"{"s":"building"}"#, 2_000).await.unwrap();
        items.copy("t", "deployments/x", "deployment", r#"{"s":"ready"}"#, 3_000).await.unwrap();
        // A slower copy of the earlier write arrives last: it's dropped.
        items.copy("t", "deployments/x", "deployment", r#"{"s":"building"}"#, 2_500).await.unwrap();
        let read = items.get("t", "deployments/x").await.unwrap().unwrap();
        assert_eq!((read.doc.as_str(), read.v), (r#"{"s":"ready"}"#, 2));
    }

    #[tokio::test]
    async fn lists_by_kind() {
        let items = Items::memory();
        items.put_if("t", "projects/b", "project", "{}", None).await.unwrap();
        items.put_if("t", "projects/a", "project", "{}", None).await.unwrap();
        items.put_if("t", "deployments/x", "deployment", "{}", None).await.unwrap();
        assert_eq!(items.list("t", "project").await.unwrap(), vec![("projects/a".to_owned(), 1), ("projects/b".to_owned(), 1)]);
        assert_eq!(items.list("t", "deployment").await.unwrap(), vec![("deployments/x".to_owned(), 1)]);
        assert_eq!(items.all("t").await.unwrap().len(), 3);
    }

    /// The same operations against a real table made like deploy/install/state.yaml's:
    /// `AWS_PROFILE=forkrail cargo test -p forkrail-cli live_ -- --ignored`.
    #[tokio::test]
    #[ignore = "needs AWS credentials and the test table"]
    async fn live_items_in_a_real_table() {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest()).region(aws_config::Region::new("us-east-1")).load().await;
        let table = std::env::var("FORKRAIL_TEST_TABLE").unwrap_or_else(|_| "forkrail-state-test".into());
        let items = Items::Dynamo(aws_sdk_dynamodb::Client::new(&config));
        let id = forkrail_common::random_hex(4);
        let pk = format!("deployments/test{id}");
        let doc = |status: &str| format!(r#"{{"id":"test{id}","project":"test-{id}","created":"2026-11-13T10:00:00Z","status":"{status}"}}"#);
        assert!(items.put_if(&table, &pk, "deployment", &doc("building"), None).await.unwrap());
        assert!(!items.put_if(&table, &pk, "deployment", &doc("building"), None).await.unwrap(), "created twice");
        assert_eq!(items.get(&table, &pk).await.unwrap().unwrap().v, 1);
        assert!(items.put_if(&table, &pk, "deployment", &doc("ready"), Some(1)).await.unwrap());
        assert!(!items.put_if(&table, &pk, "deployment", &doc("error"), Some(1)).await.unwrap(), "a stale version won");
        // A copy older than the item is dropped; a newer one lands and adds one to the version.
        items.copy(&table, &pk, "deployment", &doc("error"), 1).await.unwrap();
        assert!(same_json(&items.get(&table, &pk).await.unwrap().unwrap().doc, &doc("ready")));
        items.copy(&table, &pk, "deployment", &doc("canceled"), crate::state::now_ms() + 60_000).await.unwrap();
        let read = items.get(&table, &pk).await.unwrap().unwrap();
        assert!(same_json(&read.doc, &doc("canceled")));
        assert_eq!(read.v, 3);
        assert!(items.all(&table).await.unwrap().contains_key(&pk));
        items.delete(&table, &pk).await.unwrap();
        assert_eq!(items.get(&table, &pk).await.unwrap(), None);
    }

    #[test]
    fn json_compares_by_value() {
        assert!(same_json("{\n  \"a\": 1,\n  \"b\": [1, 2]\n}", r#"{"b":[1,2],"a":1}"#));
        assert!(!same_json(r#"{"a":1}"#, r#"{"a":2}"#));
    }
}
