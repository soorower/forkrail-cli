//! Image Optimization (`/_vercel/image`) as Vercel runs it: a deployment's `images` settings, and
//! the checks a request to the optimizer passes before any image is fetched.
//!
//! The settings come from `vercel.json`'s `images` (any project) or a Build Output's
//! `config.json` `images` (what a framework's adapter writes); both have the same shape, and
//! `parse` turns either into an `ImagesConfig` with every default filled in. The CLI stores that,
//! as JSON, by its SHA-256 (`static/images/<id>.json`, see `statics::images_key`), and the
//! deployment's table entry names it (`Deployment.images`), as with routing programs. A deployment
//! with settings has `/_vercel/image` (and `/_next/image`, as on Vercel) as a file of its own; one
//! without has neither, so those paths reach its function like any other.
//!
//! What a request may ask was recorded from Vercel itself (`tools/image-check.py`, saved in
//! `testdata/vercel-images.json`): `w` and `q` are read as JavaScript's `Number` reads them
//! (`640.0`, `0x280` and ` 640` are 640; `640abc` isn't a number), `q` is checked so too but
//! encoded with its leading digits (75.5 is 75), a parameter given twice is refused, `url` may be a path with
//! or without its leading slash (Astro sends `_astro%2F…`), an absolute `http(s)` URL the settings
//! allow, or a nested `/_vercel/image?url=…`, whose own `url` is used. `Accept` picks the first of
//! `formats` it names exactly, whatever its `q`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The paths that answer with optimized images, on a deployment with `images`.
pub const PATHS: [&str; 4] = ["/_vercel/image", "/_vercel/image/", "/_next/image", "/_next/image/"];

/// What Vercel sends when the settings name no `contentSecurityPolicy` (seen 2026-11-16).
pub const DEFAULT_CSP: &str = "script-src 'none'; sandbox;";
/// Without `minimumCacheTTL`, Vercel's documented default (vercel.com/docs/image-optimization).
pub const DEFAULT_MINIMUM_CACHE_TTL: u64 = 3600;
/// Without `formats`, as Next.js and Vercel default.
pub const DEFAULT_FORMATS: [&str; 1] = ["image/webp"];
/// Output formats the settings may name.
pub const FORMATS: [&str; 4] = ["image/avif", "image/webp", "image/jpeg", "image/png"];
/// Source formats that are resized and re-encoded; any other image is served as it is
/// (vercel.com/docs/image-optimization/limits-and-pricing).
pub const OPTIMIZED: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/avif"];
/// The widest or tallest source Vercel optimizes.
pub const MAX_SOURCE_SIDE: u32 = 8192;
/// How deep nested `/_vercel/image?url=…` URLs are followed.
const MAX_NESTING: usize = 3;
/// Longest `url`, as Next.js allows.
const MAX_URL: usize = 3072;

/// A deployment's image settings, every default filled in. Field names are Vercel's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesConfig {
    /// The widths `w` may ask for.
    pub sizes: Vec<u32>,
    /// Hosts whose images may be optimized, by exact name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    /// URLs elsewhere that may be optimized: `hostname` and `pathname` are regular expressions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_patterns: Vec<RemotePattern>,
    /// The deployment's own paths that may be optimized; `None` allows every one, an empty list
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_patterns: Option<Vec<LocalPattern>>,
    /// The qualities `q` may ask for; `None` allows 1 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualities: Option<Vec<u32>>,
    /// The least time an optimized image of a function or a site elsewhere is kept, in seconds.
    #[serde(rename = "minimumCacheTTL")]
    pub minimum_cache_ttl: u64,
    /// Output formats, in the order they're preferred when `Accept` names several.
    pub formats: Vec<String>,
    #[serde(rename = "dangerouslyAllowSVG", default)]
    pub dangerously_allow_svg: bool,
    pub content_security_policy: String,
    /// `inline` or `attachment`.
    pub content_disposition_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePattern {
    /// `http` or `https`; either when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub hostname: String,
    /// The port exactly as the URL has it (`""` for the scheme's own); any when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    /// The query exactly (`""`, or `?v=1`); any when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// Check an `images` value from `vercel.json` or a Build Output's `config.json` against Vercel's
/// schema (openapi.vercel.sh/vercel.json) and fill in the defaults. The error says what's wrong,
/// for the build log.
pub fn parse(value: &Value) -> Result<ImagesConfig, String> {
    let top = value.as_object().ok_or("`images` must be an object")?;
    const KEYS: [&str; 10] = [
        "sizes",
        "domains",
        "remotePatterns",
        "localPatterns",
        "qualities",
        "minimumCacheTTL",
        "formats",
        "dangerouslyAllowSVG",
        "contentSecurityPolicy",
        "contentDispositionType",
    ];
    if let Some(key) = top.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!("`images.{key}` isn't one of Vercel's image settings ({})", KEYS.join(", ")));
    }
    let sizes = array(top.get("sizes"), "sizes", 1, 50)?.ok_or("`images.sizes` is required: the widths `w` may ask for")?;
    let sizes = sizes
        .iter()
        .map(|v| match v.as_f64() {
            Some(n) if n >= 1.0 && n.fract() == 0.0 && n <= f64::from(u32::MAX) => Ok(n as u32),
            _ => Err(format!("`images.sizes`: {v} isn't a width in pixels (a whole number from 1)")),
        })
        .collect::<Result<Vec<u32>, String>>()?;
    let domains = match array(top.get("domains"), "domains", 0, 50)? {
        Some(items) => items.iter().map(|v| string(v, "domains[]", 1, 256)).collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let remote_patterns = match array(top.get("remotePatterns"), "remotePatterns", 0, 51)? {
        Some(items) => items.iter().enumerate().map(|(i, v)| remote_pattern(v, i)).collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let local_patterns = match array(top.get("localPatterns"), "localPatterns", 0, 27)? {
        Some(items) => Some(items.iter().enumerate().map(|(i, v)| local_pattern(v, i)).collect::<Result<Vec<_>, _>>()?),
        None => None,
    };
    let qualities = match array(top.get("qualities"), "qualities", 1, 20)? {
        Some(items) => Some(
            items
                .iter()
                .map(|v| match v.as_u64() {
                    Some(q @ 1..=100) => Ok(q as u32),
                    _ => Err(format!("`images.qualities`: {v} isn't a quality (a whole number from 1 to 100)")),
                })
                .collect::<Result<Vec<u32>, String>>()?,
        ),
        None => None,
    };
    let minimum_cache_ttl = match top.get("minimumCacheTTL") {
        None | Some(Value::Null) => DEFAULT_MINIMUM_CACHE_TTL,
        // Vercel's schema starts at 1; Next.js lets an app set 0, which its builder passes on.
        Some(v) => match v.as_u64() {
            Some(s @ 0..=315_360_000) => s,
            _ => return Err(format!("`images.minimumCacheTTL`: {v} isn't a number of seconds from 0 to 315360000")),
        },
    };
    let formats = match array(top.get("formats"), "formats", 1, 4)? {
        Some(items) => items
            .iter()
            .map(|v| match v.as_str() {
                Some(f) if FORMATS.contains(&f) => Ok(f.to_owned()),
                _ => Err(format!("`images.formats`: {v} isn't one of {}", FORMATS.join(", "))),
            })
            .collect::<Result<Vec<String>, String>>()?,
        None => DEFAULT_FORMATS.iter().map(|f| (*f).to_owned()).collect(),
    };
    let dangerously_allow_svg = match top.get("dangerouslyAllowSVG") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(v) => return Err(format!("`images.dangerouslyAllowSVG`: {v} isn't true or false")),
    };
    let content_security_policy = match top.get("contentSecurityPolicy") {
        None | Some(Value::Null) => DEFAULT_CSP.to_owned(),
        Some(v) => string(v, "contentSecurityPolicy", 1, 256)?,
    };
    if content_security_policy.chars().any(|c| c.is_control()) {
        return Err("`images.contentSecurityPolicy` can't hold control characters".into());
    }
    let content_disposition_type = match top.get("contentDispositionType") {
        None | Some(Value::Null) => "inline".to_owned(),
        Some(Value::String(s)) if s == "inline" || s == "attachment" => s.clone(),
        Some(v) => return Err(format!("`images.contentDispositionType`: {v} isn't \"inline\" or \"attachment\"")),
    };
    let config = ImagesConfig {
        sizes,
        domains,
        remote_patterns,
        local_patterns,
        qualities,
        minimum_cache_ttl,
        formats,
        dangerously_allow_svg,
        content_security_policy,
        content_disposition_type,
    };
    // Patterns that don't compile would refuse every request at the router: say so now.
    Compiled::new(config.clone())?;
    Ok(config)
}

impl ImagesConfig {
    /// The JSON stored at `static/images/<id>.json`, and its id: the SHA-256 of these bytes.
    pub fn to_stored(&self) -> (Vec<u8>, String) {
        use sha2::Digest;
        let json = serde_json::to_vec(self).expect("the settings serialize");
        let id = crate::hex(&sha2::Sha256::digest(&json));
        (json, id)
    }
}

fn array<'a>(value: Option<&'a Value>, name: &str, min: usize, max: usize) -> Result<Option<&'a Vec<Value>>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.len() < min => Err(format!("`images.{name}` needs at least {min} item{}", if min == 1 { "" } else { "s" })),
        Some(Value::Array(items)) if items.len() > max => Err(format!("`images.{name}` takes at most {max} items")),
        Some(Value::Array(items)) => Ok(Some(items)),
        Some(_) => Err(format!("`images.{name}` must be a list")),
    }
}

fn string(value: &Value, name: &str, min: usize, max: usize) -> Result<String, String> {
    match value.as_str() {
        Some(s) if s.chars().count() >= min && s.chars().count() <= max => Ok(s.to_owned()),
        Some(_) => Err(format!("`images.{name}` must be {min} to {max} characters")),
        None => Err(format!("`images.{name}`: {value} isn't a string")),
    }
}

fn remote_pattern(value: &Value, i: usize) -> Result<RemotePattern, String> {
    let name = format!("remotePatterns[{i}]");
    let o = value.as_object().ok_or(format!("`images.{name}` must be an object"))?;
    if let Some(key) = o.keys().find(|k| !["protocol", "hostname", "port", "pathname", "search"].contains(&k.as_str())) {
        return Err(format!("`images.{name}.{key}` isn't a pattern's field (protocol, hostname, port, pathname, search)"));
    }
    let opt = |key: &str, min: usize, max: usize| -> Result<Option<String>, String> {
        match o.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => string(v, &format!("{name}.{key}"), min, max).map(Some),
        }
    };
    let protocol = opt("protocol", 1, 5)?;
    if let Some(p) = &protocol
        && p != "http"
        && p != "https"
    {
        return Err(format!("`images.{name}.protocol` must be \"http\" or \"https\""));
    }
    let hostname = match o.get("hostname") {
        Some(v) => string(v, &format!("{name}.hostname"), 1, 256)?,
        None => return Err(format!("`images.{name}.hostname` is required")),
    };
    Ok(RemotePattern { protocol, hostname, port: opt("port", 0, 5)?, pathname: opt("pathname", 1, 256)?, search: opt("search", 0, 256)? })
}

fn local_pattern(value: &Value, i: usize) -> Result<LocalPattern, String> {
    let name = format!("localPatterns[{i}]");
    let o = value.as_object().ok_or(format!("`images.{name}` must be an object"))?;
    if let Some(key) = o.keys().find(|k| !["pathname", "search"].contains(&k.as_str())) {
        return Err(format!("`images.{name}.{key}` isn't a pattern's field (pathname, search)"));
    }
    let opt = |key: &str, min: usize, max: usize| -> Result<Option<String>, String> {
        match o.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => string(v, &format!("{name}.{key}"), min, max).map(Some),
        }
    };
    Ok(LocalPattern { pathname: opt("pathname", 1, 512)?, search: opt("search", 0, 256)? })
}

/// A pattern's regular expression: Rust's own engine when it can take it, else one with
/// look-around, as JavaScript's has.
enum Re {
    Std(regex::Regex),
    Fancy(Box<fancy_regex::Regex>),
}

impl Re {
    fn new(pattern: &str, what: &str) -> Result<Self, String> {
        match regex::Regex::new(pattern) {
            Ok(re) => Ok(Re::Std(re)),
            Err(std_err) => match fancy_regex::Regex::new(pattern) {
                Ok(re) => Ok(Re::Fancy(Box::new(re))),
                Err(_) => Err(format!("`images.{what}`: {pattern:?} isn't a regular expression ({std_err})")),
            },
        }
    }

    fn is_match(&self, text: &str) -> bool {
        match self {
            Re::Std(re) => re.is_match(text),
            Re::Fancy(re) => re.is_match(text).unwrap_or(false),
        }
    }
}

/// A `remotePatterns` hostname as the regular expression a whole host must match. Adapters write
/// one (`^img\.example\.com$`, Next's glob made a regex); one with no regular expression's
/// signs is a host, `*` standing for one label and `**` for any number of them (Next's globs).
/// Either way it is anchored: `images.example.com` alone would also match
/// `images-example.com.evil.test`.
fn host_pattern(hostname: &str) -> String {
    if hostname.contains(['\\', '(', ')', '[', ']', '^', '$', '?', '+', '|', '{', '}']) {
        return if hostname.starts_with('^') && hostname.ends_with('$') { hostname.to_owned() } else { format!("^(?:{hostname})$") };
    }
    let labels: Vec<String> = hostname
        .split('.')
        .map(|label| match label {
            "**" => ".+".to_owned(),
            "*" => "[^.]+".to_owned(),
            _ => label.split('*').map(regex::escape).collect::<Vec<_>>().join("[^.]*"),
        })
        .collect();
    format!("^{}$", labels.join("\\."))
}

/// The settings with their patterns compiled: what the router checks requests against.
pub struct Compiled {
    pub config: ImagesConfig,
    remote: Vec<(Option<Re>, Option<Re>)>,
    local: Option<Vec<Option<Re>>>,
}

impl Compiled {
    pub fn new(config: ImagesConfig) -> Result<Self, String> {
        let remote = config
            .remote_patterns
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let host = Re::new(&host_pattern(&p.hostname), &format!("remotePatterns[{i}].hostname"))?;
                let path = p.pathname.as_deref().map(|s| Re::new(s, &format!("remotePatterns[{i}].pathname"))).transpose()?;
                Ok((Some(host), path))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let local = match &config.local_patterns {
            Some(list) => Some(
                list.iter()
                    .enumerate()
                    .map(|(i, p)| p.pathname.as_deref().map(|s| Re::new(s, &format!("localPatterns[{i}].pathname"))).transpose())
                    .collect::<Result<Vec<_>, String>>()?,
            ),
            None => None,
        };
        Ok(Self { config, remote, local })
    }

    /// Whether `url` (http or https) may be fetched and optimized.
    fn remote_allowed(&self, url: &url::Url) -> bool {
        let host = url.host_str().unwrap_or_default();
        if self.config.domains.iter().any(|d| d == host) {
            return true;
        }
        let port = url.port().map(|p| p.to_string()).unwrap_or_default();
        let search = search_of(url);
        self.config.remote_patterns.iter().zip(&self.remote).any(|(p, (host_re, path_re))| {
            p.protocol.as_deref().is_none_or(|proto| proto == url.scheme())
                && p.port.as_deref().is_none_or(|want| want == port)
                && host_re.as_ref().is_some_and(|re| re.is_match(host))
                && p.search.as_deref().is_none_or(|want| want == search)
                && path_re.as_ref().is_none_or(|re| re.is_match(url.path()))
        })
    }

    /// Whether one of the deployment's own paths may be optimized.
    fn local_allowed(&self, url: &url::Url) -> bool {
        let Some(compiled) = &self.local else { return true };
        let patterns = self.config.local_patterns.as_deref().unwrap_or_default();
        let search = search_of(url);
        patterns.iter().zip(compiled).any(|(p, path_re)| {
            p.search.as_deref().is_none_or(|want| want == search) && path_re.as_ref().is_none_or(|re| re.is_match(url.path()))
        })
    }

    /// Check a request's query string as Vercel does. Every refusal is the same 400
    /// `INVALID_IMAGE_OPTIMIZE_REQUEST`, so there's nothing more to say than that.
    pub fn request(&self, query: Option<&str>) -> Result<ImageRequest, Invalid> {
        let params = parse_query(query.unwrap_or_default());
        let one = |name: &str| -> Result<String, Invalid> {
            let mut found = params.iter().filter(|(k, _)| k == name).map(|(_, v)| v);
            let first = found.next().ok_or(Invalid)?;
            if found.next().is_some() {
                return Err(Invalid);
            }
            Ok(first.clone())
        };
        let url = one("url")?;
        let w = js_number(&one("w")?).ok_or(Invalid)?;
        let q_text = one("q")?;
        let q = js_number(&q_text).ok_or(Invalid)?;
        let width = self.config.sizes.iter().copied().find(|s| f64::from(*s) == w).ok_or(Invalid)?;
        if !(1.0..=100.0).contains(&q) {
            return Err(Invalid);
        }
        // Checked as a number, encoded with its leading digits: Vercel's `q=7.5e1` passes as 75
        // and comes out as 7 would.
        let quality = js_parse_int(&q_text).unwrap_or(1).clamp(1, 100);
        if self.config.qualities.as_ref().is_some_and(|qs| !qs.contains(&quality)) {
            return Err(Invalid);
        }
        let source = self.source(&url, 0)?;
        Ok(ImageRequest { source, width, quality })
    }

    fn source(&self, raw: &str, depth: usize) -> Result<Source, Invalid> {
        if raw.is_empty() || raw.len() > MAX_URL || raw.starts_with("//") {
            return Err(Invalid);
        }
        if !raw.starts_with('/') && raw.contains("://") {
            let url = url::Url::parse(raw).map_err(|_| Invalid)?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || !self.remote_allowed(&url) {
                return Err(Invalid);
            }
            let mut url = url;
            url.set_fragment(None);
            return Ok(Source::Remote { url: url.to_string() });
        }
        // A path, with its leading slash or without (`photo.png`, `./photo.png`, even
        // `data:image/png;…`, which Vercel looks for as a file): resolved as a browser would
        // against the deployment's root, so `/../photo.png` is `/photo.png`.
        let slashed = if raw.starts_with('/') { raw.to_owned() } else { format!("/{raw}") };
        let base = url::Url::parse("http://n/").expect("a valid base");
        let url = base.join(&slashed).map_err(|_| Invalid)?;
        if url.host_str() != Some("n") {
            return Err(Invalid);
        }
        let decoded = percent_decode(url.path());
        if PATHS.contains(&decoded.as_str()) {
            // A nested optimizer URL: Vercel serves what its own `url` names.
            if depth >= MAX_NESTING {
                return Err(Invalid);
            }
            let inner = parse_query(url.query().unwrap_or_default());
            let mut found = inner.iter().filter(|(k, _)| k == "url").map(|(_, v)| v);
            let first = found.next().ok_or(Invalid)?;
            if found.next().is_some() {
                return Err(Invalid);
            }
            return self.source(first, depth + 1);
        }
        if !self.local_allowed(&url) {
            return Err(Invalid);
        }
        // Vercel finds a file by the path as written (with its leading slash); one written another
        // way (`./photo.png`, `/../photo.png`) it fetches from the deployment instead, and answers
        // as it does a function's image.
        let written = slashed.split(['?', '#']).next().unwrap_or_default();
        let exact = written == decoded;
        Ok(Source::Local { path: url.path().to_owned(), query: url.query().map(str::to_owned), exact })
    }

    /// The output format for a client: the first of `formats` its `Accept` names, if any.
    pub fn format(&self, accept: Option<&str>) -> Option<&str> {
        negotiate(&self.config.formats, accept)
    }

    /// `Content-Disposition` for a file named `stem` of type `content_type`.
    pub fn disposition(&self, stem: &str, content_type: Option<&str>) -> String {
        let ext = content_type.map_or("bin", extension);
        format!("{}; filename=\"{stem}.{ext}\"", self.config.content_disposition_type)
    }
}

/// A request's query that isn't one Vercel would answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

/// What a valid request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRequest {
    pub source: Source,
    pub width: u32,
    pub quality: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// One of the deployment's own paths (percent-encoded, starting with `/`), with its query, and
    /// whether `url` wrote the path just so (not `./a.png` or `/b/../a.png`).
    Local { path: String, query: Option<String>, exact: bool },
    /// A URL elsewhere the settings allow, without its fragment.
    Remote { url: String },
}

impl Source {
    /// The name the answer's `Content-Disposition` gives the file, before its extension: the last
    /// part of the path up to its first dot, letters, digits, `-` and `_` only (Vercel's
    /// `my-photo_1.x.png` is `my-photo_1`; `a b+c.png` is `abc`), or `image`.
    pub fn stem(&self) -> String {
        let path = match self {
            Source::Local { path, .. } => path.clone(),
            Source::Remote { url } => url::Url::parse(url).map(|u| u.path().to_owned()).unwrap_or_default(),
        };
        let last = percent_decode(path.rsplit('/').next().unwrap_or_default());
        let stem: String = last.split('.').next().unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        if stem.is_empty() { "image".to_owned() } else { stem }
    }

    /// The path and query, for a local source.
    pub fn path_and_query(&self) -> Option<String> {
        match self {
            Source::Local { path, query: Some(q), .. } => Some(format!("{path}?{q}")),
            Source::Local { path, query: None, .. } => Some(path.clone()),
            Source::Remote { .. } => None,
        }
    }
}

fn search_of(url: &url::Url) -> String {
    match url.query() {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    }
}

/// The first media range `accept` names, in its own order, that is one of `formats`: compared as
/// written, so `image/webp;q=0` still counts and `IMAGE/WEBP` and `image/*` don't, and q-values
/// don't reorder them, as on Vercel (seen on a Nuxt project, `formats` webp then avif: `image/avif,
/// image/webp` and `image/avif;q=0.9,image/webp` got AVIF, `image/webp,image/avif` WebP).
pub fn negotiate<'a>(formats: &'a [String], accept: Option<&str>) -> Option<&'a str> {
    let accept = accept?;
    accept.split(',').map(|item| item.split(';').next().unwrap_or_default().trim()).find_map(|named| formats.iter().map(String::as_str).find(|f| *f == named))
}

/// A query string's parameters, decoded as a browser's `URLSearchParams` does (`+` is a space).
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(&k.replace('+', " ")), percent_decode(&v.replace('+', " ")))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A string read as JavaScript's `Number(s)` reads it, `None` where that gives `NaN`: white space
/// around it is dropped, empty is 0, `0x`/`0o`/`0b` prefixes, exponents and `Infinity` count.
pub fn js_number(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            return Some(digits.chars().fold(0f64, |n, c| n * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))));
        }
    }
    match t {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // Rust also reads `inf`, `nan` and the like, which JavaScript doesn't.
    if !t.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E')) || !t.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse::<f64>().ok()
}

/// The leading whole number of a string, as JavaScript's `parseInt(s, 10)` reads it.
fn js_parse_int(s: &str) -> Option<u32> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
    let n: u64 = digits.get(..digits.len().min(10))?.parse().ok()?;
    if negative { Some(0) } else { Some(n.min(u64::from(u32::MAX)) as u32) }
}

/// The file extension Vercel gives a type in `Content-Disposition`.
pub fn extension(content_type: &str) -> &'static str {
    match content_type.split(';').next().unwrap_or_default().trim() {
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        "image/x-icns" => "icns",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/jxl" => "jxl",
        "image/heic" => "heic",
        _ => "bin",
    }
}

/// How long a source says it may be kept, in seconds: its `s-maxage`, else its `max-age`, else 0,
/// as Next.js's optimizer reads it.
pub fn max_age(cache_control: Option<&str>) -> u64 {
    let Some(cc) = cache_control else { return 0 };
    let mut found: [Option<String>; 2] = [None, None];
    for directive in cc.split(',') {
        let mut parts = directive.trim().splitn(2, '=');
        let key = parts.next().unwrap_or_default().to_ascii_lowercase();
        let value = parts.next().map(|v| v.trim().trim_matches('"').to_owned());
        match key.as_str() {
            "s-maxage" => found[0] = value,
            "max-age" => found[1] = value,
            _ => {}
        }
    }
    found
        .into_iter()
        .flatten()
        .find_map(|v| {
            let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u64>().ok()
        })
        .unwrap_or(0)
}

/// Whether a source's `Cache-Control` says `immutable` (a framework's hashed assets): Vercel
/// answers an optimized deployment file with `immutable` in place of `must-revalidate` then (seen
/// on Astro's `/_astro/*` images, 2026-11-16).
pub fn immutable(cache_control: Option<&str>) -> bool {
    cache_control.is_some_and(|cc| cc.split(',').any(|d| d.trim().eq_ignore_ascii_case("immutable")))
}

/// The type an image's first bytes say it is, as Next.js's optimizer tells (its
/// `detectContentType`, the formats that matter here); `None` for anything else.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.len() >= sig.len() && &bytes[..sig.len()] == sig;
    let ftyp = |brand: &[u8; 4]| {
        bytes.len() >= 12 && {
            let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            (16..=bytes.len()).contains(&size) && &bytes[4..8] == b"ftyp" && &bytes[8..12] == brand
        }
    };
    if starts(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if starts(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if starts(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if starts(b"<?xml") || starts(b"<svg") {
        Some("image/svg+xml")
    } else if ftyp(b"avif") {
        Some("image/avif")
    } else if starts(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else if starts(b"icns") {
        Some("image/x-icns")
    } else if starts(&[0x49, 0x49, 0x2a, 0]) || starts(&[0x4d, 0x4d, 0, 0x2a]) {
        Some("image/tiff")
    } else if starts(b"BM") {
        Some("image/bmp")
    } else if starts(&[0xff, 0x0a]) || starts(&[0, 0, 0, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a]) {
        Some("image/jxl")
    } else if ftyp(b"heic") {
        Some("image/heic")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sveltekit() -> Compiled {
        Compiled::new(parse(&json!({"sizes": [640, 1080], "domains": [], "formats": ["image/webp"], "minimumCacheTTL": 60})).unwrap()).unwrap()
    }

    #[test]
    fn defaults_and_storage() {
        let c = parse(&json!({"sizes": [640]})).unwrap();
        assert_eq!(c.formats, ["image/webp"]);
        assert_eq!(c.minimum_cache_ttl, DEFAULT_MINIMUM_CACHE_TTL);
        assert_eq!(c.content_security_policy, DEFAULT_CSP);
        assert_eq!(c.content_disposition_type, "inline");
        assert!(c.local_patterns.is_none() && c.qualities.is_none() && !c.dangerously_allow_svg);
        let (json, id) = c.to_stored();
        assert_eq!(id.len(), 64);
        assert_eq!(serde_json::from_slice::<ImagesConfig>(&json).unwrap(), c);
        // Every framework's settings recorded from Vercel's builds (docs/data/c16) are taken.
        for v in [
            json!({"sizes": [320, 640, 1280], "domains": [], "formats": ["image/avif", "image/webp"], "minimumCacheTTL": 60, "remotePatterns": []}),
            json!({"domains": [], "minimumCacheTTL": 300, "sizes": [640, 768, 1024, 1280, 1536], "formats": ["image/webp", "image/avif"]}),
            json!({"sizes": [640, 750, 828, 1080, 1200], "domains": [], "minimumCacheTTL": 60, "formats": ["image/avif", "image/webp"],
                "qualities": [25, 50, 75], "localPatterns": [{"pathname": "^/assets/.*$", "search": ""}],
                "remotePatterns": [{"protocol": "https", "hostname": "^via\\.placeholder\\.com$", "port": "", "pathname": "^/1280x640/.*$", "search": "?v=1"}]}),
        ] {
            parse(&v).unwrap();
        }
    }

    #[test]
    fn refusals_say_what_is_wrong() {
        let err = |v: Value| parse(&v).unwrap_err();
        assert!(err(json!({})).contains("sizes` is required"));
        assert!(err(json!({"sizes": []})).contains("at least 1"));
        assert!(err(json!({"sizes": [640.5]})).contains("640.5"));
        assert!(err(json!({"sizes": [640], "formats": ["image/gif"]})).contains("image/gif"));
        assert!(err(json!({"sizes": [640], "qualities": [0]})).contains("quality"));
        assert!(err(json!({"sizes": [640], "minimumCacheTTL": -1})).contains("minimumCacheTTL"));
        assert!(err(json!({"sizes": [640], "contentDispositionType": "x"})).contains("inline"));
        assert!(err(json!({"sizes": [640], "unoptimized": true})).contains("images.unoptimized"));
        assert!(err(json!({"sizes": [640], "remotePatterns": [{"pathname": "/a"}]})).contains("hostname` is required"));
        assert!(err(json!({"sizes": [640], "remotePatterns": [{"hostname": "(", "protocol": "https"}]})).contains("regular expression"));
        assert!(err(json!({"sizes": [640], "localPatterns": [{"pathname": "/a", "x": 1}]})).contains("localPatterns[0].x"));
        assert!(err(json!({"sizes": [640], "domains": vec!["a"; 51]})).contains("at most 50"));
    }

    #[test]
    fn numbers_as_javascript_reads_them() {
        for (s, n) in [("640", 640.0), ("640.0", 640.0), ("0640", 640.0), ("+640", 640.0), (" 640", 640.0), ("640\n", 640.0), ("6.4e2", 640.0), ("0x280", 640.0), ("", 0.0), ("75.5", 75.5), ("-0", 0.0), (".5", 0.5)] {
            assert_eq!(js_number(s), Some(n), "{s:?}");
        }
        for s in ["640abc", "abc", "inf", "NaN", "0x", "1e", "--1", "0x-1", "+0x1"] {
            assert_eq!(js_number(s), None, "{s:?}");
        }
        assert_eq!(js_number("Infinity"), Some(f64::INFINITY));
    }

    #[test]
    fn widths_and_qualities() {
        let c = sveltekit();
        let ok = |q: &str| c.request(Some(q));
        assert_eq!(ok("url=%2Fphoto.png&w=640&q=75").unwrap(), ImageRequest { source: Source::Local { path: "/photo.png".into(), query: None, exact: true }, width: 640, quality: 75 });
        assert_eq!(ok("url=%2Fphoto.png&w=0x280&q=75").unwrap().width, 640);
        assert_eq!(ok("url=%2Fphoto.png&w=6.4e2&q=75").unwrap().width, 640);
        assert_eq!(ok("url=%2Fphoto.png&w=640&q=75.5").unwrap().quality, 75);
        assert_eq!(ok("url=%2Fphoto.png&w=640&q=1.9").unwrap().quality, 1);
        assert_eq!(ok("url=%2Fphoto.png&w=640&q=7.5e1").unwrap().quality, 7);
        assert_eq!(ok("url=%2Fphoto.png&w=640&q=%2B75").unwrap().quality, 75);
        assert_eq!(ok("url=%2Fphoto.png&w=640&q=%2075%20").unwrap().quality, 75);
        for bad in ["url=%2Fphoto.png&w=100&q=75", "url=%2Fphoto.png&w=640.5&q=75", "url=%2Fphoto.png&w=Infinity&q=75", "url=%2Fphoto.png&q=75", "url=%2Fphoto.png&w=640", "url=%2Fphoto.png&w=640&q=0.5", "url=%2Fphoto.png&w=640&q=100.5", "url=%2Fphoto.png&w=640&q=-0", "url=%2Fphoto.png&w=640&w=1080&q=75", "url=%2Fphoto.png&w=640&q=75&q=80", "url=%2Fphoto.png&url=%2Fphoto.png&w=640&q=75", "w=640&q=75", "url=&w=640&q=75", ""] {
            assert_eq!(ok(bad), Err(Invalid), "{bad}");
        }
        let limited = Compiled::new(parse(&json!({"sizes": [640], "qualities": [50, 75]})).unwrap()).unwrap();
        assert!(limited.request(Some("url=/a.png&w=640&q=75")).is_ok());
        assert!(limited.request(Some("url=/a.png&w=640&q=75.9")).is_ok());
        assert_eq!(limited.request(Some("url=/a.png&w=640&q=80")), Err(Invalid));
    }

    #[test]
    fn sources() {
        let c = sveltekit();
        let source = |url: &str| c.request(Some(&format!("w=640&q=75&url={url}"))).map(|r| r.source);
        let local = |path: &str, query: Option<&str>| Ok(Source::Local { path: path.into(), query: query.map(Into::into), exact: true });
        let fetched = |path: &str| Ok(Source::Local { path: path.into(), query: None, exact: false });
        assert_eq!(source("photo.png"), local("/photo.png", None));
        assert_eq!(source("/photo.png"), local("/photo.png", None));
        assert_eq!(source(".%2Fphoto.png"), fetched("/photo.png"));
        assert_eq!(source("%2F..%2Fphoto.png"), fetched("/photo.png"));
        assert_eq!(source("%2Fphoto.png%3Fv%3D1"), local("/photo.png", Some("v=1")));
        assert_eq!(source("%2Fphoto.png%23frag"), local("/photo.png", None));
        assert_eq!(source("_astro%2Fhero.Ab12.png"), local("/_astro/hero.Ab12.png", None));
        assert_eq!(source("%2Fa%20b%C3%A9%2Bc.png"), local("/a%20b%C3%A9+c.png", None));
        // Vercel looks for a data URL as a file, and finds none (404).
        assert!(matches!(source("data%3Aimage%2Fpng%3Bbase64%2CiVBORw0KGgo%3D"), Ok(Source::Local { .. })));
        // A nested optimizer URL serves what it names.
        assert_eq!(source(&urlencode("/_vercel/image?url=%2Fphoto.png&w=640&q=75")), local("/photo.png", None));
        assert_eq!(source(&urlencode("/_next/image?url=%2Fphoto.png&w=1080&q=1")), local("/photo.png", None));
        assert_eq!(source(&urlencode("/_vercel/image?w=640")), Err(Invalid));
        // Elsewhere: only what the settings allow, and only http(s).
        for bad in ["https%3A%2F%2Fexample.com%2Fa.png", "http%3A%2F%2Fexample.com%2Fa.png", "%2F%2Fexample.com%2Fa.png", "ftp%3A%2F%2Fexample.com%2Fa.png", "HTTPS%3A%2F%2Fexample.com%2Fa.png"] {
            assert_eq!(source(bad), Err(Invalid), "{bad}");
        }
    }

    fn urlencode(s: &str) -> String {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    }

    #[test]
    fn patterns() {
        let c = Compiled::new(
            parse(&json!({"sizes": [640], "domains": ["cdn.example.com"],
                "remotePatterns": [{"protocol": "https", "hostname": "^img\\.example\\.org$", "port": "", "pathname": "^/account123/.*$", "search": "?v=1"}],
                "localPatterns": [{"pathname": "^/assets/.*$", "search": ""}]}))
            .unwrap(),
        )
        .unwrap();
        let ok = |url: &str| c.request(Some(&format!("w=640&q=75&url={}", urlencode(url)))).is_ok();
        assert!(ok("https://cdn.example.com/a.png") && ok("http://cdn.example.com/a.png?x=1"));
        assert!(!ok("https://other.example.com/a.png"));
        assert!(ok("https://img.example.org/account123/a.png?v=1"));
        assert!(!ok("https://img.example.org/account123/a.png"), "search must be exactly ?v=1");
        assert!(!ok("http://img.example.org/account123/a.png?v=1"), "protocol");
        assert!(!ok("https://img.example.org:8443/account123/a.png?v=1"), "port");
        assert!(!ok("https://img.example.org/other/a.png?v=1"), "pathname");
        assert!(ok("/assets/a.png") && !ok("/assets/a.png?v=2") && !ok("/other.png"));
        let none = Compiled::new(parse(&json!({"sizes": [640], "localPatterns": []})).unwrap()).unwrap();
        assert_eq!(none.request(Some("w=640&q=75&url=/a.png")), Err(Invalid));

        // A hostname written as a host, or with Next's globs, is the whole host.
        let hosts = Compiled::new(
            parse(&json!({"sizes": [640], "remotePatterns": [{"hostname": "images.example.com"}, {"hostname": "*.cdn.example.net"}, {"hostname": "**.example.io"}, {"hostname": "img\\.example\\.dev"}]}))
                .unwrap(),
        )
        .unwrap();
        let ok = |url: &str| hosts.request(Some(&format!("w=640&q=75&url={}", urlencode(url)))).is_ok();
        assert!(ok("https://images.example.com/a.png") && ok("https://eu.cdn.example.net/a.png") && ok("https://a.b.example.io/a.png") && ok("https://img.example.dev/a.png"));
        for bad in ["https://images-example.com/a.png", "https://images.example.com.evil.test/a.png", "https://a.b.cdn.example.net/a.png", "https://cdn.example.net/a.png", "https://example.io/a.png", "https://img.example.dev.evil.test/a.png"] {
            assert!(!ok(bad), "{bad}");
        }
    }

    #[test]
    fn accept() {
        let both = vec!["image/avif".to_owned(), "image/webp".to_owned()];
        let webp = vec!["image/webp".to_owned()];
        let chrome = "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";
        assert_eq!(negotiate(&both, Some(chrome)), Some("image/avif"));
        assert_eq!(negotiate(&both[1..].to_vec(), Some(chrome)), Some("image/webp"));
        assert_eq!(negotiate(&webp, Some("image/webp;q=0")), Some("image/webp"));
        assert_eq!(negotiate(&webp, Some("text/html, image/webp")), Some("image/webp"));
        for none in ["*/*", "image/*", "IMAGE/WEBP", "xxximage/webpxxx", "image/avif", ""] {
            assert_eq!(negotiate(&webp, Some(none)), None, "{none}");
        }
        assert_eq!(negotiate(&webp, None), None);
        // Nuxt Image lists webp first; the browser's order decides, as on Vercel (its Nuxt test
        // project, 2026-11-16): Chrome and Firefox get AVIF, Safari WebP, q-values notwithstanding.
        let nuxt = vec!["image/webp".to_owned(), "image/avif".to_owned()];
        assert_eq!(negotiate(&nuxt, Some(chrome)), Some("image/avif"));
        assert_eq!(negotiate(&nuxt, Some("image/webp,image/avif,image/jxl,image/heic,*/*;q=0.5")), Some("image/webp"));
        assert_eq!(negotiate(&nuxt, Some("image/avif;q=0.9,image/webp,*/*")), Some("image/avif"));
        assert_eq!(negotiate(&nuxt, Some("image/avif,image/webp;q=0.8,*/*")), Some("image/avif"));
    }

    #[test]
    fn names_ages_and_types() {
        let stem = |url: &str| sveltekit().request(Some(&format!("w=640&q=75&url={url}"))).unwrap().source.stem();
        assert_eq!(stem("%2Fphoto.png"), "photo");
        assert_eq!(stem("%2Fmy-photo_1.x.png"), "my-photo_1");
        assert_eq!(stem("%2Fa%20b%C3%A9%2Bc.png"), "abc");
        assert_eq!(stem("%2Fdir%2F"), "image");
        assert_eq!(stem("data%3Aimage%2Fpng%3Bbase64%2CiVBORw0KGgo%3D"), "pngbase64iVBORw0KGgo");
        let c = sveltekit();
        assert_eq!(c.disposition("photo", Some("image/webp")), "inline; filename=\"photo.webp\"");
        assert_eq!(c.disposition("missing", None), "inline; filename=\"missing.bin\"");
        assert_eq!(max_age(Some("public, max-age=0, must-revalidate")), 0);
        assert_eq!(max_age(Some("public, max-age=31536000, immutable")), 31_536_000);
        assert_eq!(max_age(Some("max-age=10, s-maxage=60")), 60);
        assert_eq!(max_age(Some("max-age=\"30\"")), 30);
        assert!(immutable(Some("public, max-age=31536000, Immutable")));
        assert!(!immutable(Some("public, max-age=0, must-revalidate")) && !immutable(None));
        assert_eq!(max_age(None), 0);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg xmlns"), Some("image/svg+xml"));
        assert_eq!(sniff(b"\0\0\0\x18ftypavif\0\0\0\0mif1avif"), Some("image/avif"));
        assert_eq!(sniff(b"<!--ftypavif-->"), None);
        assert_eq!(sniff(b"User-agent: *"), None);
    }

    /// Every case recorded from Vercel whose answer the query alone decides: refused with 400, or
    /// taken (and then answered by the source's own fate).
    #[test]
    fn recorded_from_vercel() {
        let doc: Value = serde_json::from_str(include_str!("testdata/vercel-images.json")).unwrap();
        let c = Compiled::new(parse(&doc["images"]).unwrap()).unwrap();
        let mut checked = 0;
        for (name, case) in doc["cases"].as_object().unwrap() {
            let target = case["target"].as_str().unwrap();
            let method = case["method"].as_str().unwrap();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            if !PATHS.contains(&path) || !matches!(method, "GET" | "HEAD") {
                continue;
            }
            let answer = &case["answers"][0];
            let status = answer["status"].as_u64().unwrap();
            let refused_by_query = status == 400 && answer["headers"].get("x-vercel-cache").is_none();
            let got = c.request(Some(query).filter(|q| !q.is_empty()));
            if refused_by_query {
                assert_eq!(got, Err(Invalid), "{name}: Vercel refused it on its query alone");
            } else {
                assert!(got.is_ok(), "{name}: Vercel took the query (answered {status})");
                if let Some(image) = answer.get("image") {
                    let want = format!("image/{}", image["format"].as_str().unwrap());
                    let format = c.format(case["request"]["accept"].as_str());
                    // The source is a PNG: without a format of Vercel's, it stays one.
                    assert_eq!(format.unwrap_or("image/png"), want, "{name}");
                    assert_eq!(answer["headers"]["content-disposition"].as_str().unwrap(), c.disposition(&got.unwrap().source.stem(), Some(&want)), "{name}");
                }
            }
            checked += 1;
        }
        assert!(checked >= 50, "only {checked} cases checked");
    }
}
