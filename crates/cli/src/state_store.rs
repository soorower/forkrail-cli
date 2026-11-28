//! Moving project files and deployment records between the bucket and the install's table, and
//! checking the two agree (`forkrail state`). The way over, each step a command:
//! 1. `forkrail state mode mirror`: the bucket is still read; every write is copied to the table
//! 2. `forkrail state migrate`: everything written before that is copied too
//! 3. `forkrail state mode dynamodb`: once `check` finds the two the same, the table is read, and
//!    the bucket keeps getting a copy of every write, so `forkrail state mode s3` is the way back
//! 4. later, `forkrail state mode dynamodb --no-s3-copy`
//!
//! Running processes see a change within `table::STORE_TTL`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use forkrail_common::install::{STORE_KEY, TABLE_KINDS, state_key, table_item};

use crate::deploy::Ctx;
use crate::state::{Store, now_ms};
use crate::table::{Item, Mode, StoreConfig, same_json};

/// Reads at once from the bucket.
const AT_ONCE: usize = 16;

/// The files the bucket holds that belong in the table: `(bucket key, (kind, item key))`.
async fn bucket_files(store: &Store) -> Result<Vec<(String, (&'static str, String))>> {
    let mut keys = Vec::new();
    for (_, prefix) in TABLE_KINDS {
        for key in store.bucket_list(prefix).await? {
            if let Some(item) = table_item(&key) {
                keys.push((key, item));
            }
        }
    }
    Ok(keys)
}

/// Those files' JSON, by item key, read a few at a time.
async fn bucket_docs(store: &Store) -> Result<BTreeMap<String, (&'static str, String)>> {
    let files = bucket_files(store).await?;
    let mut docs = BTreeMap::new();
    for batch in files.chunks(AT_ONCE) {
        let mut reads = tokio::task::JoinSet::new();
        for (key, (kind, pk)) in batch.iter().cloned() {
            let store = store.clone();
            reads.spawn(async move {
                let bytes = store.get_bytes(&key).await?;
                Ok::<_, anyhow::Error>((pk, kind, bytes))
            });
        }
        while let Some(read) = reads.join_next().await {
            let (pk, kind, bytes) = read.context("a read stopped")??;
            // Gone since the listing: nothing to compare.
            if let Some(bytes) = bytes {
                docs.insert(pk, (kind, String::from_utf8(bytes).context("a state file isn't UTF-8")?));
            }
        }
    }
    Ok(docs)
}

fn table(store: &Store) -> Result<&crate::state::Table> {
    store.table.as_deref().context("this store has no table")
}

/// The file's JSON as the table keeps it: compact.
fn compact(doc: &str) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::from_str::<serde_json::Value>(doc)?)?)
}

/// Where things are kept now, and how many of each there are in the bucket and in the table.
pub async fn status(ctx: &Ctx) -> Result<()> {
    let config = ctx.store.store_config().await?;
    println!("mode: {} (table {}{})", config.mode.as_str(), config.table, if config.mode == Mode::Dynamodb && !config.s3_copy { ", no copy in the bucket" } else { "" });
    let files = bucket_files(&ctx.store).await?;
    for (kind, _) in TABLE_KINDS {
        let in_bucket = files.iter().filter(|(_, (k, _))| *k == kind).count();
        let in_table = match table(&ctx.store)?.items.list(&config.table, kind).await {
            Ok(items) => items.len().to_string(),
            Err(e) => format!("unreadable ({e:#})"),
        };
        println!("{kind}s: {in_bucket} in the bucket, {in_table} in the table");
    }
    Ok(())
}

/// Copy every project file and deployment record the table doesn't have yet. Ones it has are
/// left: in `mirror` they were copied as they were written, which is newer.
pub async fn migrate(ctx: &Ctx) -> Result<()> {
    let config = ctx.store.store_config().await?;
    if config.mode == Mode::S3 {
        bail!("the bucket is the only store, so writes made during the copy would be missing from the table: `forkrail state mode mirror` first");
    }
    let items = &table(&ctx.store)?.items;
    let docs = bucket_docs(&ctx.store).await?;
    let (mut copied, mut there) = (0, 0);
    for (pk, (kind, doc)) in &docs {
        if items.put_if(&config.table, pk, kind, &compact(doc)?, None).await? {
            copied += 1;
        } else {
            there += 1;
        }
    }
    println!("copied {copied} of {} files to {}; {there} were there already", docs.len(), config.table);
    Ok(())
}

/// How the bucket and the table differ, by item key.
#[derive(Debug, Default)]
pub struct Differences {
    pub only_in_bucket: Vec<String>,
    pub only_in_table: Vec<String>,
    pub different: Vec<String>,
}

impl Differences {
    pub fn none(&self) -> bool {
        self.only_in_bucket.is_empty() && self.only_in_table.is_empty() && self.different.is_empty()
    }

    fn print(&self) {
        for (what, keys) in [("only in the bucket", &self.only_in_bucket), ("only in the table", &self.only_in_table), ("different", &self.different)] {
            if !keys.is_empty() {
                println!("{what} ({}): {}", keys.len(), keys.join(", "));
            }
        }
    }
}

fn compare(bucket: &BTreeMap<String, (&'static str, String)>, table: &BTreeMap<String, Item>) -> Differences {
    let mut d = Differences::default();
    for (pk, (_, doc)) in bucket {
        match table.get(pk) {
            None => d.only_in_bucket.push(pk.clone()),
            Some(item) if !same_json(doc, &item.doc) => d.different.push(pk.clone()),
            Some(_) => {}
        }
    }
    d.only_in_table = table.keys().filter(|pk| !bucket.contains_key(*pk)).cloned().collect();
    d
}

/// Compare the bucket with the table. With `repair`, make the one not read like the one read: the
/// table like the bucket in `s3` and `mirror`, the bucket like the table in `dynamodb`.
pub async fn check(ctx: &Ctx, repair: bool) -> Result<Differences> {
    let config = ctx.store.store_config().await?;
    let items = &table(&ctx.store)?.items;
    let (bucket, in_table) = tokio::try_join!(bucket_docs(&ctx.store), items.all(&config.table))?;
    let d = compare(&bucket, &in_table);
    if d.none() {
        println!("the bucket and {} agree: {} files", config.table, bucket.len());
        return Ok(d);
    }
    d.print();
    if !repair {
        return Ok(d);
    }
    match config.mode {
        Mode::S3 | Mode::Mirror => {
            for pk in d.only_in_bucket.iter().chain(&d.different) {
                let (kind, doc) = &bucket[pk];
                items.copy(&config.table, pk, kind, &compact(doc)?, now_ms()).await?;
            }
            for pk in &d.only_in_table {
                items.delete(&config.table, pk).await?;
            }
            println!("the table now has what the bucket has");
        }
        Mode::Dynamodb => {
            if !config.s3_copy {
                bail!("the bucket isn't kept level with the table (--no-s3-copy): nothing to repair there");
            }
            for pk in d.only_in_table.iter().chain(&d.different) {
                let doc: serde_json::Value = serde_json::from_str(&in_table[pk].doc)?;
                ctx.store.put_bytes(&state_key(pk), serde_json::to_vec_pretty(&doc)?, "application/json").await?;
            }
            for pk in &d.only_in_bucket {
                ctx.store.bucket_delete(&state_key(pk)).await?;
            }
            println!("the bucket now has what the table has");
        }
    }
    Ok(d)
}

/// Say where project files and deployment records are kept from now on. Refused when it would
/// lose writes, unless `force`: the table must agree with the bucket before it's read, and the
/// bucket must have been kept level before going back to it.
pub async fn set_mode(ctx: &Ctx, mode: Mode, s3_copy: bool, force: bool) -> Result<()> {
    let current = ctx.store.store_config().await?;
    let next = StoreConfig { mode, table: current.table.clone(), s3_copy: mode != Mode::Dynamodb || s3_copy };
    if next == current {
        println!("already {}", mode.as_str());
        return Ok(());
    }
    let items = &table(&ctx.store)?.items;
    if mode != Mode::S3 {
        items.list(&next.table, "project").await.with_context(|| format!("the table {} can't be read with these credentials (deploy/aws/stack.sh apply state)", next.table))?;
    }
    if !force {
        match (current.mode, mode) {
            (Mode::S3, Mode::Dynamodb) => bail!("fill the table first: `forkrail state mode mirror`, then `forkrail state migrate`"),
            (Mode::Mirror, Mode::Dynamodb) if !check(ctx, false).await?.none() => {
                bail!("the table doesn't have what the bucket has yet: `forkrail state migrate`, or `forkrail state check --repair`")
            }
            (Mode::Dynamodb, Mode::S3 | Mode::Mirror) if !current.s3_copy => {
                bail!("the bucket hasn't been kept level with the table (--no-s3-copy): `forkrail state mode dynamodb` (with the copy) and `forkrail state check --repair` first")
            }
            (Mode::Dynamodb, Mode::S3 | Mode::Mirror) if !check(ctx, false).await?.none() => {
                bail!("the bucket is behind the table: `forkrail state check --repair` first")
            }
            _ => {}
        }
    }
    ctx.store.put_bytes(STORE_KEY, serde_json::to_vec_pretty(&next)?, "application/json").await?;
    println!("{}: running control functions follow within {} s", mode.as_str(), crate::table::STORE_TTL.as_secs());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differences_by_key_and_by_value() {
        let bucket: BTreeMap<String, (&'static str, String)> = [
            ("projects/a".to_owned(), ("project", "{\n  \"n\": 1\n}".to_owned())),
            ("projects/b".to_owned(), ("project", r#"{"n":1}"#.to_owned())),
            ("deployments/x".to_owned(), ("deployment", r#"{"s":"ready"}"#.to_owned())),
        ]
        .into();
        let table: BTreeMap<String, Item> = [
            ("projects/a".to_owned(), Item { doc: r#"{"n":1}"#.to_owned(), v: 3 }),
            ("projects/b".to_owned(), Item { doc: r#"{"n":2}"#.to_owned(), v: 1 }),
            ("deployments/y".to_owned(), Item { doc: "{}".to_owned(), v: 1 }),
        ]
        .into();
        let d = compare(&bucket, &table);
        assert_eq!(d.only_in_bucket, vec!["deployments/x"]);
        assert_eq!(d.only_in_table, vec!["deployments/y"]);
        assert_eq!(d.different, vec!["projects/b"]);
        assert!(!d.none());
        assert!(compare(&BTreeMap::new(), &BTreeMap::new()).none());
    }
}
