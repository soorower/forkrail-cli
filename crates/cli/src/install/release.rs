//! A forkrail release, as the installer copies it into an account: the router's binary, the
//! functions' zips, Vector for the routers, the dashboard already built, and the builder's
//! image as an OCI image layout. `deploy/aws/bundle.sh DIR` makes one from a checkout; `manifest.json`
//! names its files.
//!
//! Zips go to the install's bucket under keys named by their content (as control.sh, notify.sh and
//! the others upload them), so the same release twice uploads nothing. The image goes to the
//! install's registry through ECR's own API, layer by layer: no Docker, and a layer the registry
//! has already isn't sent again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use aws_sdk_ecr::primitives::Blob;
use aws_sdk_s3::primitives::ByteStream;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::problem::aws_text;

pub const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// The forkrail version, and the commit it was built from.
    pub version: String,
    #[serde(default)]
    pub commit: String,
    /// File names in the release folder.
    pub router: String,
    pub control: String,
    pub notify: String,
    pub guard: String,
    pub image: String,
    pub screenshot: String,
    pub vector: String,
    /// The dashboard's source as a .tar.gz with one top folder. Only releases from before
    /// `web_built` have it: a release is published for anyone to download, and the source isn't.
    #[serde(default)]
    pub web: Option<String>,
    /// The dashboard already built (`forkrail build`'s folder, without its source), as a .tar.gz
    /// with one top folder: published as it is, so an install builds nothing (a new account's
    /// builder may be too small to).
    #[serde(default)]
    pub web_built: Option<String>,
    /// The builder's image, an OCI image layout folder (`crane pull --format oci`).
    pub builder: String,
}

pub struct Release {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Release {
    pub fn open(dir: &Path) -> Result<Self> {
        let text = std::fs::read(dir.join(MANIFEST)).with_context(|| format!("{} has no {MANIFEST}: is it a forkrail release folder?", dir.display()))?;
        let manifest: Manifest = serde_json::from_slice(&text).with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
        let release = Release { dir: dir.to_owned(), manifest };
        let required = [&release.manifest.router, &release.manifest.control, &release.manifest.notify, &release.manifest.guard, &release.manifest.image, &release.manifest.screenshot, &release.manifest.vector];
        if release.manifest.web.is_none() && release.manifest.web_built.is_none() {
            bail!("the release has no dashboard (web_built)");
        }
        for file in required.into_iter().chain(release.manifest.web_built.as_ref()).chain(release.manifest.web.as_ref()) {
            if !release.dir.join(file).is_file() {
                bail!("the release is missing {file}");
            }
        }
        if !release.dir.join(&release.manifest.builder).join("index.json").is_file() {
            bail!("the release's builder image ({}) isn't an OCI image layout", release.manifest.builder);
        }
        Ok(release)
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(forkrail_common::hex(&hasher.finalize()))
}

/// Uploads `path` to `bucket` as `key` unless an object is there already (keys are named by
/// content, so one that's there is this file). `metadata` goes on the object.
pub async fn upload(s3: &aws_sdk_s3::Client, bucket: &str, key: &str, path: &Path, metadata: &[(&str, &str)]) -> Result<(), String> {
    if s3.head_object().bucket(bucket).key(key).send().await.is_ok() {
        return Ok(());
    }
    let body = ByteStream::from_path(path).await.map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut put = s3.put_object().bucket(bucket).key(key).body(body);
    for (k, v) in metadata {
        put = put.metadata(*k, *v);
    }
    put.send().await.map(drop).map_err(|e| aws_text(&e))
}

/// The key a function's zip goes under: `<dir>/forkrail-<dir>-<first 16 of its SHA-256>.zip`.
pub fn zip_key(dir: &str, sha: &str) -> String {
    format!("{dir}/forkrail-{dir}-{}.zip", &sha[..16])
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Debug, Clone, Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Deserialize)]
struct ImageManifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

fn blob_path(layout: &Path, digest: &str) -> Result<PathBuf> {
    let (algorithm, hex) = digest.split_once(':').with_context(|| format!("{digest} isn't a digest"))?;
    Ok(layout.join("blobs").join(algorithm).join(hex))
}

/// The image's manifest in an OCI layout: the arm64 Linux one when the layout holds an index.
/// Its bytes, as written (the digest is of those bytes), and its media type.
fn image_manifest(layout: &Path) -> Result<(Vec<u8>, String, String)> {
    let index: Index = serde_json::from_slice(&std::fs::read(layout.join("index.json"))?).context("reading the layout's index.json")?;
    let mut descriptor = index.manifests.first().cloned().context("the layout's index lists no image")?;
    // An index inside: pick linux/arm64.
    if descriptor.media_type.contains("index") || descriptor.media_type.contains("manifest.list") {
        let inner: Index = serde_json::from_slice(&std::fs::read(blob_path(layout, &descriptor.digest)?)?).context("reading the image index")?;
        descriptor = inner
            .manifests
            .into_iter()
            .find(|m| m.platform.as_ref().is_some_and(|p| p.architecture == "arm64" && p.os == "linux"))
            .context("the image has no linux/arm64 variant")?;
    }
    let bytes = std::fs::read(blob_path(layout, &descriptor.digest)?).context("reading the image manifest")?;
    let media_type = if descriptor.media_type.is_empty() {
        serde_json::from_slice::<serde_json::Value>(&bytes)?.get("mediaType").and_then(|m| m.as_str()).unwrap_or("application/vnd.oci.image.manifest.v1+json").to_owned()
    } else {
        descriptor.media_type.clone()
    };
    Ok((bytes, media_type, descriptor.digest))
}

/// ECR takes a layer in parts of this size (its minimum part is 5 MiB; the last may be smaller).
const PART: usize = 20 * 1024 * 1024;

/// Layers sent side by side.
const LAYERS_AT_ONCE: usize = 4;

/// Sends one layer to ECR, a part at a time in order; its size.
async fn push_layer(ecr: &aws_sdk_ecr::Client, repository: &str, layout: &Path, blob: &Descriptor) -> Result<u64, String> {
    use std::io::Read;
    let path = blob_path(layout, &blob.digest).map_err(|e| e.to_string())?;
    let mut file = std::fs::File::open(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let started = ecr.initiate_layer_upload().repository_name(repository).send().await.map_err(|e| aws_text(&e))?;
    let upload = started.upload_id().ok_or("ECR gave no upload id")?.to_owned();
    let part = started.part_size().map_or(PART, |p| (p as usize).max(5 * 1024 * 1024)).min(PART.max(5 * 1024 * 1024));
    let mut first = 0usize;
    let mut chunk = vec![0u8; part];
    loop {
        let mut filled = 0;
        while filled < part {
            let n = file.read(&mut chunk[filled..]).map_err(|e| format!("reading {}: {e}", path.display()))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        let last = first + filled - 1;
        ecr.upload_layer_part()
            .repository_name(repository)
            .upload_id(&upload)
            .part_first_byte(first as i64)
            .part_last_byte(last as i64)
            .layer_part_blob(Blob::new(chunk[..filled].to_vec()))
            .send()
            .await
            .map_err(|e| aws_text(&e))?;
        first = last + 1;
        if filled < part {
            break;
        }
    }
    match ecr.complete_layer_upload().repository_name(repository).upload_id(&upload).layer_digests(&blob.digest).send().await {
        Ok(_) => Ok(blob.size),
        Err(e) if aws_text(&e).contains("LayerAlreadyExists") => Ok(blob.size),
        Err(e) => Err(aws_text(&e)),
    }
}

/// Pushes the image in OCI layout `layout` to ECR repository `repository` (in the client's region)
/// as `tag`; its digest. Layers the repository has are skipped.
pub async fn push_image(ecr: &aws_sdk_ecr::Client, repository: &str, layout: &Path, tag: &str, progress: &(dyn Fn(String) + Sync)) -> Result<String, String> {
    let (manifest_bytes, media_type, digest) = image_manifest(layout).map_err(|e| format!("{e:#}"))?;
    let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes).map_err(|e| format!("reading the image manifest: {e}"))?;
    let mut blobs = vec![manifest.config.clone()];
    blobs.extend(manifest.layers.iter().cloned());
    let digests: Vec<String> = blobs.iter().map(|b| b.digest.clone()).collect();
    let available = ecr.batch_check_layer_availability().repository_name(repository).set_layer_digests(Some(digests)).send().await.map_err(|e| aws_text(&e))?;
    let have: BTreeMap<String, bool> = available
        .layers()
        .iter()
        .filter_map(|l| Some((l.layer_digest()?.to_owned(), l.layer_availability().is_some_and(|a| a.as_str() == "AVAILABLE"))))
        .collect();
    let missing: Vec<&Descriptor> = blobs.iter().filter(|b| !have.get(&b.digest).copied().unwrap_or(false)).collect();
    let total: u64 = missing.iter().map(|b| b.size).sum();
    progress(format!("builder image: {} of {} layers to send, {} MB", missing.len(), blobs.len(), total / 1_000_000));
    // A few layers at once: one upload from far away is slowed by the distance more than by the
    // line, and a layer's parts must go in order. Each part is read when it's sent, so memory
    // stays at a few parts whatever the layers' sizes.
    let count = missing.len();
    let sent = std::sync::atomic::AtomicUsize::new(0);
    let mut uploads = futures_util::stream::FuturesUnordered::new();
    let mut queue = missing.into_iter();
    loop {
        while uploads.len() < LAYERS_AT_ONCE {
            let Some(blob) = queue.next() else { break };
            uploads.push(push_layer(ecr, repository, layout, blob));
        }
        match futures_util::StreamExt::next(&mut uploads).await {
            Some(Ok(size)) => {
                let done = sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if size >= 10_000_000 || done == count {
                    progress(format!("builder image: {done} of {count} layers sent"));
                }
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }
    let manifest_text = String::from_utf8(manifest_bytes).map_err(|_| "the image manifest isn't UTF-8".to_owned())?;
    match ecr.put_image().repository_name(repository).image_manifest(&manifest_text).image_manifest_media_type(&media_type).image_tag(tag).image_digest(&digest).send().await {
        Ok(_) => Ok(digest),
        Err(e) if aws_text(&e).contains("ImageAlreadyExists") => Ok(digest),
        Err(e) => Err(aws_text(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_keys_follow_the_scripts() {
        assert_eq!(zip_key("control", "61154142d8fe6aa9ffff"), "control/forkrail-control-61154142d8fe6aa9.zip");
    }

    #[test]
    fn picks_arm64_from_an_index() {
        let dir = std::env::temp_dir().join(format!("forkrail-oci-{}", forkrail_common::random_hex(6)));
        let blobs = dir.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:c","size":1},"layers":[]}"#;
        std::fs::write(blobs.join("m"), manifest).unwrap();
        std::fs::write(
            blobs.join("i"),
            br#"{"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:x","platform":{"architecture":"amd64","os":"linux"}},{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:m","platform":{"architecture":"arm64","os":"linux"}}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("index.json"), br#"{"manifests":[{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"sha256:i"}]}"#).unwrap();
        let (bytes, media_type, digest) = image_manifest(&dir).unwrap();
        assert_eq!(bytes, manifest);
        assert_eq!(digest, "sha256:m");
        assert!(media_type.contains("manifest"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
