//! A deployment's static files: what the CLI uploads and the router serves before the function.
//!
//! Each file is stored once per region, by content: `static/<sha256>` in the region's bucket, with
//! `static/<sha256>.br` and `static/<sha256>.gz` beside it when compressing pays. A deployment's
//! manifest maps URL paths to those files and is stored the same way, at
//! `static/manifests/<sha256 of the manifest>.json`; the routing table names it by that hash. So
//! files shared by deployments are uploaded once, and nothing is ever rewritten.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The manifest for one deployment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// URL path (`/assets/app-1a2b.js`, starting with `/`, not percent-encoded) → file.
    pub files: BTreeMap<String, StaticFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticFile {
    /// Hex SHA-256 of the file as it is: its key, and its ETag.
    pub sha256: String,
    pub size: u64,
    pub content_type: String,
    pub cache_control: String,
    /// Sizes of the Brotli and gzip copies, when there are any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub br: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gz: Option<u64>,
}

/// A copy of a file as stored: as it is, or compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Identity,
    Brotli,
    Gzip,
}

impl Encoding {
    /// The `Content-Encoding` it's served with, if any.
    pub fn header(self) -> Option<&'static str> {
        match self {
            Encoding::Identity => None,
            Encoding::Brotli => Some("br"),
            Encoding::Gzip => Some("gzip"),
        }
    }
}

impl StaticFile {
    /// The copy's key in a region's bucket.
    pub fn key(&self, encoding: Encoding) -> String {
        object_key(&self.sha256, encoding)
    }

    /// The copy's size, if it exists.
    pub fn size_of(&self, encoding: Encoding) -> Option<u64> {
        match encoding {
            Encoding::Identity => Some(self.size),
            Encoding::Brotli => self.br,
            Encoding::Gzip => self.gz,
        }
    }

    /// Its ETag: strong, and different for each copy, as HTTP requires of strong validators.
    pub fn etag(&self, encoding: Encoding) -> String {
        let short = &self.sha256[..self.sha256.len().min(32)];
        match encoding {
            Encoding::Identity => format!("\"{short}\""),
            Encoding::Brotli => format!("\"{short}-br\""),
            Encoding::Gzip => format!("\"{short}-gz\""),
        }
    }

    /// Whether it has compressed copies, so its responses vary by `Accept-Encoding`.
    pub fn compressed(&self) -> bool {
        self.br.is_some() || self.gz.is_some()
    }
}

pub fn object_key(sha256: &str, encoding: Encoding) -> String {
    match encoding {
        Encoding::Identity => format!("static/{sha256}"),
        Encoding::Brotli => format!("static/{sha256}.br"),
        Encoding::Gzip => format!("static/{sha256}.gz"),
    }
}

pub fn manifest_key(id: &str) -> String {
    format!("static/manifests/{id}.json")
}

/// Whether `id` can name a manifest: a hex SHA-256.
pub fn valid_manifest_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The file for a request's path (already percent-decoded): the path itself, or a folder's
/// `index.html`, with or without the trailing slash, as Vercel serves a folder.
pub fn lookup<'a>(manifest: &'a Manifest, path: &str) -> Option<&'a StaticFile> {
    if let Some(file) = manifest.files.get(path) {
        return Some(file);
    }
    if path.ends_with('/') {
        return manifest.files.get(&format!("{path}index.html"));
    }
    manifest.files.get(&format!("{path}/index.html"))
}

/// A prerender's fallback page in its deployment's manifest (`rules::Prerender::fallback`), by
/// the prerender's function path. Not a URL path (those start with `/`), so no request finds it;
/// being in the manifest uploads it with the static files and keeps it from the sweep.
pub fn fallback_key(function: &str) -> String {
    format!("prerender-fallback:{function}")
}

/// Where a deployment's routing program is kept in each region's bucket.
pub fn rules_key(id: &str) -> String {
    format!("static/rules/{id}.json")
}

/// Where a firewall configuration is, in each region's bucket (see `firewall`), as JSON.
pub fn firewall_key(id: &str) -> String {
    format!("static/firewall/{id}.json")
}

/// Where a deployment's image settings are, in each region's bucket (see `images`), as JSON.
pub fn images_key(id: &str) -> String {
    format!("static/images/{id}.json")
}

/// Where an optimized image is kept, once for the install, in the home bucket: by project, under a
/// name the router makes from what was asked (see the router's `images.rs`). Not under `static/`,
/// which the CLI sweeps by what deployments name; these expire by the bucket's lifecycle instead
/// (31 days, `deploy/aws/image.sh`).
pub fn image_result_key(project: &str, id: &str) -> String {
    format!("images/{project}/{id}")
}

/// Where a table of bulk redirects is, in each region's bucket (see `rules::bulk`).
pub fn redirects_key(id: &str) -> String {
    format!("static/redirects/{id}")
}

/// The best copy the client takes, by its `Accept-Encoding`: Brotli, then gzip, then the file as
/// it is. A coding with `q=0` is refused; `*` stands for any coding not named.
pub fn negotiate(file: &StaticFile, accept_encoding: Option<&str>) -> Encoding {
    let Some(accept) = accept_encoding else { return Encoding::Identity };
    let mut accepted: BTreeMap<String, bool> = BTreeMap::new();
    for item in accept.split(',') {
        let mut parts = item.split(';');
        let coding = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        if coding.is_empty() {
            continue;
        }
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q=").or_else(|| p.trim().strip_prefix("Q=")))
            .find_map(|q| q.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        accepted.insert(coding, q > 0.0);
    }
    let takes = |coding: &str| accepted.get(coding).copied().or_else(|| accepted.get("*").copied()).unwrap_or(false);
    if file.br.is_some() && takes("br") {
        Encoding::Brotli
    } else if file.gz.is_some() && (takes("gzip") || takes("x-gzip")) {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

/// The `Content-Type` for a file name, by its extension. Text types say UTF-8.
pub fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        // Vercel's type for scripts (browsers take either).
        "js" | "mjs" | "cjs" => "application/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        // Images served as they are (Next's optimizer passes them through, and so does the
        // router's): with no type of their own they went out as octet-stream, which the router's
        // `/_next/image` refused as "not an image" (Next's next-image-legacy test: a BMP).
        "bmp" | "dib" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        // Vercel's (mime-types' IANA name; seen on its Astro and Nuxt projects), not `image/x-icon`.
        "ico" => "image/vnd.microsoft.icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Whether a type is worth compressing: text, and the few binary formats that aren't compressed
/// already.
pub fn compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.contains("javascript")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.starts_with("image/svg")
        || content_type.starts_with("image/x-icon")
        || content_type.starts_with("image/vnd.microsoft.icon")
        || content_type.starts_with("application/wasm")
        || content_type.starts_with("font/ttf")
        || content_type.starts_with("font/otf")
}

/// Vercel's default for static files: always revalidated, which costs a 304 when unchanged.
pub const REVALIDATE: &str = "public, max-age=0, must-revalidate";
/// For files whose names change with their contents.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_served_as_they_are_have_their_types() {
        assert_eq!(content_type("/test.bmp"), "image/bmp");
        assert_eq!(content_type("/a/scan.TIFF"), "image/tiff");
        assert_eq!(content_type("/x.bin"), "application/octet-stream");
    }

    fn file(br: bool, gz: bool) -> StaticFile {
        StaticFile {
            sha256: "ab".repeat(32),
            size: 100,
            content_type: "text/css; charset=utf-8".into(),
            cache_control: REVALIDATE.into(),
            br: br.then_some(20),
            gz: gz.then_some(25),
        }
    }

    #[test]
    fn negotiation() {
        let both = file(true, true);
        assert_eq!(negotiate(&both, None), Encoding::Identity);
        assert_eq!(negotiate(&both, Some("gzip, deflate, br, zstd")), Encoding::Brotli);
        assert_eq!(negotiate(&both, Some("gzip")), Encoding::Gzip);
        assert_eq!(negotiate(&both, Some("br;q=0, gzip;q=0.5")), Encoding::Gzip);
        assert_eq!(negotiate(&both, Some("identity")), Encoding::Identity);
        assert_eq!(negotiate(&both, Some("*")), Encoding::Brotli);
        assert_eq!(negotiate(&both, Some("*;q=0")), Encoding::Identity);
        assert_eq!(negotiate(&file(false, true), Some("br, gzip")), Encoding::Gzip);
        assert_eq!(negotiate(&file(false, false), Some("br, gzip")), Encoding::Identity);
    }

    #[test]
    fn lookup_and_keys() {
        let mut m = Manifest::default();
        m.files.insert("/index.html".into(), file(false, false));
        m.files.insert("/docs/index.html".into(), file(true, false));
        m.files.insert("/assets/a.js".into(), file(true, true));
        assert!(lookup(&m, "/assets/a.js").is_some());
        assert!(lookup(&m, "/").is_some());
        assert!(lookup(&m, "/docs/").is_some());
        assert!(lookup(&m, "/docs").is_some());
        assert!(lookup(&m, "/missing").is_none());
        let f = file(true, true);
        assert_eq!(f.key(Encoding::Brotli), format!("static/{}.br", "ab".repeat(32)));
        assert_ne!(f.etag(Encoding::Identity), f.etag(Encoding::Gzip));
        assert!(valid_manifest_id(&"0f".repeat(32)) && !valid_manifest_id("0f") && !valid_manifest_id(&"0F".repeat(32)));
        assert_eq!(content_type("a/b.WOFF2"), "font/woff2");
        assert!(compressible(content_type("x.js")) && !compressible(content_type("x.png")));
    }
}
