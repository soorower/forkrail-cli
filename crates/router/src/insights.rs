//! Web Analytics (Vercel's, `/docs/analytics`): page views and custom events, counted in the
//! install's own ClickHouse, with no cookies.
//!
//! `@vercel/analytics` is a loader: it puts `/_vercel/insights/script.js` on the page and queues
//! calls in `window.va`; the script that sends anything is the platform's, not the package's (read
//! from 2.0.1). So the router serves its own (`insights.js`), which answers that queue, at that
//! path and at `/_forkrail/insights/script.js` (`@forkrail/analytics`, or a plain script tag). It
//! posts a beacon to `view` for each page view and to `event` for each custom event; the
//! package's server `track` posts the same `event` from functions, to the deployment's own URL
//! (`VERCEL_URL`), with the visitor's address and agent in its headers.
//!
//! A project has it off until someone switches it on (`ProjectRouting::analytics`), as on Vercel:
//! until then the script is an empty one and beacons are answered and dropped. A beacon kept is
//! one `web event` line in the log, which Vector takes to `forkrail.web_events`
//! (deploy/aws/clickhouse/schema.sql), in place of the request's own line: the line carries the
//! beacon's bytes, which a view adds to `requests_1m`, so usage still counts them.
//!
//! Nothing that names a visitor is kept. Their address and user agent make the day's visitor
//! number (`Salts`), and the agent its device, browser and OS; then both are dropped. The number
//! changes every UTC day, so a visitor who comes back another day counts again. Crawlers, headless
//! browsers and HTTP libraries are known by their agent and dropped.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use hyper::Method;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value};
use tracing::{info, warn};

/// The script pages load.
pub const SCRIPT: &str = include_str!("insights.js");
/// The script gzipped, for the browsers that take it (all of them): about a third of its size.
pub fn script_gzipped() -> bytes::Bytes {
    static GZIPPED: std::sync::LazyLock<bytes::Bytes> = std::sync::LazyLock::new(|| {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(SCRIPT.as_bytes()).expect("writing to memory");
        encoder.finish().expect("writing to memory").into()
    });
    GZIPPED.clone()
}

/// What a project with analytics off gets instead, and what Speed Insights gets (not collected).
pub const EMPTY_SCRIPT: &str = "/* forkrail: Web Analytics is off for this project. */\n";
/// A beacon's largest body: a page's URL and a referrer, or an event's name and properties.
pub const MAX_BODY: usize = 16 * 1024;
/// What of an event is kept: its name's length, how many properties and flags, their names' and
/// values' lengths. The rest is cut, or dropped.
const MAX_NAME: usize = 255;
const MAX_PROPERTIES: usize = 32;
const MAX_KEY: usize = 64;
const MAX_VALUE: usize = 255;
/// A page's path and route, and a UTM value.
const MAX_PATH: usize = 512;
const MAX_UTM: usize = 128;
/// The header the package's server `track` sends, and its visitor's address.
pub const SERVER_HEADER: &str = "x-va-server";
pub const SERVER_ADDRESS_HEADER: &str = "x-vercel-ip";

const PREFIXES: [&str; 2] = ["/_vercel/insights/", "/_forkrail/insights/"];
const SPEED_PREFIX: &str = "/_vercel/speed-insights/";

/// What a request under one of the paths asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The script (`script.js`).
    Script,
    /// A page view's beacon.
    View,
    /// A custom event's beacon.
    Event,
    /// Speed Insights' script: an empty one, as nothing of it is collected.
    SpeedScript,
    /// Another beacon (Speed Insights' vitals, the package's sessions) or a preflight: answered
    /// 204, nothing kept.
    Other,
    NotFound,
}

/// What a request for `path` asks of analytics; None for a path that isn't one of its.
pub fn ask(method: &Method, path: &str) -> Option<Ask> {
    let beacon = *method == Method::POST || *method == Method::OPTIONS;
    if let Some(rest) = path.strip_prefix(SPEED_PREFIX) {
        return Some(match rest {
            _ if rest == "script.js" || rest.ends_with("/script.js") => Ask::SpeedScript,
            _ if beacon => Ask::Other,
            _ => Ask::NotFound,
        });
    }
    let rest = PREFIXES.iter().find_map(|p| path.strip_prefix(p))?;
    Some(match rest {
        _ if rest == "script.js" || rest.ends_with("/script.js") => Ask::Script,
        "view" if *method == Method::POST => Ask::View,
        "event" if *method == Method::POST => Ask::Event,
        _ if beacon => Ask::Other,
        _ => Ask::NotFound,
    })
}

/// What a beacon says, read and cut to size.
#[derive(Debug, Default, PartialEq)]
pub struct Beacon {
    /// `view` or `event`.
    pub kind: &'static str,
    /// The page's host, lowercase.
    pub host: String,
    /// The page's path, decoded, without its query.
    pub path: String,
    /// The framework's route for it (`/blog/[slug]`), when the package sent one.
    pub route: Option<String>,
    /// Where the visitor came from: the referrer's host without `www.`, when it's another site.
    pub referrer: Option<String>,
    /// `utm_source`, `utm_medium`, `utm_campaign`, `utm_term`, `utm_content` of the page's URL.
    pub utm: [Option<String>; 5],
    /// An event's name.
    pub event: Option<String>,
    /// An event's properties, each value as text.
    pub properties: BTreeMap<String, String>,
    /// The feature flags the event was sent with, each value as text.
    pub flags: BTreeMap<String, String>,
    /// The package that sent it, `@vercel/analytics/next`.
    pub sdk: Option<String>,
}

pub const UTM: [&str; 5] = ["utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content"];

impl Beacon {
    /// Read a beacon's body. None for one that isn't a beacon: not JSON, or with no page's URL, or
    /// an event without a name.
    pub fn read(ask: Ask, body: &[u8]) -> Option<Self> {
        let kind = match ask {
            Ask::View => "view",
            Ask::Event => "event",
            _ => return None,
        };
        let Value::Object(fields) = serde_json::from_slice::<Value>(body).ok()? else { return None };
        let text = |name: &str| fields.get(name).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let page = Url::parse(text("o")?)?;
        let mut utm: [Option<String>; 5] = Default::default();
        for (key, value) in page.query.split('&').filter_map(|pair| pair.split_once('=')) {
            if let Some(i) = UTM.iter().position(|u| *u == key) {
                utm[i] = Some(cut(&decode(value, true), MAX_UTM)).filter(|v| !v.is_empty());
            }
        }
        let referrer = text("r").and_then(Url::parse).map(|r| r.site()).filter(|site| *site != page.site() && !site.is_empty());
        let event = match ask {
            Ask::Event => Some(cut(text("en")?, MAX_NAME)),
            _ => None,
        };
        Some(Self {
            kind,
            path: cut(&decode(page.path, false), MAX_PATH),
            route: text("dp").filter(|r| r.starts_with('/')).map(|r| cut(r, MAX_PATH)),
            referrer,
            utm,
            event,
            properties: fields.get("ed").and_then(Value::as_object).map(texts).unwrap_or_default(),
            // `{p: {name: value}}`, as both the script and the server `track` send them.
            flags: fields.get("f").and_then(|f| f.get("p")).and_then(Value::as_object).map(texts).unwrap_or_default(),
            sdk: text("sdkn").map(|s| cut(s, 64)),
            host: page.host,
        })
    }
}

/// An object's values that are text, numbers or booleans, as text; nested ones and nulls are left
/// out, as the package leaves them out in production.
fn texts(map: &Map<String, Value>) -> BTreeMap<String, String> {
    map.iter()
        .filter_map(|(k, v)| {
            let value = match v {
                Value::String(s) => cut(s, MAX_VALUE),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((cut(k, MAX_KEY), value))
        })
        .take(MAX_PROPERTIES)
        .collect()
}

/// The parts of an absolute URL analytics reads.
struct Url<'a> {
    host: String,
    path: &'a str,
    query: &'a str,
}

impl<'a> Url<'a> {
    fn parse(url: &'a str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = rest[..end].rsplit_once('@').map_or(&rest[..end], |(_, host)| host);
        let host = match authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or_default(),
            None => authority.split(':').next().unwrap_or_default(),
        };
        let rest = rest[end..].split('#').next().unwrap_or_default();
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        Some(Self { host: host.trim_end_matches('.').to_ascii_lowercase(), path: if path.is_empty() { "/" } else { path }, query })
    }

    /// The site as a list shows it: the host without `www.`.
    fn site(&self) -> String {
        self.host.strip_prefix("www.").unwrap_or(&self.host).to_owned()
    }
}

/// Percent-decoded text (`+` a space in a query), lossy where the bytes aren't UTF-8.
fn decode(text: &str, plus: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let hex = |c: u8| (c as char).to_digit(16);
        match b {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b),
            },
            b'+' if plus => out.push(b' '),
            _ => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// At most `max` bytes of `text`, cut at a character's edge.
fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// What a user agent says of its device, browser and operating system, as the lists name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    pub device: &'static str,
    pub browser: &'static str,
    pub os: &'static str,
}

impl Agent {
    pub fn of(agent: &str) -> Self {
        let a = agent.to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| a.contains(w));
        let os = match () {
            _ if has(&["windows phone"]) => "Windows Phone",
            _ if has(&["windows"]) => "Windows",
            _ if has(&["iphone", "ipad", "ipod"]) => "iOS",
            _ if has(&["cros"]) => "Chrome OS",
            _ if has(&["android"]) => "Android",
            _ if has(&["macintosh", "mac os x"]) => "Mac",
            _ if has(&["linux", "x11"]) => "GNU/Linux",
            _ => "Other",
        };
        let device = match () {
            _ if has(&["ipad", "tablet", "kindle", "silk/", "playbook"]) || (os == "Android" && !has(&["mobile"])) => "Tablet",
            _ if has(&["mobi", "iphone", "ipod", "windows phone", "blackberry", "opera mini"]) => "Mobile",
            _ => "Desktop",
        };
        let browser = match () {
            _ if has(&["edg/", "edga/", "edgios/", "edge/"]) => "Edge",
            _ if has(&["opr/", "opera", "opios/"]) => "Opera",
            _ if has(&["samsungbrowser/"]) => "Samsung Internet",
            _ if has(&["yabrowser/"]) => "Yandex",
            _ if has(&["ucbrowser/"]) => "UC Browser",
            _ if has(&["vivaldi/"]) => "Vivaldi",
            _ if has(&["duckduckgo/", "ddg/"]) => "DuckDuckGo",
            _ if has(&["fban", "fbav"]) => "Facebook",
            _ if has(&["instagram"]) => "Instagram",
            _ if has(&["firefox/", "fxios/"]) => "Firefox",
            _ if has(&["crios/", "chrome/", "chromium/"]) => "Chrome",
            _ if has(&["safari/"]) => "Safari",
            _ if has(&["msie ", "trident/"]) => "Internet Explorer",
            _ => "Other",
        };
        Self { device, browser, os }
    }
}

/// Whether an agent is a crawler, a headless browser or a monitor: never a visitor.
pub fn is_bot(agent: &str) -> bool {
    const WORDS: [&str; 22] = [
        "bot", "crawl", "spider", "slurp", "headless", "lighthouse", "pagespeed", "pingdom", "uptime", "phantomjs", "puppeteer", "playwright", "selenium", "scrapy",
        "facebookexternalhit", "embedly", "feedfetcher", "mediapartners", "apis-google", "prerender", "preview", "vercel-screenshot",
    ];
    let a = agent.to_ascii_lowercase();
    WORDS.iter().any(|w| a.contains(w))
}

/// Whether an agent is a program rather than a browser (an HTTP library, a command-line tool):
/// what a page's beacon never comes from. A function's server `track` sends its visitor's agent.
pub fn is_library(agent: &str) -> bool {
    const STARTS: [&str; 12] = ["curl/", "wget/", "python-", "python/", "go-http-client", "java/", "okhttp", "axios/", "node-fetch", "node", "undici", "libwww-perl"];
    let a = agent.trim().to_ascii_lowercase();
    a.is_empty() || (!a.starts_with("mozilla/") && STARTS.iter().any(|s| a.starts_with(s))) || !a.contains('/')
}

/// The day a time falls on: days since 1970-01-01, UTC.
pub fn day_of(unix_s: u64) -> u32 {
    (unix_s / 86_400) as u32
}

/// A day as `2026-11-22`.
pub fn date(day: u32) -> String {
    // Howard Hinnant's civil_from_days.
    let z = i64::from(day) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The days' salts, from which visitors' numbers are made: a random key a day, the same on every
/// router, and forgotten once its day is over. The first router to need a day's makes it, at
/// `<prefix><date>` in the state bucket (written only if it isn't there yet, so two routers can't
/// make two), and the others read it; each day's is made the day before, so midnight finds it
/// ready, and deleted once its day has passed. Without the bucket (a local run), or while it
/// can't be reached, a router makes its own, so a visitor may count once on each router that day.
pub struct Salts {
    store: Option<Store>,
    days: RwLock<Vec<(u32, hmac::Key, bool)>>,
}

struct Store {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

/// Where the salts are kept in the state bucket, by default.
pub const SALTS_PREFIX: &str = "state/insights/salts/";
/// How often a router looks after them.
const TEND_EVERY: Duration = Duration::from_secs(60);

impl Salts {
    /// Salts of this router's own.
    pub fn local() -> Self {
        Self { store: None, days: RwLock::default() }
    }

    /// Salts shared through `bucket`.
    pub fn shared(client: aws_sdk_s3::Client, bucket: String, prefix: String) -> Self {
        Self { store: Some(Store { client, bucket, prefix }), days: RwLock::default() }
    }

    /// A visitor's number for the day: from the project, the visitor's address and agent, under
    /// the day's salt. 63 bits, so it stays a whole number in JSON on its way to ClickHouse.
    pub fn visitor(&self, day: u32, project: &str, ip: IpAddr, agent: &str) -> u64 {
        let key = self.key(day);
        let mut ctx = hmac::Context::with_key(&key);
        ctx.update(project.as_bytes());
        ctx.update(&[0]);
        let ip = ip.to_canonical();
        ctx.update(ip.to_string().as_bytes());
        ctx.update(&[0]);
        ctx.update(agent.as_bytes());
        let tag = ctx.sign();
        let mut n = [0u8; 8];
        n.copy_from_slice(&tag.as_ref()[..8]);
        u64::from_be_bytes(n) >> 1
    }

    /// The day's key: the shared one once it's been read, else this router's own for the day.
    fn key(&self, day: u32) -> hmac::Key {
        if let Some((_, key, _)) = self.days.read().unwrap().iter().find(|(d, ..)| *d == day) {
            return key.clone();
        }
        let mut days = self.days.write().unwrap();
        if let Some((_, key, _)) = days.iter().find(|(d, ..)| *d == day) {
            return key.clone();
        }
        if self.store.is_some() {
            warn!(day = %date(day), "no shared salt for the day yet: this router uses its own");
        }
        let key = hmac::Key::new(hmac::HMAC_SHA256, &random_salt());
        days.push((day, key.clone(), false));
        key
    }

    /// Keep today's and tomorrow's shared salts in hand and forget older ones, every minute.
    pub async fn tend(self: Arc<Self>) {
        loop {
            let today = day_of(now_s());
            self.days.write().unwrap().retain(|(d, ..)| *d >= today);
            if let Some(store) = &self.store {
                for day in [today, today + 1] {
                    let shared = self.days.read().unwrap().iter().any(|(d, _, shared)| *d == day && *shared);
                    if shared {
                        continue;
                    }
                    match store.salt(day).await {
                        Ok(salt) => {
                            let mut days = self.days.write().unwrap();
                            days.retain(|(d, ..)| *d != day);
                            days.push((day, hmac::Key::new(hmac::HMAC_SHA256, &salt), true));
                        }
                        Err(e) => warn!(day = %date(day), error = %e, "the day's shared salt couldn't be read or made"),
                    }
                }
                // Yesterday's is of no more use to anyone.
                store.forget(today - 1).await;
            }
            tokio::time::sleep(TEND_EVERY).await;
        }
    }
}

impl Store {
    fn key(&self, day: u32) -> String {
        format!("{}{}", self.prefix, date(day))
    }

    /// The day's salt: read, or made if no router has made it yet.
    async fn salt(&self, day: u32) -> Result<Vec<u8>, String> {
        if let Some(salt) = self.read(day).await? {
            return Ok(salt);
        }
        let salt = random_salt();
        let made = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.key(day))
            .if_none_match("*")
            .content_type("text/plain")
            .body(forkrail_common::hex(&salt).into_bytes().into())
            .send()
            .await;
        match made {
            Ok(_) => {
                info!(day = %date(day), "made the day's salt for visitors");
                Ok(salt)
            }
            // Another router made it first (412), or is making it (409): theirs is the one.
            Err(_) => self.read(day).await?.ok_or_else(|| "neither made nor found".to_owned()),
        }
    }

    async fn read(&self, day: u32) -> Result<Option<Vec<u8>>, String> {
        match self.client.get_object().bucket(&self.bucket).key(self.key(day)).send().await {
            Ok(out) => {
                let body = out.body.collect().await.map_err(|e| e.to_string())?.into_bytes();
                let salt = unhex(String::from_utf8_lossy(&body).trim()).filter(|s| s.len() == 32).ok_or("a salt that isn't 32 bytes of hex")?;
                Ok(Some(salt))
            }
            Err(e) if e.as_service_error().is_some_and(|e| e.is_no_such_key()) => Ok(None),
            Err(e) => Err(aws_sdk_s3::error::DisplayErrorContext(&e).to_string()),
        }
    }

    async fn forget(&self, day: u32) {
        if let Err(e) = self.client.delete_object().bucket(&self.bucket).key(self.key(day)).send().await {
            warn!(day = %date(day), error = %aws_sdk_s3::error::DisplayErrorContext(&e), "an old salt couldn't be deleted");
        }
    }
}

fn random_salt() -> Vec<u8> {
    let mut salt = vec![0u8; 32];
    SystemRandom::new().fill(&mut salt).expect("the system's random source works");
    salt
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Where a kept beacon was taken, and what answering it cost: the rest of its line.
pub struct Place<'a> {
    pub project: &'a str,
    pub deployment: Option<&'a str>,
    pub environment: &'a str,
    /// The host the beacon was sent to (usage counts its bytes there).
    pub request_host: &'a str,
    pub entry: &'static str,
    pub region: &'a str,
    pub country: Option<&'a str>,
    pub status: u16,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub total_ms: u64,
}

/// Write a kept beacon's line.
pub fn write(beacon: &Beacon, visitor: u64, agent: Agent, place: &Place) {
    let json = |m: &BTreeMap<String, String>| (!m.is_empty()).then(|| serde_json::to_string(m).unwrap_or_default());
    info!(
        kind = beacon.kind,
        project = place.project,
        deployment = place.deployment,
        environment = Some(place.environment).filter(|e| !e.is_empty()),
        host = %beacon.host,
        request_host = place.request_host,
        entry = place.entry,
        region = Some(place.region).filter(|r| !r.is_empty()),
        path = %beacon.path,
        route = beacon.route.as_deref(),
        referrer = beacon.referrer.as_deref(),
        utm_source = beacon.utm[0].as_deref(),
        utm_medium = beacon.utm[1].as_deref(),
        utm_campaign = beacon.utm[2].as_deref(),
        utm_term = beacon.utm[3].as_deref(),
        utm_content = beacon.utm[4].as_deref(),
        country = place.country,
        device = agent.device,
        browser = agent.browser,
        os = agent.os,
        visitor,
        event = beacon.event.as_deref(),
        properties = json(&beacon.properties).as_deref(),
        flags = json(&beacon.flags).as_deref(),
        sdk = beacon.sdk.as_deref(),
        status = place.status,
        bytes_in = place.bytes_in,
        bytes_out = place.bytes_out,
        total_ms = place.total_ms,
        "web event"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(body: &str) -> Option<Beacon> {
        Beacon::read(Ask::View, body.as_bytes())
    }

    #[test]
    fn paths_as_the_packages_ask_them() {
        assert_eq!(ask(&Method::GET, "/_vercel/insights/script.js"), Some(Ask::Script));
        assert_eq!(ask(&Method::GET, "/_forkrail/insights/script.js"), Some(Ask::Script));
        assert_eq!(ask(&Method::POST, "/_vercel/insights/view"), Some(Ask::View));
        assert_eq!(ask(&Method::POST, "/_forkrail/insights/event"), Some(Ask::Event));
        assert_eq!(ask(&Method::POST, "/_vercel/insights/session"), Some(Ask::Other));
        assert_eq!(ask(&Method::OPTIONS, "/_vercel/insights/view"), Some(Ask::Other));
        assert_eq!(ask(&Method::GET, "/_vercel/insights/view"), Some(Ask::NotFound));
        assert_eq!(ask(&Method::GET, "/_vercel/speed-insights/script.js"), Some(Ask::SpeedScript));
        assert_eq!(ask(&Method::POST, "/_vercel/speed-insights/vitals"), Some(Ask::Other));
        assert_eq!(ask(&Method::GET, "/_vercel/image"), None);
        assert_eq!(ask(&Method::GET, "/insights/script.js"), None);
    }

    #[test]
    fn a_view_keeps_the_page_its_referrer_and_campaign() {
        let b = view(r#"{"o":"https://Shop.example.com:443/blog/%C3%BCber?utm_source=news+letter&utm_campaign=fall&x=1#top","r":"https://www.google.com/search?q=x","dp":"/blog/[slug]","sdkn":"@vercel/analytics/next","ts":1}"#).unwrap();
        assert_eq!((b.kind, b.host.as_str(), b.path.as_str()), ("view", "shop.example.com", "/blog/über"));
        assert_eq!(b.route.as_deref(), Some("/blog/[slug]"));
        assert_eq!(b.referrer.as_deref(), Some("google.com"));
        assert_eq!(b.utm[0].as_deref(), Some("news letter"));
        assert_eq!(b.utm[2].as_deref(), Some("fall"));
        assert_eq!(b.utm[1], None);
        assert_eq!(b.sdk.as_deref(), Some("@vercel/analytics/next"));
        assert!(b.event.is_none() && b.properties.is_empty());
    }

    #[test]
    fn a_referrer_of_the_same_site_or_none_is_no_referrer() {
        assert_eq!(view(r#"{"o":"https://www.example.com/a","r":"https://example.com/b"}"#).unwrap().referrer, None);
        assert_eq!(view(r#"{"o":"https://example.com/a","r":""}"#).unwrap().referrer, None);
        assert_eq!(view(r#"{"o":"https://example.com/"}"#).unwrap().referrer, None);
        let app = view(r#"{"o":"https://example.com/","r":"android-app://com.google.android.googlequicksearchbox/"}"#).unwrap();
        assert_eq!(app.referrer.as_deref(), Some("com.google.android.googlequicksearchbox"));
    }

    #[test]
    fn what_isnt_a_beacon_is_dropped() {
        assert!(view("not json").is_none());
        assert!(view(r#"["https://example.com/"]"#).is_none());
        assert!(view(r#"{"r":"https://example.com/"}"#).is_none(), "no page");
        assert!(view(r#"{"o":"/relative"}"#).is_none());
        assert!(Beacon::read(Ask::Event, br#"{"o":"https://example.com/"}"#).is_none(), "an event without a name");
        assert!(Beacon::read(Ask::Script, br#"{"o":"https://example.com/"}"#).is_none());
    }

    #[test]
    fn an_event_keeps_its_name_properties_and_flags() {
        let body = r#"{"o":"https://example.com/pricing","en":"Signup","ed":{"plan":"pro","seats":3,"trial":true,"nested":{"a":1},"none":null},"f":{"p":{"new-pricing":true}}}"#;
        let b = Beacon::read(Ask::Event, body.as_bytes()).unwrap();
        assert_eq!((b.kind, b.event.as_deref()), ("event", Some("Signup")));
        assert_eq!(b.properties, BTreeMap::from([("plan".into(), "pro".into()), ("seats".into(), "3".into()), ("trial".into(), "true".into())]));
        assert_eq!(b.flags, BTreeMap::from([("new-pricing".into(), "true".into())]));
    }

    #[test]
    fn long_values_are_cut() {
        let long = "x".repeat(2000);
        let body = format!(r#"{{"o":"https://example.com/{long}","en":"{long}","ed":{{"{long}":"{long}"}}}}"#);
        let b = Beacon::read(Ask::Event, body.as_bytes()).unwrap();
        assert_eq!((b.path.len(), b.event.unwrap().len()), (MAX_PATH, MAX_NAME));
        assert_eq!(b.properties.iter().next().map(|(k, v)| (k.len(), v.len())), Some((MAX_KEY, MAX_VALUE)));
    }

    #[test]
    fn agents_as_the_lists_name_them() {
        let mac = Agent::of("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15");
        assert_eq!((mac.device, mac.browser, mac.os), ("Desktop", "Safari", "Mac"));
        let win = Agent::of("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0");
        assert_eq!((win.device, win.browser, win.os), ("Desktop", "Edge", "Windows"));
        let iphone = Agent::of("Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/131.0 Mobile/15E148 Safari/604.1");
        assert_eq!((iphone.device, iphone.browser, iphone.os), ("Mobile", "Chrome", "iOS"));
        let android = Agent::of("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36");
        assert_eq!((android.device, android.browser, android.os), ("Mobile", "Chrome", "Android"));
        let tablet = Agent::of("Mozilla/5.0 (Linux; Android 13; SM-X700) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/23.0 Chrome/115.0.0.0 Safari/537.36");
        assert_eq!((tablet.device, tablet.browser, tablet.os), ("Tablet", "Samsung Internet", "Android"));
        let linux = Agent::of("Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0");
        assert_eq!((linux.device, linux.browser, linux.os), ("Desktop", "Firefox", "GNU/Linux"));
    }

    #[test]
    fn robots_and_programs_are_no_visitors() {
        assert!(is_bot("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"));
        assert!(is_bot("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/131.0.0.0 Safari/537.36"));
        assert!(is_bot("Mozilla/5.0 (Windows NT 10.0) Chrome-Lighthouse"));
        assert!(!is_bot("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"));
        assert!(is_library("curl/8.5.0"));
        assert!(is_library("python-requests/2.32.3"));
        assert!(is_library("node"));
        assert!(is_library(""));
        assert!(!is_library("Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15"));
    }

    #[test]
    fn a_visitor_is_the_same_all_day_and_another_the_next() {
        let salts = Salts::local();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let a = salts.visitor(20_414, "shop", ip, "agent");
        assert_eq!(a, salts.visitor(20_414, "shop", ip, "agent"));
        assert_eq!(a, salts.visitor(20_414, "shop", "::ffff:203.0.113.7".parse().unwrap(), "agent"), "an IPv4 address seen over IPv6");
        assert_ne!(a, salts.visitor(20_415, "shop", ip, "agent"));
        assert_ne!(a, salts.visitor(20_414, "blog", ip, "agent"));
        assert_ne!(a, salts.visitor(20_414, "shop", ip, "other agent"));
        assert!(a < 1 << 63);
    }

    #[test]
    fn days_and_dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(day_of(1_795_305_600)), "2026-11-22");
        assert_eq!(date(day_of(951_782_400)), "2000-02-29");
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0g"), None);
    }

    #[test]
    fn the_script_is_gzipped_whole() {
        use std::io::Read;
        let gz = script_gzipped();
        let mut text = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut text).unwrap();
        assert_eq!(text, SCRIPT);
        assert!(gz.len() < SCRIPT.len() / 2, "{} of {}", gz.len(), SCRIPT.len());
    }

    #[test]
    fn the_script_answers_the_packages_queue() {
        for needle in ["window", "vaq", "beforeSend", "pageview", "disableAutoTrack", "/view", "/event", "credentials: \"same-origin\""] {
            assert!(SCRIPT.contains(needle), "{needle}");
        }
    }
}
