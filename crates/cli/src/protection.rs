//! Deployment protection's settings (C3): which of a project's hosts need access, and the ways in
//! the routers check (`forkrail_common::protection`, the router's `protection.rs`). They live in
//! the routing table, so a change reaches every router within seconds, with no redeploy.
//!
//! What the routers never see stays with the dashboard: the password itself, the bypass secrets'
//! names, share links. The dashboard signs access tokens with the install's key
//! (`KEY_PARAM`), whose public half `setup` puts in the table.
//!
//! The same ops serve the CLI (`--aws`), the control function (the dashboard) and the dashboard's
//! API (the CLI signed in): `run`.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use forkrail_common::protection::{Protection, ProtectionConfig, ProtectionScope, TrustedIps};
use forkrail_common::routes::{Routes, normalize_host};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::deploy::{Ctx, project_record};
use crate::secrets::Secrets;
use crate::state::ROUTES_KEY;

/// The system env var a project's chosen bypass secret reaches its deployments as: one only
/// protection sets (`deploy::SYSTEM_ENV`).
pub const BYPASS_ENV: &str = crate::deploy::BYPASS_SECRET_ENV;
/// The install's signing key (PKCS #8 PEM), which only the dashboard reads.
pub const KEY_PARAM: &str = "/forkrail/web/protection-key";
/// Bypass secrets a project may have.
const MAX_BYPASS: usize = 10;
/// Revoked share links kept; the oldest go first.
const MAX_REVOKED_SHARES: usize = 1000;

/// What the dashboard, the CLI and the API ask of a project's protection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProtectionOp {
    /// Its settings as the routers apply them, and the install's.
    Get { project: String },
    /// Change some settings: absent leaves one, null clears it.
    Update {
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<ProtectionScope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        members: Option<bool>,
        /// `true` makes a new password version (a new password: earlier access by password ends),
        /// `false` removes the password.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<bool>,
        #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
        trusted_ips: Option<Option<TrustedIps>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options_paths: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exceptions: Option<Vec<String>>,
        /// End every access given so far: everyone signs in again.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        sign_out: bool,
    },
    /// Let automation in with a secret, by its SHA-256 (hex). With `secret` (whose hash it must
    /// be), it also becomes the project's `FORKRAIL_AUTOMATION_BYPASS_SECRET` from its next deploy.
    BypassAdd {
        project: String,
        hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },
    /// Stop a secret working; with `env`, also take `FORKRAIL_AUTOMATION_BYPASS_SECRET` away.
    BypassRemove {
        project: String,
        hash: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        env: bool,
    },
    /// Take a share link back: its token stops working at once.
    ShareRevoke { project: String, id: String },
    /// The scope of projects with no settings of their own.
    Default { scope: ProtectionScope },
}

/// A field that may be absent (`None`), null (`Some(None)`) or set.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

impl ProtectionOp {
    pub fn name(&self) -> &'static str {
        match self {
            ProtectionOp::Get { .. } => "get",
            ProtectionOp::Update { .. } => "update",
            ProtectionOp::BypassAdd { .. } => "bypass_add",
            ProtectionOp::BypassRemove { .. } => "bypass_remove",
            ProtectionOp::ShareRevoke { .. } => "share_revoke",
            ProtectionOp::Default { .. } => "default",
        }
    }

    pub fn changes(&self) -> bool {
        !matches!(self, ProtectionOp::Get { .. })
    }

    pub fn project(&self) -> Option<&str> {
        match self {
            ProtectionOp::Get { project }
            | ProtectionOp::Update { project, .. }
            | ProtectionOp::BypassAdd { project, .. }
            | ProtectionOp::BypassRemove { project, .. }
            | ProtectionOp::ShareRevoke { project, .. } => Some(project),
            ProtectionOp::Default { .. } => None,
        }
    }
}

/// A project's protection as the dashboard and `forkrail protection` show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub project: String,
    /// What applies: its own settings, or the install's default.
    pub protection: Protection,
    /// Whether it has settings of its own.
    pub own: bool,
    /// The install's default scope.
    pub default: ProtectionScope,
    /// The dashboard's address browsers are sent to, and whether the routers can check tokens:
    /// without both, protected hosts answer 401 to everyone not let in another way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    pub ready: bool,
}

pub async fn run(ctx: &Ctx, op: ProtectionOp) -> Result<Value> {
    if let Some(project) = op.project() {
        project_record(ctx, project).await?;
    }
    Ok(match op {
        ProtectionOp::Get { project } => {
            let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
            json!(view(&routes, &project))
        }
        ProtectionOp::Update { project, scope, members, password, trusted_ips, options_paths, exceptions, sign_out } => {
            let exceptions = exceptions.map(|e| e.iter().map(|h| normalize_host(h)).collect::<Vec<_>>());
            change(ctx, &project, |routes, p| {
                if let Some(scope) = scope {
                    p.scope = scope;
                }
                if let Some(members) = members {
                    p.members = members;
                }
                match password {
                    Some(true) => p.password_version = Some(p.password_version.unwrap_or(0) + 1),
                    Some(false) => p.password_version = None,
                    None => {}
                }
                if let Some(t) = trusted_ips.clone() {
                    p.trusted_ips = t;
                }
                if let Some(paths) = options_paths.clone() {
                    p.options_paths = paths;
                }
                if let Some(hosts) = exceptions.clone() {
                    if let Some(h) = hosts.iter().find(|h| !host_of(routes, h, &project)) {
                        bail!("{h} isn't one of {project}'s hosts");
                    }
                    p.exceptions = hosts;
                }
                if sign_out {
                    p.revoked_before = Some(crate::state::now_ms() / 1000);
                }
                Ok(())
            })
            .await?
        }
        ProtectionOp::BypassAdd { project, hash, secret } => {
            let hash = hash.to_ascii_lowercase();
            if let Some(secret) = &secret
                && forkrail_common::hex(&Sha256::digest(secret.as_bytes())) != hash
            {
                bail!("the secret isn't the one the hash is of");
            }
            let view = change(ctx, &project, |_, p| {
                if !p.bypass.contains(&hash) {
                    if p.bypass.len() >= MAX_BYPASS {
                        bail!("{project} has {MAX_BYPASS} bypass secrets already: remove one first");
                    }
                    p.bypass.push(hash.clone());
                }
                Ok(())
            })
            .await?;
            if let Some(secret) = secret {
                let values = [(BYPASS_ENV.to_owned(), secret)].into();
                crate::env::set_system(ctx, &project, &values, &[crate::state::EnvTarget::Production, crate::state::EnvTarget::Preview]).await?;
            }
            view
        }
        ProtectionOp::BypassRemove { project, hash, env } => {
            let hash = hash.to_ascii_lowercase();
            let view = change(ctx, &project, |_, p| {
                p.bypass.retain(|h| *h != hash);
                Ok(())
            })
            .await?;
            if env {
                crate::env::remove(ctx, &project, BYPASS_ENV, &[crate::state::EnvTarget::Production, crate::state::EnvTarget::Preview], None).await?;
            }
            view
        }
        ProtectionOp::ShareRevoke { project, id } => {
            if id.is_empty() || id.len() > 64 {
                bail!("{id:?} isn't a share link's id");
            }
            change(ctx, &project, |_, p| {
                if !p.revoked_shares.contains(&id) {
                    // Full: every share link and sign-in made before now is refused instead, and the
                    // list starts again. Dropping the oldest ids would bring those links back.
                    if p.revoked_shares.len() >= MAX_REVOKED_SHARES {
                        p.revoked_before = Some(crate::state::now_ms() / 1000);
                        p.revoked_shares.clear();
                    }
                    p.revoked_shares.push(id.clone());
                }
                Ok(())
            })
            .await?
        }
        ProtectionOp::Default { scope } => {
            let routes = ctx
                .store
                .update::<Routes, _>(ROUTES_KEY, |routes| {
                    let mut routes = routes.unwrap_or_default();
                    routes.protection.get_or_insert_with(ProtectionConfig::default).default = scope;
                    routes.validate().map_err(anyhow::Error::msg)?;
                    Ok(routes)
                })
                .await?;
            json!({"default": routes.protection.map(|p| p.default)})
        }
    })
}

/// Change `project`'s settings (starting from the install's default when it has none of its
/// own), and write the table.
async fn change(ctx: &Ctx, project: &str, mut f: impl FnMut(&Routes, &mut Protection) -> Result<()>) -> Result<Value> {
    let routes = ctx
        .store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            let mut p = effective(&routes, project).0;
            f(&routes, &mut p)?;
            p.validate().map_err(anyhow::Error::msg)?;
            routes.projects.entry(project.to_owned()).or_default().protection = Some(p);
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    Ok(json!(view(&routes, project)))
}

/// `project`'s settings as the routers apply them, and whether they're its own.
fn effective(routes: &Routes, project: &str) -> (Protection, bool) {
    match routes.projects.get(project).and_then(|p| p.protection.clone()) {
        Some(p) => (p, true),
        None => {
            let default = routes.protection.as_ref().map_or(ProtectionScope::Off, |c| c.default);
            (Protection { scope: default, ..Protection::default() }, false)
        }
    }
}

pub fn view(routes: &Routes, project: &str) -> View {
    let (protection, own) = effective(routes, project);
    let config = routes.protection.clone().unwrap_or_default();
    View {
        project: project.to_owned(),
        protection,
        own,
        default: config.default,
        ready: !config.keys.is_empty() && config.login.is_some(),
        login: config.login,
    }
}

/// Whether `host` is one of `project`'s: a host of one of its deployments, or a custom domain of it.
fn host_of(routes: &Routes, host: &str, project: &str) -> bool {
    routes.lookup(host).is_some_and(|(_, d)| d.project == project) || routes.domain(host).is_some_and(|(_, d)| d.project == project)
}

/// Give the install a signing key, or a new one with `rotate`, and say where browsers go for
/// access (`login`, the dashboard's origin). The table gets the public key first, so the routers
/// know it before the dashboard signs with it; the key before stays in it, so access it gave
/// keeps working (a sign-in's for its 12 h; share links signed with it until the next rotation).
pub async fn setup(ctx: &Ctx, login: Option<String>, rotate: bool) -> Result<Value> {
    let secrets = Secrets::new(ctx);
    let existing = secrets.get_secret(KEY_PARAM).await?;
    let (public, new_pem) = match existing.as_deref() {
        Some(pem) if !rotate => (public_of(pem)?, None),
        _ => {
            let (pem, public) = generate()?;
            (public, Some(pem))
        }
    };
    let routes = ctx
        .store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            let config = routes.protection.get_or_insert_with(ProtectionConfig::default);
            if !config.keys.contains(&public) {
                config.keys.insert(0, public.clone());
                config.keys.truncate(2);
            }
            if let Some(login) = &login {
                config.login = Some(login.trim_end_matches('/').to_owned());
            }
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    if let Some(pem) = new_pem {
        secrets.put_install(KEY_PARAM, &pem, true, "forkrail: deployment protection's signing key (the dashboard's)").await?;
    }
    let config = routes.protection.unwrap_or_default();
    Ok(json!({"keys": config.keys.len(), "login": config.login, "default": config.default, "rotated": rotate || existing.is_none()}))
}

/// A new Ed25519 key: its PKCS #8 v1 PEM (what Node's crypto reads) and its public key in
/// base64url.
fn generate() -> Result<(String, String)> {
    let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| anyhow::anyhow!("no randomness for a key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| anyhow::anyhow!("the new key didn't read back"))?;
    // ring writes v2 (with the public key); the seed is the 32 bytes at 16.
    let seed = &doc.as_ref()[16..48];
    Ok((pem_v1(seed), URL_SAFE_NO_PAD.encode(pair.public_key().as_ref())))
}

/// PKCS #8 v1 for an Ed25519 seed.
fn pem_v1(seed: &[u8]) -> String {
    const PREFIX: [u8; 16] = [0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];
    let der: Vec<u8> = PREFIX.iter().chain(seed).copied().collect();
    format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", STANDARD.encode(der))
}

/// The public key (base64url) of a key `generate` made.
fn public_of(pem: &str) -> Result<String> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = STANDARD.decode(body.trim()).context("the signing key isn't PEM")?;
    if der.len() != 48 {
        bail!("the signing key isn't an Ed25519 PKCS #8 key");
    }
    let pair = Ed25519KeyPair::from_seed_unchecked(&der[16..]).map_err(|_| anyhow::anyhow!("the signing key doesn't read"))?;
    Ok(URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()))
}

/// A team member's access token for `host`, for an hour: what `forkrail curl` sends, made with
/// the install's own key (AWS credentials that may read it; signed in, the dashboard makes it).
pub async fn member_token(ctx: &Ctx, host: &str) -> Result<String> {
    let host = normalize_host(host);
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let project = project_of(&routes, &host).with_context(|| format!("{host} isn't a host of this install"))?;
    let pem = Secrets::new(ctx).get_secret(KEY_PARAM).await?.context("no signing key yet: `forkrail --aws protection setup`")?;
    let now = crate::state::now_ms() / 1000;
    sign(&pem, &json!({"aud": host, "project": project, "via": "member", "sub": "cli", "iat": now, "exp": now + 3600}))
}

/// The project serving `host`.
pub fn project_of(routes: &Routes, host: &str) -> Option<String> {
    routes.lookup(host).map(|(_, d)| d.project.clone()).or_else(|| routes.domain(host).map(|(_, d)| d.project.clone()))
}

/// A JWT of `claims`, signed with the key in `pem` (EdDSA).
fn sign(pem: &str, claims: &Value) -> Result<String> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = STANDARD.decode(body.trim()).context("the signing key isn't PEM")?;
    if der.len() != 48 {
        bail!("the signing key isn't an Ed25519 PKCS #8 key");
    }
    let pair = Ed25519KeyPair::from_seed_unchecked(&der[16..]).map_err(|_| anyhow::anyhow!("the signing key doesn't read"))?;
    let signed = format!("{}.{}", URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#), URL_SAFE_NO_PAD.encode(claims.to_string()));
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(pair.sign(signed.as_bytes()))))
}

/// Print a view for `forkrail protection`.
pub fn print_view(v: &View) {
    let p = &v.protection;
    let scope = |s: ProtectionScope| match s {
        ProtectionScope::Standard => "standard (every generated URL; production names open)",
        ProtectionScope::All => "all (every URL)",
        ProtectionScope::Off => "off",
    };
    println!("{}: {}{}", v.project, scope(p.scope), if v.own { "" } else { "  (the install's default)" });
    println!("  team sign-in   {}", if p.members { "on" } else { "off" });
    println!("  password       {}", if p.password_version.is_some() { "on" } else { "off" });
    if let Some(t) = &p.trusted_ips {
        println!("  trusted IPs    {:?}: {}", t.scope, t.addresses.join(", "));
    }
    if !p.options_paths.is_empty() {
        println!("  OPTIONS open   {}", p.options_paths.join(", "));
    }
    if !p.exceptions.is_empty() {
        println!("  public hosts   {}", p.exceptions.join(", "));
    }
    println!("  bypass secrets {}", p.bypass.len());
    if !v.ready {
        println!("\n  not set up: run `forkrail --aws protection setup --login https://<dashboard>` (until then a protected URL lets in only bypass secrets and trusted ways)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        let (pem, public) = generate().unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEI"), "{pem}");
        assert_eq!(public_of(&pem).unwrap(), public);
        assert_eq!(public.len(), 43);
        // RFC 8032's first test vector: seed 9d61…7f60 has public key d75a…511a.
        let seed: Vec<u8> = (0..32).map(|i| u8::from_str_radix(&"9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"[i * 2..i * 2 + 2], 16).unwrap()).collect();
        let public = public_of(&pem_v1(&seed)).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(public).unwrap(), (0..32).map(|i| u8::from_str_radix(&"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"[i * 2..i * 2 + 2], 16).unwrap()).collect::<Vec<_>>());
    }

    #[test]
    fn ops_read_as_the_dashboard_sends_them() {
        let op: ProtectionOp = serde_json::from_str(r#"{"op":"update","project":"shop","scope":"all","trusted_ips":null,"sign_out":true}"#).unwrap();
        let ProtectionOp::Update { scope, trusted_ips, sign_out, password, .. } = op else { panic!() };
        assert_eq!((scope, trusted_ips, sign_out, password), (Some(ProtectionScope::All), Some(None), true, None));
        let op: ProtectionOp = serde_json::from_str(r#"{"op":"update","project":"shop"}"#).unwrap();
        assert!(matches!(op, ProtectionOp::Update { trusted_ips: None, .. }));
        assert!(!ProtectionOp::Get { project: "x".into() }.changes());
    }

    #[test]
    fn views_fall_back_to_the_default() {
        let mut routes = Routes::default();
        let v = view(&routes, "shop");
        assert_eq!((v.protection.scope, v.own, v.ready), (ProtectionScope::Off, false, false));
        routes.protection = Some(ProtectionConfig { keys: vec!["k".into()], login: Some("https://d".into()), default: ProtectionScope::Standard });
        let v = view(&routes, "shop");
        assert_eq!((v.protection.scope, v.own, v.ready), (ProtectionScope::Standard, false, true));
    }
}
