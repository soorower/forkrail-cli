//! Let's Encrypt: the base domain's wildcard, and a certificate for each custom domain. A name is
//! proved ours one of three ways (`Method`), the first that works for it (`plan`):
//!
//! 1. **Cloudflare**: a zone the install's token edits holds it. The TXT record
//!    `_acme-challenge.<name>` goes there (DNS-01). Always the way for the base domain.
//! 2. **Delegated**: `_acme-challenge.<name>` is a CNAME to `routes::acme_delegation`, under
//!    `_acme.<base>` in the install's own zone, where the TXT record goes (DNS-01 through the
//!    CNAME, which Let's Encrypt follows). It works before the domain points here, so a domain
//!    moved from elsewhere has its certificate before DNS moves; and it's how a wildcard gets one.
//! 3. **HTTP**: the domain points here already (its addresses are the install's, or it's a CNAME
//!    to a name under the base domain). The key authorization is written to the challenge store
//!    (`state/acme/http-01/<token>`), and every router answers
//!    `http://<name>/.well-known/acme-challenge/<token>` from there on port 80 (proxy.rs), so it
//!    doesn't matter which region Let's Encrypt reaches. The issuer fetches the URL itself first.
//!
//! Before Let's Encrypt is asked anything, the domain's own nameservers are asked (never a caching
//! resolver), so a domain that isn't set up costs no failed validation: Let's Encrypt allows 5 a
//! hour per name.
//!
//! - The Cloudflare token is read from SSM when it's needed, kept in memory for at most
//!   `TOKEN_TTL`, and never logged; Cloudflare's error messages don't contain it.
//! - The ACME account is `account.json` in the base domain's certificate directory, created on
//!   first use, with no contact email.
//! - DNS-01: the TXT record is created, and the zone's authoritative nameservers asked until all of
//!   them answer with it (at most 2 minutes), then it's deleted whatever happened. Stale ones from a
//!   run that crashed are deleted before starting.
//! - The order is finalized with a CSR for a fresh ECDSA P-256 key.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use forkrail_common::domains::{CertMethod, DnsFindings};
use forkrail_common::routes::acme_delegation;
use http_body_util::{BodyExt, Full};
use hyper::{Method as HttpMethod, Request, StatusCode};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::BoxError;
use crate::certs::Store;

/// How long to wait for the TXT record to show up in DNS.
const PROPAGATION_TIMEOUT: Duration = Duration::from_secs(120);
const PROPAGATION_POLL: Duration = Duration::from_secs(3);
/// Each DNS query to an authoritative nameserver.
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
/// Each call to Cloudflare or the resolver.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the Cloudflare token is kept in memory after it's read, and how long a failure to
/// read it stands before it's tried again.
const TOKEN_TTL: Duration = Duration::from_secs(600);
const TOKEN_RETRY: Duration = Duration::from_secs(60);
/// How long the install's own addresses are kept after they're looked up.
const ADDRESSES_TTL: Duration = Duration::from_secs(3600);
/// How long a name's Cloudflare zone is kept once found, and its having none.
const ZONE_TTL: Duration = Duration::from_secs(600);
const NO_ZONE_TTL: Duration = Duration::from_secs(300);
/// CNAMEs followed when resolving a name.
const MAX_CNAMES: usize = 8;
const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";
const DOH: &str = "https://cloudflare-dns.com/dns-query";
/// DNS record types.
const A: u16 = 1;
const NS: u16 = 2;
const CNAME: u16 = 5;
const TXT: u16 = 16;
const AAAA: u16 = 28;

pub struct Acme {
    /// Let's Encrypt's staging server, for tests: its certificates aren't trusted, and its rate
    /// limits are far higher.
    pub staging: bool,
    /// The base domain, whose zone holds delegated challenges.
    pub base: String,
    /// The Cloudflare zone of the base domain, if known; otherwise found by name with the token.
    pub zone: Option<String>,
    /// The SSM SecureString holding a Cloudflare API token with DNS edit on that zone (and any
    /// custom domain's zone it should handle). Without one, only HTTP-01 works.
    pub token_param: String,
    pub ssm: aws_sdk_ssm::Client,
    /// Where HTTP-01 key authorizations go, for every router to answer from.
    pub http01: Option<Store>,
    /// The install's public IPv4 addresses (`FORKRAIL_PUBLIC_ADDRESSES`); otherwise what the base
    /// domain's wildcard resolves to.
    pub addresses: Vec<Ipv4Addr>,
    pub cache: AcmeCache,
}

#[derive(Default)]
pub struct AcmeCache {
    /// The token, or why it couldn't be read (kept a minute, so a missing token isn't asked for by
    /// every domain).
    token: Mutex<Option<(Instant, Result<String, String>)>>,
    /// Held while the token is read, so the domains a router starts with read it once.
    reading: tokio::sync::Mutex<()>,
    addresses: Mutex<Option<(Instant, Vec<Ipv4Addr>)>>,
    /// Each name's Cloudflare zone (or none), as last found: domains waiting on DNS are looked at
    /// every 30 s, and Cloudflare needn't be asked each time.
    zones: Mutex<std::collections::HashMap<String, (Instant, Option<String>)>>,
    /// One HTTPS client for Cloudflare and DNS-over-HTTPS, so its connections are reused.
    http: std::sync::OnceLock<Https>,
}

/// How a name is proved ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// The TXT record `_acme-challenge.<name>` in Cloudflare zone `zone`.
    Cloudflare { zone: String },
    /// The TXT record `target` in the base domain's zone `zone`, which `_acme-challenge.<name>`
    /// is a CNAME to.
    Delegated { zone: String, target: String },
    /// HTTP-01, through the routers' port 80.
    Http,
}

impl Method {
    pub fn kind(&self) -> CertMethod {
        match self {
            Method::Cloudflare { .. } => CertMethod::Cloudflare,
            Method::Delegated { .. } => CertMethod::Delegated,
            Method::Http => CertMethod::Http,
        }
    }
}

/// What the DNS of a name says, and the way to prove it ours if there's one.
#[derive(Debug, Clone)]
pub struct Plan {
    pub method: Option<Method>,
    pub findings: DnsFindings,
    /// Why there's no way, for the domain's owner.
    pub reason: Option<String>,
}

pub struct Issued {
    /// The chain, leaf first.
    pub cert_pem: String,
    pub key_pem: String,
}

impl Acme {
    async fn token(&self) -> Result<String, BoxError> {
        let cached = || match &*self.cache.token.lock().unwrap() {
            Some((at, Ok(token))) if at.elapsed() < TOKEN_TTL => Some(Ok(token.clone())),
            Some((at, Err(e))) if at.elapsed() < TOKEN_RETRY => Some(Err(e.clone())),
            _ => None,
        };
        if let Some(token) = cached() {
            return token.map_err(Into::into);
        }
        let _reading = self.cache.reading.lock().await;
        if let Some(token) = cached() {
            return token.map_err(Into::into);
        }
        let token = read_token(self).await.map_err(|e| e.to_string());
        if let Err(e) = &token {
            warn!(param = %self.token_param, error = %e, "no Cloudflare token: only HTTP challenges work until it can be read");
        }
        *self.cache.token.lock().unwrap() = Some((Instant::now(), token.clone()));
        token.map_err(Into::into)
    }

    fn http(&self) -> Result<Https, BoxError> {
        if let Some(http) = self.cache.http.get() {
            return Ok(http.clone());
        }
        let http = https_client()?;
        Ok(self.cache.http.get_or_init(|| http).clone())
    }

    /// The Cloudflare zone holding `name`, or none the token sees: cached unless `fresh`.
    async fn zone_of(&self, name: &str, fresh: bool) -> Result<Option<String>, BoxError> {
        if !fresh
            && let Some((at, zone)) = self.cache.zones.lock().unwrap().get(name)
            && at.elapsed() < if zone.is_some() { ZONE_TTL } else { NO_ZONE_TTL }
        {
            return Ok(zone.clone());
        }
        let zone = match self.cloudflare(None, name).await {
            Ok(cloudflare) => Some(cloudflare.zone),
            Err(e) if e.to_string().contains("no Cloudflare zone") => None,
            Err(e) => return Err(e),
        };
        self.cache.zones.lock().unwrap().insert(name.to_owned(), (Instant::now(), zone.clone()));
        Ok(zone)
    }

    async fn cloudflare(&self, zone: Option<&str>, name: &str) -> Result<Cloudflare, BoxError> {
        let mut cloudflare = Cloudflare { http: self.http()?, zone: String::new(), token: self.token().await? };
        cloudflare.zone = match zone {
            Some(zone) => zone.to_owned(),
            None => cloudflare.find_zone(name).await?,
        };
        Ok(cloudflare)
    }

    /// The base domain's Cloudflare zone.
    pub async fn base_zone(&self) -> Result<String, BoxError> {
        match &self.zone {
            Some(zone) => Ok(zone.clone()),
            None => Ok(self.cloudflare(None, &self.base).await?.zone),
        }
    }

    /// The install's public addresses: as configured, else the base domain's wildcard's, asked of
    /// its own nameservers.
    async fn our_addresses(&self, http: &Https) -> Result<Vec<Ipv4Addr>, BoxError> {
        if !self.addresses.is_empty() {
            return Ok(self.addresses.clone());
        }
        if let Some((at, found)) = &*self.cache.addresses.lock().unwrap()
            && at.elapsed() < ADDRESSES_TTL
        {
            return Ok(found.clone());
        }
        let resolved = resolve(http, &format!("forkrail-addresses.{}", self.base)).await?;
        if resolved.v4.is_empty() {
            return Err(format!("the base domain's wildcard, *.{}, has no addresses", self.base).into());
        }
        *self.cache.addresses.lock().unwrap() = Some((Instant::now(), resolved.v4.clone()));
        Ok(resolved.v4)
    }
}

/// How `name` (a custom domain, maybe `*.example.com`) can be proved ours, and what its DNS says.
/// Asks Cloudflare (for a zone, remembered a few minutes unless `fresh`) and the domain's own
/// nameservers; never Let's Encrypt.
pub async fn plan(acme: &Acme, name: &str, fresh: bool) -> Plan {
    let mut findings = DnsFindings::default();
    let wildcard = name.starts_with("*.");
    let bare = name.trim_start_matches("*.");
    let http = match acme.http() {
        Ok(http) => http,
        Err(e) => return Plan { method: None, findings, reason: Some(format!("no HTTPS client: {e}")) },
    };

    // 1. A Cloudflare zone the token edits.
    let cloudflare_error = match acme.zone_of(bare, fresh).await {
        Ok(Some(zone)) => {
            findings.cloudflare = true;
            // The rest is only for the status: what DNS says.
            if let Err(e) = look(acme, &http, name, &mut findings).await {
                warn!(domain = %name, error = %e, "reading the domain's DNS failed");
            }
            return Plan { method: Some(Method::Cloudflare { zone }), findings, reason: None };
        }
        Ok(None) => "no Cloudflare zone".to_owned(),
        Err(e) => e.to_string(),
    };
    let looked = look(acme, &http, name, &mut findings).await;

    // 2. The challenge delegated to the install's zone.
    if findings.delegated {
        return match acme.base_zone().await {
            Ok(zone) => Plan { method: Some(Method::Delegated { zone, target: acme_delegation(name, &acme.base) }), findings, reason: None },
            Err(e) => Plan { method: None, findings, reason: Some(format!("the challenge is delegated, but the install's own zone can't be written: {e}")) },
        };
    }
    // 3. It points here.
    if findings.points_here && !wildcard && acme.http01.is_some() {
        return Plan { method: Some(Method::Http), findings, reason: None };
    }
    let delegate = format!("_acme-challenge.{bare} CNAME {}", acme_delegation(name, &acme.base));
    let reason = match looked {
        Err(e) => format!("its DNS couldn't be read: {e}"),
        Ok(()) if wildcard => format!("a wildcard's certificate needs the record {delegate}"),
        Ok(()) if findings.points_here => format!("it points here, but this router can't answer HTTP challenges; add {delegate}"),
        Ok(()) if findings.proxied => {
            "it's proxied by Cloudflare: set its record to DNS only (the grey cloud), so it reaches the install".to_owned()
        }
        Ok(()) => {
            let found = match (&findings.cname, findings.addresses.is_empty()) {
                (Some(cname), _) => format!("it's a CNAME to {cname}"),
                (None, true) => "it has no address".to_owned(),
                (None, false) => format!("its addresses are {}", findings.addresses.join(", ")),
            };
            format!("DNS doesn't point here yet ({found}); add the records shown, or {delegate} to get the certificate first")
        }
    };
    // No zone, or no token (said once when it was read), is the usual case for a domain elsewhere.
    if !cloudflare_error.contains("no Cloudflare zone") && !cloudflare_error.contains("from SSM") {
        warn!(domain = %name, error = %cloudflare_error, "the Cloudflare zone lookup failed");
    }
    Plan { method: None, findings, reason: Some(reason) }
}

/// Fill in what DNS says of `name`: where it points, and whether its challenge is delegated.
async fn look(acme: &Acme, http: &Https, name: &str, findings: &mut DnsFindings) -> Result<(), BoxError> {
    let bare = name.trim_start_matches("*.");
    let challenge = format!("_acme-challenge.{bare}");
    let expected = acme_delegation(name, &acme.base);
    match first_cname(http, &challenge).await {
        Ok(Some(target)) => findings.delegated = target.eq_ignore_ascii_case(&expected),
        Ok(None) => {}
        Err(e) => warn!(%challenge, error = %e, "looking up the challenge's CNAME failed"),
    }
    // A wildcard points here when a name under it does.
    let probe = if name.starts_with("*.") { format!("forkrail-probe.{bare}") } else { bare.to_owned() };
    let resolved = resolve(http, &probe).await?;
    findings.cname = resolved.cnames.first().cloned();
    findings.addresses = resolved.v4.iter().map(ToString::to_string).chain(resolved.v6.iter().map(ToString::to_string)).collect();
    findings.proxied = cloudflare_proxied(&resolved.v4, &resolved.v6);
    let base = acme.base.as_str();
    let under_base = |n: &str| n == base || n.ends_with(&format!(".{base}"));
    findings.points_here = if resolved.cnames.iter().any(|c| under_base(c)) {
        true
    } else {
        let ours = acme.our_addresses(http).await?;
        // An IPv6 address elsewhere would take Let's Encrypt (which prefers IPv6) elsewhere.
        !resolved.v4.is_empty() && resolved.v4.iter().all(|a| ours.contains(a)) && resolved.v6.is_empty()
    };
    Ok(())
}

/// Cloudflare's proxy's address ranges (cloudflare.com/ips): a record pointing there is proxied,
/// and the install never sees its requests' real names.
const CLOUDFLARE_V4: [(u32, u8); 15] = [
    (0xadf5_3000, 20), // 173.245.48.0/20
    (0x6715_f400, 22), // 103.21.244.0/22
    (0x6716_c800, 22), // 103.22.200.0/22
    (0x671f_0400, 22), // 103.31.4.0/22
    (0x8d65_4000, 18), // 141.101.64.0/18
    (0x6ca2_c000, 18), // 108.162.192.0/18
    (0xbe5d_f000, 20), // 190.93.240.0/20
    (0xbc72_6000, 20), // 188.114.96.0/20
    (0xc5ea_f000, 22), // 197.234.240.0/22
    (0xc629_8000, 17), // 198.41.128.0/17
    (0xa29e_0000, 15), // 162.158.0.0/15
    (0x6810_0000, 13), // 104.16.0.0/13
    (0x6818_0000, 14), // 104.24.0.0/14
    (0xac40_0000, 13), // 172.64.0.0/13
    (0x8300_4800, 22), // 131.0.72.0/22
];
const CLOUDFLARE_V6: [(u128, u8); 7] = [
    (0x2400_cb00 << 96, 32),
    (0x2606_4700 << 96, 32),
    (0x2803_f800 << 96, 32),
    (0x2405_b500 << 96, 32),
    (0x2405_8100 << 96, 32),
    (0x2a06_98c0 << 96, 29),
    (0x2c0f_f248 << 96, 32),
];

/// Whether every address given is Cloudflare's proxy's (and there's at least one).
fn cloudflare_proxied(v4: &[Ipv4Addr], v6: &[Ipv6Addr]) -> bool {
    let in4 = |a: &Ipv4Addr| CLOUDFLARE_V4.iter().any(|(net, bits)| u32::from(*a) >> (32 - bits) == net >> (32 - bits));
    let in6 = |a: &Ipv6Addr| CLOUDFLARE_V6.iter().any(|(net, bits)| u128::from(*a) >> (128 - bits) == net >> (128 - bits));
    (!v4.is_empty() || !v6.is_empty()) && v4.iter().all(in4) && v6.iter().all(in6)
}

/// Get a certificate for `name`: `*.<base>` or a custom domain, proved ours by `method`. `store`
/// keeps the ACME account.
pub async fn issue(acme: &Acme, name: &str, method: &Method, store: &Store) -> Result<Issued, BoxError> {
    let account = account(acme, store).await?;
    match method {
        Method::Cloudflare { zone } => {
            let cloudflare = acme.cloudflare(Some(zone), name).await?;
            dns01(&account, &cloudflare, name, None).await
        }
        Method::Delegated { zone, target } => {
            let cloudflare = acme.cloudflare(Some(zone), &acme.base).await?;
            dns01(&account, &cloudflare, name, Some(target)).await
        }
        Method::Http => {
            let store = acme.http01.as_ref().ok_or("no HTTP-01 challenge store")?;
            http01(&account, store, name).await
        }
    }
}

/// DNS-01 through Cloudflare: the TXT record at `_acme-challenge.<domain>`, or at `target` for a
/// delegated challenge.
async fn dns01(account: &Account, cloudflare: &Cloudflare, name: &str, target: Option<&String>) -> Result<Issued, BoxError> {
    let bare = name.trim_start_matches("*.");
    let record = target.cloned().unwrap_or_else(|| format!("_acme-challenge.{bare}"));
    for id in cloudflare.txt_records(&record).await? {
        warn!(record = %id, "deleting a stale challenge record");
        cloudflare.delete(&id).await?;
    }
    let mut created = Vec::new();
    let result = async {
        let mut order = account.new_order(&NewOrder::new(&[Identifier::Dns(name.to_owned())])).await?;
        {
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authorization = authorization?;
                match authorization.status {
                    AuthorizationStatus::Pending => {}
                    AuthorizationStatus::Valid => continue,
                    status => return Err(format!("authorization is {status:?}").into()),
                }
                let mut challenge =
                    authorization.challenge(ChallengeType::Dns01).ok_or("Let's Encrypt offered no dns-01 challenge")?;
                let value = challenge.key_authorization().dns_value();
                created.push(cloudflare.create_txt(&record, &value).await?);
                let waited = Instant::now();
                wait_for_txt(&cloudflare.http, &record, &value).await?;
                info!(name = %record, propagation_ms = waited.elapsed().as_millis() as u64, "challenge record is visible");
                challenge.set_ready().await?;
            }
        }
        finalize(&mut order, name).await
    }
    .await;
    for id in created {
        if let Err(e) = cloudflare.delete(&id).await {
            warn!(record = %id, error = %e, "deleting the challenge record failed");
        }
    }
    result
}

/// HTTP-01: the key authorization in the challenge store, which every router serves on port 80.
async fn http01(account: &Account, store: &Store, name: &str) -> Result<Issued, BoxError> {
    let mut written = Vec::new();
    let result = async {
        let mut order = account.new_order(&NewOrder::new(&[Identifier::Dns(name.to_owned())])).await?;
        {
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authorization = authorization?;
                match authorization.status {
                    AuthorizationStatus::Pending => {}
                    AuthorizationStatus::Valid => continue,
                    status => return Err(format!("authorization is {status:?}").into()),
                }
                let mut challenge =
                    authorization.challenge(ChallengeType::Http01).ok_or("Let's Encrypt offered no http-01 challenge")?;
                let token = challenge.token.clone();
                if !valid_token(&token) {
                    return Err(format!("Let's Encrypt gave a token this router wouldn't serve: {token:?}").into());
                }
                let key_authorization = challenge.key_authorization().as_str().to_owned();
                store.put(&token, key_authorization.clone().into_bytes()).await?;
                written.push(token.clone());
                self_check(name, &token, &key_authorization).await?;
                challenge.set_ready().await?;
            }
        }
        finalize(&mut order, name).await
    }
    .await;
    for token in written {
        if let Err(e) = store.delete(&token).await {
            warn!(%token, error = %e, "deleting the challenge file failed");
        }
    }
    result
}

/// Fetch the challenge as Let's Encrypt will, over plain HTTP to the domain, before telling it to
/// look: a router that can't answer costs a failed validation.
async fn self_check(name: &str, token: &str, key_authorization: &str) -> Result<(), BoxError> {
    let client: Client<HttpConnector, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let url = format!("http://{name}/.well-known/acme-challenge/{token}");
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let request = Request::get(&url).header("user-agent", "forkrail-acme-self-check").body(Full::new(Bytes::new()))?;
        match tokio::time::timeout(HTTP_TIMEOUT, client.request(request)).await {
            Ok(Ok(response)) => {
                let status = response.status();
                let body = tokio::time::timeout(HTTP_TIMEOUT, response.into_body().collect()).await;
                match body.map(|b| b.map(|b| b.to_bytes())) {
                    Ok(Ok(body)) if status == StatusCode::OK && body == key_authorization.as_bytes() => return Ok(()),
                    _ => last = format!("{url} answered {status}"),
                }
            }
            Ok(Err(e)) => last = format!("{url}: {e}"),
            Err(_) => last = format!("{url}: timed out"),
        }
    }
    Err(format!("the challenge isn't served yet: {last}").into())
}

/// Wait for the order to be ready, finalize it with a fresh key, and fetch the certificate.
async fn finalize(order: &mut instant_acme::Order, name: &str) -> Result<Issued, BoxError> {
    let status = order.poll_ready(&RetryPolicy::new().timeout(Duration::from_secs(120))).await?;
    if status != OrderStatus::Ready {
        return Err(format!("order is {status:?}: {}", describe(&order.state().error)).into());
    }
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()])?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key)?;
    order.finalize_csr(csr.der()).await?;
    let cert_pem = order.poll_certificate(&RetryPolicy::new().timeout(Duration::from_secs(120))).await?;
    Ok(Issued { cert_pem, key_pem: key.serialize_pem() })
}

fn describe(problem: &Option<instant_acme::Problem>) -> String {
    match problem {
        Some(p) => p.detail.clone().unwrap_or_else(|| format!("{p:?}")),
        None => "no detail".to_owned(),
    }
}

/// Whether `token` is what Let's Encrypt gives and the routers serve: base64url, 16-128 long.
pub fn valid_token(token: &str) -> bool {
    (16..=128).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The stored ACME account, or a new one, stored before use.
async fn account(acme: &Acme, store: &Store) -> Result<Account, BoxError> {
    if let Some(bytes) = store.get("account.json").await? {
        let credentials: AccountCredentials = serde_json::from_slice(&bytes)?;
        return Ok(Account::builder()?.from_credentials(credentials).await?);
    }
    let directory = if acme.staging { LetsEncrypt::Staging } else { LetsEncrypt::Production };
    let new = NewAccount { contact: &[], terms_of_service_agreed: true, only_return_existing: false };
    let (account, credentials) = Account::builder()?.create(&new, directory.url().to_owned(), None).await?;
    store.put("account.json", serde_json::to_vec(&credentials)?).await?;
    info!(account = account.id(), staging = acme.staging, "ACME account created");
    Ok(account)
}

async fn read_token(acme: &Acme) -> Result<String, BoxError> {
    let out = acme
        .ssm
        .get_parameter()
        .name(&acme.token_param)
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| format!("reading {} from SSM: {}", acme.token_param, aws_sdk_ssm::error::DisplayErrorContext(&e)))?;
    Ok(out.parameter().and_then(|p| p.value()).ok_or("the parameter has no value")?.to_owned())
}

type Https = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

fn https_client() -> Result<Https, BoxError> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_platform_verifier(rustls::crypto::ring::default_provider())?
        .https_only()
        .enable_http1()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

async fn send(http: &Https, request: Request<Full<Bytes>>) -> Result<(StatusCode, Bytes), BoxError> {
    let what = format!("{} {}", request.method(), request.uri().path());
    let response = tokio::time::timeout(HTTP_TIMEOUT, http.request(request)).await.map_err(|_| format!("{what}: timed out"))??;
    let status = response.status();
    let body = tokio::time::timeout(HTTP_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| format!("{what}: timed out reading the body"))??
        .to_bytes();
    Ok((status, body))
}

struct Cloudflare {
    http: Https,
    zone: String,
    token: String,
}

impl Cloudflare {
    /// The id of the zone holding `name`: the longest of its parent domains (or itself) that's a
    /// zone the token can see.
    async fn find_zone(&self, name: &str) -> Result<String, BoxError> {
        let name = name.trim_start_matches("*.");
        let mut candidate = name;
        loop {
            let found = self.request(HttpMethod::GET, &format!("/zones?name={candidate}"), None).await?;
            if let Some(id) = found["result"].as_array().and_then(|zones| zones.first()).and_then(|z| z["id"].as_str()) {
                info!(%name, zone = %candidate, "found the Cloudflare zone");
                return Ok(id.to_owned());
            }
            match candidate.split_once('.') {
                Some((_, parent)) if parent.contains('.') => candidate = parent,
                _ => {
                    return Err(format!(
                        "no Cloudflare zone for {name} that the token can see; give the token DNS edit on that zone"
                    )
                    .into());
                }
            }
        }
    }

    /// A call on the zone, `path` under `/zones/<zone>`.
    async fn call(&self, method: HttpMethod, path: &str, body: Option<Value>) -> Result<Value, BoxError> {
        self.request(method, &format!("/zones/{}{path}", self.zone), body).await
    }

    async fn request(&self, method: HttpMethod, path: &str, body: Option<Value>) -> Result<Value, BoxError> {
        let request = Request::builder()
            .method(method.clone())
            .uri(format!("{CLOUDFLARE_API}{path}"))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .body(Full::new(body.map_or_else(Bytes::new, |b| Bytes::from(b.to_string()))))?;
        let (status, bytes) = send(&self.http, request).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| format!("Cloudflare {method} {path}: {status}, {e}"))?;
        if !value["success"].as_bool().unwrap_or(false) {
            return Err(format!("Cloudflare {method} {path}: {status}, errors {}", value["errors"]).into());
        }
        Ok(value)
    }

    async fn txt_records(&self, name: &str) -> Result<Vec<String>, BoxError> {
        let found = self.call(HttpMethod::GET, &format!("/dns_records?type=TXT&name={name}"), None).await?;
        Ok(found["result"].as_array().into_iter().flatten().filter_map(|r| r["id"].as_str().map(str::to_owned)).collect())
    }

    async fn create_txt(&self, name: &str, content: &str) -> Result<String, BoxError> {
        let record = json!({"type": "TXT", "name": name, "content": content, "ttl": 60, "comment": "forkrail ACME challenge"});
        let created = self.call(HttpMethod::POST, "/dns_records", Some(record)).await?;
        Ok(created["result"]["id"].as_str().ok_or("Cloudflare returned no record id")?.to_owned())
    }

    async fn delete(&self, id: &str) -> Result<(), BoxError> {
        self.call(HttpMethod::DELETE, &format!("/dns_records/{id}"), None).await.map(drop)
    }
}

/// Ask the authoritative nameservers of the zone holding `name` until every one answers it with
/// the TXT `value`. Never a caching resolver: one that's asked before the record exists caches "no
/// such record" for the zone's negative TTL, 30 minutes on Cloudflare, and the wait times out.
async fn wait_for_txt(http: &Https, name: &str, value: &str) -> Result<(), BoxError> {
    let servers = nameservers(http, name).await?;
    let deadline = tokio::time::Instant::now() + PROPAGATION_TIMEOUT;
    let mut pending = servers.clone();
    loop {
        let mut still = Vec::new();
        for server in pending {
            match query(server, name, TXT).await {
                Ok(answer) if answer.txt(name).iter().any(|v| v == value) => {}
                Ok(_) => still.push(server),
                Err(e) => {
                    warn!(%server, error = %e, "DNS query failed");
                    still.push(server);
                }
            }
        }
        if still.is_empty() {
            return Ok(());
        }
        pending = still;
        if tokio::time::Instant::now() + PROPAGATION_POLL > deadline {
            return Err(format!("{name} wasn't on {pending:?} within {}s", PROPAGATION_TIMEOUT.as_secs()).into());
        }
        tokio::time::sleep(PROPAGATION_POLL).await;
    }
}

/// The addresses of the nameservers of the zone holding `name`: NS from Cloudflare's resolver (an
/// answer that exists, so its caching is harmless), trying parent domains until one has NS, then
/// the system resolver for their addresses.
async fn nameservers(http: &Https, name: &str) -> Result<Vec<SocketAddr>, BoxError> {
    let mut domain = name;
    let names = loop {
        let request = Request::get(format!("{DOH}?name={domain}&type=NS"))
            .header("accept", "application/dns-json")
            .body(Full::new(Bytes::new()))?;
        let (status, body) = send(http, request).await?;
        if !status.is_success() {
            return Err(format!("DNS-over-HTTPS NS query for {domain}: {status}").into());
        }
        let names = answers(&body, NS.into(), domain);
        if !names.is_empty() {
            break names;
        }
        domain = domain.split_once('.').map(|(_, parent)| parent).filter(|p| p.contains('.')).ok_or("no NS records found")?;
    };
    let mut servers = Vec::new();
    for ns in names {
        let ns = ns.trim_end_matches('.').to_owned();
        if let Some(addr) = tokio::net::lookup_host((ns.as_str(), 53)).await?.find(SocketAddr::is_ipv4) {
            servers.push(addr);
        }
    }
    if servers.is_empty() {
        return Err(format!("no addresses for {domain}'s nameservers").into());
    }
    Ok(servers)
}

/// The `data` of records of `rtype` for `name` in a DNS JSON answer. Records for other names, as at
/// the end of a CNAME chain, don't count: `atlas.example.net` isn't a zone because the name it
/// points at has NS records.
fn answers(body: &[u8], rtype: u64, name: &str) -> Vec<String> {
    let Ok(answer) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    let same = |r: &Value| r["name"].as_str().is_some_and(|n| n.trim_end_matches('.').eq_ignore_ascii_case(name.trim_end_matches('.')));
    answer["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["type"].as_u64() == Some(rtype) && same(r))
        .filter_map(|r| r["data"].as_str().map(str::to_owned))
        .collect()
}

/// Ask the zone's nameservers one after another until one answers.
async fn ask(http: &Https, name: &str, rtype: u16) -> Result<Answer, BoxError> {
    let mut last: BoxError = "no nameservers".into();
    for server in nameservers(http, name).await? {
        match query(server, name, rtype).await {
            Ok(answer) => return Ok(answer),
            Err(e) => last = format!("{server}: {e}").into(),
        }
    }
    Err(last)
}

/// The CNAME target `name` has in its own zone, if any.
async fn first_cname(http: &Https, name: &str) -> Result<Option<String>, BoxError> {
    Ok(ask(http, name, CNAME).await?.cname(name))
}

/// What a name resolves to, asked of each zone's own nameservers.
#[derive(Debug, Default)]
struct Resolved {
    /// The CNAMEs on the way, in order.
    cnames: Vec<String>,
    v4: Vec<Ipv4Addr>,
    v6: Vec<Ipv6Addr>,
}

/// Resolve `name` the way Let's Encrypt will, without any cache: CNAMEs followed (at most
/// `MAX_CNAMES`), each asked of the zone that holds it, then the addresses at the end.
async fn resolve(http: &Https, name: &str) -> Result<Resolved, BoxError> {
    let mut resolved = Resolved::default();
    let mut current = name.trim_end_matches('.').to_ascii_lowercase();
    for _ in 0..=MAX_CNAMES {
        let answer = ask(http, &current, A).await?;
        if let Some(target) = answer.cname(&current) {
            resolved.cnames.push(target.clone());
            current = target;
            continue;
        }
        resolved.v4 = answer.v4(&current);
        resolved.v6 = ask(http, &current, AAAA).await?.v6(&current);
        return Ok(resolved);
    }
    Err(format!("{name}: more than {MAX_CNAMES} CNAMEs").into())
}

/// A DNS response's answer section.
#[derive(Debug, Default)]
struct Answer {
    records: Vec<(String, Data)>,
}

#[derive(Debug, PartialEq)]
enum Data {
    Txt(String),
    Cname(String),
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Other,
}

impl Answer {
    fn same(owner: &str, name: &str) -> bool {
        owner.eq_ignore_ascii_case(name.trim_end_matches('.'))
    }

    fn txt(&self, name: &str) -> Vec<String> {
        self.records.iter().filter(|(n, _)| Self::same(n, name)).filter_map(|(_, d)| if let Data::Txt(v) = d { Some(v.clone()) } else { None }).collect()
    }

    fn cname(&self, name: &str) -> Option<String> {
        self.records.iter().filter(|(n, _)| Self::same(n, name)).find_map(|(_, d)| if let Data::Cname(t) = d { Some(t.to_ascii_lowercase()) } else { None })
    }

    fn v4(&self, name: &str) -> Vec<Ipv4Addr> {
        self.records.iter().filter(|(n, _)| Self::same(n, name)).filter_map(|(_, d)| if let Data::A(a) = d { Some(*a) } else { None }).collect()
    }

    fn v6(&self, name: &str) -> Vec<Ipv6Addr> {
        self.records.iter().filter(|(n, _)| Self::same(n, name)).filter_map(|(_, d)| if let Data::Aaaa(a) = d { Some(*a) } else { None }).collect()
    }
}

/// One query over UDP, without recursion, as an authoritative server answers it.
async fn query(server: SocketAddr, name: &str, rtype: u16) -> Result<Answer, BoxError> {
    let id = u16::from_str_radix(&forkrail_common::random_hex(2), 16)?;
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    socket.send_to(&make_query(id, name, rtype)?, server).await?;
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + DNS_TIMEOUT;
    loop {
        let (n, from) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await.map_err(|_| "timed out")??;
        if from == server {
            return Ok(parse_answer(id, &buf[..n])?);
        }
    }
}

fn make_query(id: u16, name: &str, rtype: u16) -> Result<Vec<u8>, String> {
    let mut query = Vec::with_capacity(64);
    query.extend_from_slice(&id.to_be_bytes());
    // No flags (a standard query, recursion not wanted), one question.
    query.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("bad DNS name {name}"));
        }
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&rtype.to_be_bytes());
    query.extend_from_slice(&[0, 1]); // IN
    Ok(query)
}

/// The answer section of a DNS response. NXDOMAIN is no records.
fn parse_answer(id: u16, msg: &[u8]) -> Result<Answer, String> {
    let u16_at = |i: usize| msg.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or("short DNS response");
    if u16_at(0)? != id {
        return Err("DNS response for another query".into());
    }
    let flags = u16_at(2)?;
    if flags & 0x8000 == 0 || flags & 0x0200 != 0 {
        return Err(format!("unusable DNS response (flags {flags:#06x})"));
    }
    match flags & 0xf {
        0 => {}
        3 => return Ok(Answer::default()),
        rcode => return Err(format!("DNS error {rcode}")),
    }
    let (questions, records) = (u16_at(4)?, u16_at(6)?);
    let mut pos = 12;
    for _ in 0..questions {
        pos = read_name(msg, pos)?.1 + 4;
    }
    let mut answer = Answer::default();
    for _ in 0..records {
        let (owner, next) = read_name(msg, pos)?;
        pos = next;
        let (rtype, len) = (u16_at(pos)?, u16_at(pos + 8)? as usize);
        pos += 10;
        let rdata = msg.get(pos..pos + len).ok_or("short DNS record")?;
        let data = match rtype {
            TXT => {
                let (mut value, mut i) = (Vec::new(), 0);
                while i < rdata.len() {
                    let n = rdata[i] as usize;
                    value.extend_from_slice(rdata.get(i + 1..i + 1 + n).ok_or("short TXT string")?);
                    i += 1 + n;
                }
                Data::Txt(String::from_utf8_lossy(&value).into_owned())
            }
            // The target may point back into the message.
            CNAME => Data::Cname(read_name(msg, pos)?.0),
            A if len == 4 => Data::A(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])),
            AAAA if len == 16 => Data::Aaaa(Ipv6Addr::from(<[u8; 16]>::try_from(rdata).expect("16 bytes"))),
            _ => Data::Other,
        };
        pos += len;
        answer.records.push((owner, data));
    }
    Ok(answer)
}

/// The (possibly compressed) name at `pos`, without its trailing dot, and where what follows it
/// starts.
fn read_name(msg: &[u8], start: usize) -> Result<(String, usize), String> {
    let mut labels: Vec<String> = Vec::new();
    let (mut pos, mut end, mut jumps) = (start, None, 0);
    loop {
        let len = *msg.get(pos).ok_or("short DNS name")? as usize;
        match len {
            0 => {
                return Ok((labels.join("."), end.unwrap_or(pos + 1)));
            }
            _ if len & 0xc0 == 0xc0 => {
                let target = (len & 0x3f) << 8 | *msg.get(pos + 1).ok_or("short DNS pointer")? as usize;
                end.get_or_insert(pos + 2);
                jumps += 1;
                if jumps > 32 || target >= msg.len() {
                    return Err("bad DNS name pointer".into());
                }
                pos = target;
            }
            _ if len & 0xc0 != 0 => return Err("bad DNS label".into()),
            _ => {
                let label = msg.get(pos + 1..pos + 1 + len).ok_or("short DNS label")?;
                labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
                pos += 1 + len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ns_answers() {
        let body = br#"{"Status":0,"Answer":[{"name":"forkrail.com","type":2,"TTL":86400,"data":"roxy.ns.cloudflare.com."},
            {"name":"forkrail.com","type":2,"data":"rudy.ns.cloudflare.com."},{"name":"x","type":5,"data":"y."}]}"#;
        assert_eq!(answers(body, 2, "forkrail.com"), ["roxy.ns.cloudflare.com.", "rudy.ns.cloudflare.com."]);
        assert!(answers(b"{\"Status\":3}", 2, "forkrail.com").is_empty());
        assert!(answers(b"not json", 2, "forkrail.com").is_empty());
        // A CNAME to a zone apex: the NS records are the target's, not the name's.
        let chain = br#"{"Status":0,"Answer":[{"name":"atlas.example.net.","type":5,"data":"forkrail.com."},
            {"name":"forkrail.com.","type":2,"data":"roxy.ns.cloudflare.com."}]}"#;
        assert!(answers(chain, 2, "atlas.example.net").is_empty());
        assert_eq!(answers(chain, 2, "forkrail.com.").len(), 1);
    }

    /// A response to `query`: its question, then `records` (rtype, rdata) owned by the question's
    /// name through a pointer to it.
    fn respond(query: &[u8], rcode: u8, records: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut response = query.to_vec();
        response[2..4].copy_from_slice(&[0x84, rcode]); // response, authoritative
        response[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
        for (rtype, rdata) in records {
            response.extend_from_slice(&[0xc0, 12]);
            response.extend_from_slice(&rtype.to_be_bytes());
            response.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
            response.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            response.extend_from_slice(rdata);
        }
        response
    }

    #[test]
    fn txt_over_udp_wire_format() {
        let query = make_query(0xbeef, "_acme-challenge.forkrail.com", TXT).unwrap();
        assert_eq!(&query[..4], &[0xbe, 0xef, 0, 0]);
        assert_eq!(&query[12..28], b"\x0f_acme-challenge");
        assert_eq!(&query[query.len() - 5..], &[0, 0, 16, 0, 1]);
        assert!(make_query(1, "a..b", TXT).is_err());

        // Two TXT answers, one in two strings.
        let rdata = |strings: &[&[u8]]| strings.iter().flat_map(|s| std::iter::once(s.len() as u8).chain(s.iter().copied())).collect::<Vec<u8>>();
        let response = respond(&query, 0, &[(TXT, rdata(&[b"abc-123"])), (TXT, rdata(&[b"part1", b"part2"]))]);
        let answer = parse_answer(0xbeef, &response).unwrap();
        assert_eq!(answer.txt("_acme-challenge.forkrail.com"), ["abc-123", "part1part2"]);
        assert_eq!(answer.txt("_ACME-challenge.forkrail.com."), ["abc-123", "part1part2"]);
        assert!(answer.txt("other.forkrail.com").is_empty());
        assert!(parse_answer(0xbeee, &response).is_err());
        // NXDOMAIN is no values; a truncated answer is an error.
        let nx = respond(&query, 3, &[]);
        assert!(parse_answer(0xbeef, &nx).unwrap().records.is_empty());
        let mut tc = query.clone();
        tc[2..4].copy_from_slice(&[0x86, 0x00]);
        assert!(parse_answer(0xbeef, &tc).is_err());
        assert!(parse_answer(0xbeef, &response[..response.len() - 3]).is_err());
    }

    #[test]
    fn cnames_and_addresses_on_the_wire() {
        let query = make_query(7, "www.example.com", A).unwrap();
        // A CNAME whose target ends in a pointer back to the question's `example.com`.
        let mut target = b"\x04shop\x08forkrail\x03com\x00".to_vec();
        let response = respond(&query, 0, &[(CNAME, target.clone())]);
        let answer = parse_answer(7, &response).unwrap();
        assert_eq!(answer.cname("www.example.com").as_deref(), Some("shop.forkrail.com"));
        target = b"\x03api\xc0\x10".to_vec(); // offset 16: `example.com` in the question
        let answer = parse_answer(7, &respond(&query, 0, &[(CNAME, target)])).unwrap();
        assert_eq!(answer.cname("www.example.com").as_deref(), Some("api.example.com"));
        let answer = parse_answer(7, &respond(&query, 0, &[(A, vec![166, 117, 62, 155]), (AAAA, [0u8; 16].to_vec()), (A, vec![1, 2, 3])])).unwrap();
        assert_eq!(answer.v4("www.example.com"), [Ipv4Addr::new(166, 117, 62, 155)]);
        assert_eq!(answer.v6("www.example.com"), [Ipv6Addr::UNSPECIFIED]);
        // Pointers that loop are refused.
        let mut looped = query.clone();
        looped[2..4].copy_from_slice(&[0x84, 0]);
        looped[6..8].copy_from_slice(&[0, 1]);
        let at = looped.len();
        looped.extend_from_slice(&[0xc0, at as u8]);
        assert!(parse_answer(7, &looped).is_err());
    }

    #[test]
    fn cloudflare_proxy_addresses() {
        let v4 = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert!(cloudflare_proxied(&[v4("104.21.32.1"), v4("172.67.1.2")], &[]));
        assert!(cloudflare_proxied(&[], &["2606:4700:3030::6815:2001".parse().unwrap()]));
        assert!(!cloudflare_proxied(&[v4("203.0.113.10")], &[]));
        assert!(!cloudflare_proxied(&[v4("104.21.32.1"), v4("203.0.113.10")], &[]));
        assert!(!cloudflare_proxied(&[], &[]));
        assert!(cloudflare_proxied(&[v4("198.41.255.255")], &[]) && !cloudflare_proxied(&[v4("198.42.0.0")], &[]));
    }

    #[test]
    fn tokens() {
        assert!(valid_token("LoqXcYV8q5ONbJQxbmR7SCTNo3tiAXDfowyjxAjEuX0"));
        assert!(!valid_token("../../state/certs/x/key.pem"));
        assert!(!valid_token("short"));
        assert!(!valid_token(&"a".repeat(129)));
    }

    /// Against the real nameservers; run with `cargo test -- --ignored` when changing the DNS code.
    /// Needs outbound UDP 53, which some networks block (the router instance allows it).
    #[tokio::test]
    #[ignore]
    async fn live_authoritative_query() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = https_client().unwrap();
        let servers = nameservers(&http, "forkrail.com").await.unwrap();
        assert!(!servers.is_empty());
        for server in servers {
            let values = query(server, "_dmarc.forkrail.com", TXT).await.unwrap().txt("_dmarc.forkrail.com");
            assert!(values.iter().any(|v| v.starts_with("v=DMARC1")), "{server}: {values:?}");
            assert!(query(server, "_nothing-here.forkrail.com", TXT).await.unwrap().txt("_nothing-here.forkrail.com").is_empty());
        }
        let wildcard = resolve(&http, "forkrail-addresses.forkrail.com").await.unwrap();
        assert_eq!(wildcard.v4.len(), 2, "{wildcard:?}");
    }
}
