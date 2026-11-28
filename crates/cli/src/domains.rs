//! Custom domains: `atlas.example.net` serving a project's production deployment, a branch's
//! newest preview, or redirecting to another of the project's domains. `*.example.com` covers one
//! label below it.
//!
//! A domain is an entry in the routing table's `domains`, plus a host pointing at the deployment
//! it serves (none for a redirect). Deploying with `--prod`, `promote` and `rollback` move the
//! production ones with `<project>.<base>`; a push to a branch moves that branch's (publish.rs).
//!
//! The issuing router sees the entry and gets the domain a certificate from Let's Encrypt, proving
//! it one of three ways, the first that works (see the router's `acme.rs`):
//! 1. a Cloudflare zone the install's token edits holds the domain: a TXT record there;
//! 2. `_acme-challenge.<domain>` is a CNAME to `routes::acme_delegation`, in the install's own
//!    zone: a TXT record there. This works before the domain points here, so a domain moved from
//!    elsewhere has its certificate before DNS moves, and it's the only way for a wildcard;
//! 3. the domain points here: HTTP-01, answered by every router on port 80.
//!
//! It says how that went in `state/certs/<domain>/status.json`. `check` asks it to look again now.
//!
//! The same ops serve the CLI (`--aws`), the control function (the dashboard) and the dashboard's
//! API (the CLI signed in): `run`.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use forkrail_common::domains::{CertState, CertStatus};
use forkrail_common::install::{cert_key, cert_status_key};
use forkrail_common::routes::{
    CustomDomain, DOMAIN_REDIRECT_STATUSES, DomainRedirect, Routes, acme_delegation, normalize_host, valid_domain_entry,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::deploy::{Ctx, RESERVED_NAMES, branch_host, check_project, project_record};
use crate::state::{self, ROUTES_KEY, Settings};

/// What the dashboard, the CLI and the API ask of the install's custom domains.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DomainsOp {
    /// Every custom domain, or `project`'s, with how each stands.
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// One domain: how it stands, and the DNS records it needs.
    Inspect { domain: String },
    /// Give `project` a domain: it serves production, `branch`'s newest preview, or redirects.
    Add {
        domain: String,
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redirect: Option<DomainRedirect>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    Remove { domain: String },
    /// Change what a domain does: absent leaves it, null clears it (back to production).
    Update {
        domain: String,
        #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
        redirect: Option<Option<DomainRedirect>>,
        #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
        branch: Option<Option<String>>,
    },
    /// Have the issuing router look at its DNS and certificate again now.
    Check { domain: String },
}

/// A field that may be absent (`None`), null (`Some(None)`) or set.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

impl DomainsOp {
    pub fn name(&self) -> &'static str {
        match self {
            DomainsOp::List { .. } => "list",
            DomainsOp::Inspect { .. } => "inspect",
            DomainsOp::Add { .. } => "add",
            DomainsOp::Remove { .. } => "remove",
            DomainsOp::Update { .. } => "update",
            DomainsOp::Check { .. } => "check",
        }
    }

    /// Whether it changes the routing table. `check` does, only to say when it was asked.
    pub fn changes(&self) -> bool {
        !matches!(self, DomainsOp::List { .. } | DomainsOp::Inspect { .. })
    }

    /// The domain it names, for the log.
    pub fn domain(&self) -> Option<&str> {
        match self {
            DomainsOp::List { .. } => None,
            DomainsOp::Inspect { domain }
            | DomainsOp::Add { domain, .. }
            | DomainsOp::Remove { domain }
            | DomainsOp::Update { domain, .. }
            | DomainsOp::Check { domain } => Some(domain),
        }
    }

    /// The project it names, when it names one.
    pub fn project(&self) -> Option<&str> {
        match self {
            DomainsOp::List { project } => project.as_deref(),
            DomainsOp::Add { project, .. } => Some(project),
            _ => None,
        }
    }
}

/// How a domain stands, for the dashboard and `domains ls`/`inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub name: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<DomainRedirect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    /// The deployment it serves now: none for a redirect, or a branch with no ready preview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serves: Option<String>,
    /// The issuing router's last word on its certificate; none before it has looked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate: Option<CertStatus>,
    /// Whether a certificate is stored, for domains the issuer hasn't written a status for.
    #[serde(default)]
    pub has_certificate: bool,
    /// What to add at the domain's DNS provider.
    pub records: Vec<Record>,
}

/// A DNS record a domain needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// `A` or `CNAME`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The full name, and the same relative to the registered domain (`@` for itself), as most
    /// DNS providers ask for it.
    pub fqdn: String,
    pub name: String,
    pub value: String,
    /// `serve`: what points the domain here. `certificate`: what lets the certificate be issued
    /// before that, or at all for a wildcard.
    pub purpose: String,
    /// Without it the domain can't work (a wildcard's challenge, any domain's serving record); a
    /// non-wildcard's `certificate` record is optional.
    pub required: bool,
}

pub async fn run(ctx: &Ctx, op: DomainsOp, actor: Option<&str>) -> Result<Value> {
    Ok(match op {
        DomainsOp::List { project } => {
            if let Some(project) = &project {
                check_project(project)?;
            }
            json!({ "domains": list(ctx, project.as_deref()).await? })
        }
        DomainsOp::Inspect { domain } => json!(view(ctx, &normalize_host(&domain)).await?),
        DomainsOp::Add { domain, project, redirect, branch } => json!(add(ctx, &domain, &project, redirect, branch, actor).await?),
        DomainsOp::Remove { domain } => json!(remove(ctx, &domain).await?),
        DomainsOp::Update { domain, redirect, branch } => json!(update(ctx, &domain, redirect, branch).await?),
        DomainsOp::Check { domain } => json!(check(ctx, &domain).await?),
    })
}

pub async fn add(
    ctx: &Ctx,
    domain: &str,
    project: &str,
    redirect: Option<DomainRedirect>,
    branch: Option<String>,
    actor: Option<&str>,
) -> Result<View> {
    let domain = normalize_host(domain);
    let settings = ctx.store.settings().await?;
    check_project(project)?;
    let p = project_record(ctx, project).await?;
    check_domain(&domain, &settings.base_domain, p.display_name.as_deref())?;
    let branch = branch_of(&p, branch)?;
    if redirect.is_some() && branch.is_some() {
        bail!("a domain either redirects or follows a branch, not both");
    }
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            if let Some(d) = routes.domains.get(&domain) {
                if d.project != project {
                    bail!("{domain} already belongs to {}; remove it there first", d.project);
                }
                bail!("{domain} is already {project}'s: change it instead of adding it");
            }
            if let Some(id) = routes.hosts.get(&domain)
                && let Some(d) = routes.deployments.get(id)
                && d.project != project
            {
                bail!("{domain} already serves deployment {id} of {}", d.project);
            }
            if let Some(r) = &redirect {
                check_redirect(&routes, &domain, project, r, &settings)?;
            }
            let entry = CustomDomain {
                project: project.to_owned(),
                added_at: Some(state::now_rfc3339()),
                added_by: actor.map(str::to_owned),
                redirect: redirect.clone(),
                branch: branch.clone(),
                checked_at: None,
            };
            routes.domains.insert(domain.clone(), entry);
            repoint(&mut routes, &domain, p.production.as_deref(), &settings)?;
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    view(ctx, &domain).await
}

pub async fn remove(ctx: &Ctx, domain: &str) -> Result<View> {
    let domain = normalize_host(domain);
    let before = view(ctx, &domain).await?;
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            if !routes.domains.contains_key(&domain) {
                bail!("{domain} isn't a custom domain");
            }
            let pointing: Vec<&str> =
                routes.domains.iter().filter(|(_, d)| d.redirect.as_ref().is_some_and(|r| r.to == domain)).map(|(n, _)| n.as_str()).collect();
            if !pointing.is_empty() {
                bail!("{} redirect{} to {domain}: change or remove {} first", pointing.join(", "), if pointing.len() == 1 { "s" } else { "" }, if pointing.len() == 1 { "it" } else { "them" });
            }
            routes.domains.remove(&domain);
            routes.hosts.remove(&domain);
            Ok(routes)
        })
        .await?;
    Ok(before)
}

pub async fn update(ctx: &Ctx, domain: &str, redirect: Option<Option<DomainRedirect>>, branch: Option<Option<String>>) -> Result<View> {
    let domain = normalize_host(domain);
    let settings = ctx.store.settings().await?;
    let project = {
        let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
        match routes.domains.get(&domain) {
            Some(d) => d.project.clone(),
            None => bail!("{domain} isn't a custom domain"),
        }
    };
    let p = project_record(ctx, &project).await?;
    let branch = match branch {
        Some(b) => Some(branch_of(&p, b)?),
        None => None,
    };
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            let Some(mut entry) = routes.domains.get(&domain).cloned() else { bail!("{domain} isn't a custom domain") };
            if entry.project != project {
                bail!("{domain} moved to {} meanwhile: try again", entry.project);
            }
            if let Some(redirect) = &redirect {
                entry.redirect = redirect.clone();
                // A redirect follows no branch.
                if redirect.is_some() {
                    entry.branch = None;
                }
            }
            if let Some(branch) = &branch {
                entry.branch = branch.clone();
                if branch.is_some() {
                    entry.redirect = None;
                }
            }
            if redirect.as_ref().is_some_and(Option::is_some) && branch.as_ref().is_some_and(Option::is_some) {
                bail!("a domain either redirects or follows a branch, not both");
            }
            if let Some(r) = &entry.redirect {
                check_redirect(&routes, &domain, &project, r, &settings)?;
                let from: Vec<&str> =
                    routes.domains.iter().filter(|(_, d)| d.redirect.as_ref().is_some_and(|r| r.to == domain)).map(|(n, _)| n.as_str()).collect();
                if !from.is_empty() {
                    bail!("{} redirect{} to {domain}, so it can't redirect itself", from.join(", "), if from.len() == 1 { "s" } else { "" });
                }
            }
            routes.domains.insert(domain.clone(), entry);
            repoint(&mut routes, &domain, p.production.as_deref(), &settings)?;
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    view(ctx, &domain).await
}

pub async fn check(ctx: &Ctx, domain: &str) -> Result<View> {
    let domain = normalize_host(domain);
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            let Some(entry) = routes.domains.get_mut(&domain) else { bail!("{domain} isn't a custom domain") };
            entry.checked_at = Some(state::now_rfc3339());
            Ok(routes)
        })
        .await?;
    view(ctx, &domain).await
}

pub async fn list(ctx: &Ctx, project: Option<&str>) -> Result<Vec<View>> {
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let settings = ctx.store.settings().await?;
    let addresses = addresses(&settings).await;
    let names: Vec<&String> = routes.domains.iter().filter(|(_, d)| project.is_none_or(|p| d.project == p)).map(|(n, _)| n).collect();
    let mut views = Vec::new();
    // 16 at a time: each reads two small objects.
    for chunk in names.chunks(16) {
        let found = futures_util::future::join_all(chunk.iter().map(|name| status_of(ctx, name))).await;
        for (name, (status, has)) in chunk.iter().zip(found) {
            views.push(view_of(&routes, name, status?, has?, &settings, &addresses));
        }
    }
    Ok(views)
}

pub async fn view(ctx: &Ctx, domain: &str) -> Result<View> {
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    if !routes.domains.contains_key(domain) {
        bail!("{domain} isn't a custom domain");
    }
    let settings = ctx.store.settings().await?;
    let (status, has) = status_of(ctx, domain).await;
    Ok(view_of(&routes, domain, status?, has?, &settings, &addresses(&settings).await))
}

/// The issuer's status for a domain, and whether a certificate is stored.
async fn status_of(ctx: &Ctx, domain: &str) -> (Result<Option<CertStatus>>, Result<bool>) {
    let status = async {
        Ok(match ctx.store.get_bytes(&cert_status_key(domain)).await? {
            Some(bytes) => serde_json::from_slice(&bytes).ok(),
            None => None,
        })
    };
    let key = cert_key(domain);
    tokio::join!(status, ctx.store.exists(&key))
}

fn view_of(routes: &Routes, name: &str, certificate: Option<CertStatus>, has_certificate: bool, settings: &Settings, addresses: &[String]) -> View {
    let entry = routes.domains.get(name).cloned().unwrap_or_default();
    View {
        name: name.to_owned(),
        project: entry.project.clone(),
        added_at: entry.added_at.clone(),
        added_by: entry.added_by.clone(),
        redirect: entry.redirect.clone(),
        branch: entry.branch.clone(),
        checked_at: entry.checked_at.clone(),
        serves: routes.hosts.get(name).cloned(),
        certificate,
        has_certificate,
        records: records(name, &entry.project, &settings.base_domain, addresses),
    }
}

/// The records `domain` needs: what points it here (A records at the install's addresses for a
/// registered domain itself, a CNAME for a name under one), and the challenge's CNAME, which a
/// wildcard can't do without.
pub fn records(domain: &str, project: &str, base: &str, addresses: &[String]) -> Vec<Record> {
    let wildcard = domain.starts_with("*.");
    let bare = domain.trim_start_matches("*.");
    let registered = registered_domain(bare);
    let relative = |fqdn: &str| match fqdn.strip_suffix(&registered).map(|r| r.trim_end_matches('.')) {
        Some("") => "@".to_owned(),
        Some(rest) => rest.to_owned(),
        None => fqdn.to_owned(),
    };
    let mut out = Vec::new();
    if !wildcard && bare == registered {
        for address in addresses {
            out.push(Record { kind: "A".into(), fqdn: domain.to_owned(), name: "@".into(), value: address.clone(), purpose: "serve".into(), required: true });
        }
    } else {
        out.push(Record { kind: "CNAME".into(), fqdn: domain.to_owned(), name: relative(domain), value: format!("{project}.{base}"), purpose: "serve".into(), required: true });
    }
    let challenge = format!("_acme-challenge.{bare}");
    out.push(Record {
        kind: "CNAME".into(),
        name: relative(&challenge),
        fqdn: challenge,
        value: acme_delegation(domain, base),
        purpose: "certificate".into(),
        required: wildcard,
    });
    out
}

/// The registered domain holding `name` (`example.co.uk` for `www.example.co.uk`), by the public
/// suffix list; `name` itself when the list doesn't know its suffix.
pub fn registered_domain(name: &str) -> String {
    psl::domain_str(name).unwrap_or(name).to_owned()
}

/// The install's public addresses, for a registered domain's A records: the settings' (the
/// accelerator's), else what the base domain's wildcard resolves to.
pub async fn addresses(settings: &Settings) -> Vec<String> {
    if !settings.addresses.is_empty() {
        return settings.addresses.clone();
    }
    let probe = format!("forkrail-addresses.{}:443", settings.base_domain);
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), tokio::net::lookup_host(probe)).await;
    let mut out: Vec<String> = match found {
        Ok(Ok(addrs)) => addrs.filter(|a| a.is_ipv4()).map(|a| a.ip().to_string()).collect::<BTreeSet<_>>().into_iter().collect(),
        _ => Vec::new(),
    };
    out.sort();
    out
}

/// Point `domain`'s host at what it serves: production (`production`), its branch's newest ready
/// preview (whatever holds the branch's host), or nothing for a redirect. A branch with no ready
/// preview yet serves nothing until one is published.
fn repoint(routes: &mut Routes, domain: &str, production: Option<&str>, settings: &Settings) -> Result<()> {
    let Some(entry) = routes.domains.get(domain).cloned() else { return Ok(()) };
    let target = if entry.redirect.is_some() {
        None
    } else if let Some(branch) = &entry.branch {
        let host = branch_host(&entry.project, branch, &settings.base_domain);
        routes.hosts.get(&host).cloned()
    } else {
        match production {
            Some(id) if !routes.deployments.contains_key(id) => bail!("{}'s production deployment {id} isn't in the routing table", entry.project),
            other => other.map(str::to_owned),
        }
    };
    match target {
        Some(id) => routes.hosts.insert(domain.to_owned(), id),
        None => routes.hosts.remove(domain),
    };
    Ok(())
}

/// Point the project's domains that follow `branch` at deployment `id`, its newest ready preview.
pub fn point_branch_domains(routes: &mut Routes, project: &str, branch: &str, id: &str) {
    let names: Vec<String> = routes
        .domains
        .iter()
        .filter(|(_, d)| d.project == project && d.redirect.is_none() && d.branch.as_deref() == Some(branch))
        .map(|(n, _)| n.clone())
        .collect();
    for name in names {
        routes.hosts.insert(name, id.to_owned());
    }
}

/// A domain's branch as given: none, or none for the production branch (whose domains are
/// production's), or one of a project connected to a repository.
fn branch_of(p: &state::Project, branch: Option<String>) -> Result<Option<String>> {
    let Some(branch) = branch.map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()) else { return Ok(None) };
    let Some(link) = &p.git else {
        bail!("{} isn't connected to a repository, so it has no branch previews to follow: connect one first", p.name);
    };
    if branch == link.production_branch {
        return Ok(None);
    }
    crate::connect::check_branch(&branch)?;
    Ok(Some(branch))
}

/// A redirect goes to another of the project's own domains that serves (not a redirect itself),
/// or to `<project>.<base>`, with one of Vercel's four statuses.
fn check_redirect(routes: &Routes, domain: &str, project: &str, r: &DomainRedirect, settings: &Settings) -> Result<()> {
    if !DOMAIN_REDIRECT_STATUSES.contains(&r.status) {
        bail!("a domain redirects with 301, 302, 307 or 308, not {}", r.status);
    }
    let to = normalize_host(&r.to);
    if to != r.to {
        bail!("redirect to {to:?}, written as a domain name in lowercase");
    }
    if to == domain {
        bail!("{domain} can't redirect to itself");
    }
    if to == format!("{project}.{}", settings.base_domain) {
        return Ok(());
    }
    match routes.domains.get(&to) {
        Some(d) if d.project != project => bail!("{to} is {}'s, not {project}'s", d.project),
        Some(d) if d.redirect.is_some() => bail!("{to} redirects itself: redirect to where it goes instead"),
        Some(_) if to.starts_with("*.") => bail!("a redirect goes to one domain, not a wildcard"),
        Some(_) => Ok(()),
        None => bail!("{to} isn't one of {project}'s domains: add it first, or redirect to {project}.{}", settings.base_domain),
    }
}

/// A custom domain is a DNS name outside the base domain: hosts under it are projects' own. A
/// renamed project may also take `<its name>.<base>` (`renamed_to`): no other project can be called
/// that.
fn check_domain(domain: &str, base: &str, renamed_to: Option<&str>) -> Result<()> {
    if !valid_domain_entry(domain) {
        bail!("{domain:?} isn't a domain name like api.example.com or *.example.com");
    }
    let bare = domain.strip_prefix("*.").unwrap_or(domain);
    if psl::suffix_str(bare) == Some(bare) {
        bail!("{bare} is a public suffix (like co.uk), not a domain anyone registers");
    }
    // The names kept for forkrail's own site (`app.forkrail.com`) are the exception: no project
    // can take them as a name, so the site's projects take them as domains.
    let reserved = domain.strip_suffix(&format!(".{base}")).is_some_and(|label| RESERVED_NAMES.contains(&label) || Some(label) == renamed_to);
    if (bare == base || bare.ends_with(&format!(".{base}"))) && !reserved {
        bail!("{domain} is under {base}, where projects get their hosts already");
    }
    Ok(())
}

/// `forkrail domains ls`: a table.
pub fn print_list(views: &[View]) {
    if views.is_empty() {
        println!("no custom domains");
        return;
    }
    println!("{:<36} {:<20} {:<26} {:<20} CERTIFICATE", "DOMAIN", "PROJECT", "SERVES", "CONFIGURATION");
    for v in views {
        println!("{:<36} {:<20} {:<26} {:<20} {}", v.name, v.project, serves(v), configuration(v), certificate(v));
    }
}

/// `forkrail domains inspect` (and what add and update print): the domain, then its records.
pub fn print_view(v: &View) {
    println!("{}  ({})", v.name, v.project);
    println!("  serves         {}", serves(v));
    println!("  configuration  {}", configuration(v));
    println!("  certificate    {}", certificate(v));
    if let Some(c) = &v.certificate {
        if let Some(error) = &c.error {
            println!("  why            {error}");
        }
        println!(
            "  DNS            {}{}{}",
            if c.dns.points_here { "points here" } else if c.dns.proxied { "proxied by Cloudflare: set the record to DNS only" } else { "doesn't point here" },
            if c.dns.delegated { ", challenge delegated" } else { "" },
            if c.dns.cloudflare { ", in a Cloudflare zone the install edits" } else { "" }
        );
        println!("  checked        {}{}", c.checked_at, c.next_check_at.as_ref().map(|n| format!(", next {n}")).unwrap_or_default());
    }
    if let Some(by) = &v.added_by {
        println!("  added        {} by {by}", v.added_at.as_deref().unwrap_or("-"));
    }
    println!("\n  DNS records at the domain's provider (DNS only: on Cloudflare, proxy off):");
    for r in &v.records {
        let need = match (r.purpose.as_str(), r.required) {
            ("serve", _) => "points it here",
            (_, true) => "for its certificate (required)",
            _ => "optional: its certificate before DNS points here",
        };
        println!("    {:<5} {:<40} {:<44} {need}", r.kind, r.fqdn, r.value);
    }
}

fn serves(v: &View) -> String {
    match (&v.redirect, &v.branch, &v.serves) {
        (Some(r), _, _) => format!("{} to {}", r.status, r.to),
        (None, Some(b), Some(id)) => format!("{id} (branch {b})"),
        (None, Some(b), None) => format!("- (branch {b}: no preview yet)"),
        (None, None, Some(id)) => id.clone(),
        (None, None, None) => "- (no production yet)".into(),
    }
}

/// Whether its DNS points here, as the issuing router last saw it: what Vercel calls a valid
/// configuration.
fn configuration(v: &View) -> &'static str {
    match &v.certificate {
        Some(c) if c.dns.points_here => "valid",
        Some(c) if c.dns.proxied => "invalid (proxied)",
        Some(_) => "invalid",
        None => "-",
    }
}

fn certificate(v: &View) -> String {
    match &v.certificate {
        Some(c) => {
            let until = c.not_after.as_ref().map(|t| format!(" until {}", &t[..t.len().min(10)])).unwrap_or_default();
            let method = c.method.map(|m| format!(" ({})", m.name())).unwrap_or_default();
            match c.state {
                CertState::Issued => format!("issued{until}{method}"),
                CertState::Pending => format!("pending{method}"),
                CertState::Invalid => "invalid configuration".into(),
                CertState::RenewalFailed => format!("renewal failed{until}"),
            }
        }
        None if v.has_certificate => "issued".into(),
        None => "pending".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_checks() {
        assert!(check_domain("atlas.example.net", "forkrail.com", None).is_ok());
        assert!(check_domain("forkrail.com", "forkrail.com", None).is_err());
        assert!(check_domain("x.forkrail.com", "forkrail.com", None).is_err());
        // A project renamed to x may take it; another name, or a deeper one, stays refused.
        assert!(check_domain("x.forkrail.com", "forkrail.com", Some("x")).is_ok());
        assert!(check_domain("y.forkrail.com", "forkrail.com", Some("x")).is_err());
        assert!(check_domain("a.x.forkrail.com", "forkrail.com", Some("x")).is_err());
        assert!(check_domain("app.forkrail.com", "forkrail.com", None).is_ok());
        assert!(check_domain("a.app.forkrail.com", "forkrail.com", None).is_err());
        assert!(check_domain("evilforkrail.com", "forkrail.com", None).is_ok());
        assert!(check_domain("localhost", "forkrail.com", None).is_err());
        assert!(check_domain("*.example.net", "forkrail.com", None).is_ok());
        assert!(check_domain("*.forkrail.com", "forkrail.com", None).is_err());
        assert!(check_domain("*.co.uk", "forkrail.com", None).is_err());
        assert!(check_domain("co.uk", "forkrail.com", None).is_err());
        assert!(check_domain("example.co.uk", "forkrail.com", None).is_ok());
    }

    #[test]
    fn registered_domains() {
        assert_eq!(registered_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(registered_domain("atlas.example.net"), "example.net");
        assert_eq!(registered_domain("example.com"), "example.com");
    }

    #[test]
    fn records_per_kind() {
        let ips = vec!["203.0.113.10".to_owned(), "203.0.113.20".to_owned()];
        let apex = records("example.co.uk", "shop", "forkrail.com", &ips);
        assert_eq!(apex.iter().map(|r| (r.kind.as_str(), r.name.as_str())).collect::<Vec<_>>(), [("A", "@"), ("A", "@"), ("CNAME", "_acme-challenge")]);
        assert!(!apex[2].required && apex[2].value.ends_with("._acme.forkrail.com"));
        let sub = records("api.example.com", "shop", "forkrail.com", &ips);
        assert_eq!((sub[0].kind.as_str(), sub[0].name.as_str(), sub[0].value.as_str()), ("CNAME", "api", "shop.forkrail.com"));
        assert_eq!((sub[1].name.as_str(), sub[1].fqdn.as_str()), ("_acme-challenge.api", "_acme-challenge.api.example.com"));
        let wild = records("*.example.com", "shop", "forkrail.com", &ips);
        assert_eq!((wild[0].kind.as_str(), wild[0].name.as_str()), ("CNAME", "*"));
        assert_eq!((wild[1].name.as_str(), wild[1].required), ("_acme-challenge", true));
        assert_eq!(wild[1].value, acme_delegation("example.com", "forkrail.com"));
    }

    fn routes_with(domains: &[(&str, CustomDomain)]) -> Routes {
        let mut routes = Routes::default();
        let d = |p: &str| forkrail_common::routes::Deployment {
            project: p.into(),
            function: format!("forkrail-app-{p}"),
            qualifier: "1".into(),
            region: None,
            replicas: Default::default(),
            max_inflight: None,
            cpu_target_pct: None,
            max_workers: None,
            idle_ms: None,
            headroom_pct: None,
            max_duration_s: 60,
            memory_mb: 640,
            target: None,
            runtime: None,
            entry: None,
            env: Default::default(),
            static_files: None,
            egress: Vec::new(),
            rules: None,
            images: None,
            schedules: Vec::new(),
            created: None,
            skew_id: None,
            next: None,
        };
        routes.deployments.insert("prod1".into(), d("shop"));
        routes.deployments.insert("prev2".into(), d("shop"));
        routes.hosts.insert("shop-git-staging.forkrail.com".into(), "prev2".into());
        for (name, entry) in domains {
            routes.domains.insert((*name).into(), entry.clone());
        }
        routes
    }

    #[test]
    fn domains_point_where_they_should() {
        let settings = Settings { base_domain: "forkrail.com".into(), ..Default::default() };
        let staging = CustomDomain { branch: Some("staging".into()), ..CustomDomain::new("shop") };
        let www = CustomDomain { redirect: Some(DomainRedirect { to: "example.com".into(), status: 308 }), ..CustomDomain::new("shop") };
        let mut routes = routes_with(&[("example.com", CustomDomain::new("shop")), ("staging.example.com", staging), ("www.example.com", www)]);
        for name in ["example.com", "staging.example.com", "www.example.com"] {
            repoint(&mut routes, name, Some("prod1"), &settings).unwrap();
        }
        assert_eq!(routes.hosts.get("example.com").map(String::as_str), Some("prod1"));
        assert_eq!(routes.hosts.get("staging.example.com").map(String::as_str), Some("prev2"));
        assert!(!routes.hosts.contains_key("www.example.com"));
        assert!(routes.validate().is_ok());
        // Production moves only production's; a push to the branch only the branch's.
        crate::deploy::point_domains(&mut routes, "shop", "prev2");
        assert_eq!(routes.hosts.get("staging.example.com").map(String::as_str), Some("prev2"));
        assert!(!routes.hosts.contains_key("www.example.com"));
        point_branch_domains(&mut routes, "shop", "staging", "prod1");
        point_branch_domains(&mut routes, "shop", "other", "prev2");
        assert_eq!(routes.hosts.get("staging.example.com").map(String::as_str), Some("prod1"));
        // Redirects go to the project's own serving domains.
        let to = |to: &str| DomainRedirect { to: to.into(), status: 301 };
        assert!(check_redirect(&routes, "www.example.com", "shop", &to("example.com"), &settings).is_ok());
        assert!(check_redirect(&routes, "www.example.com", "shop", &to("shop.forkrail.com"), &settings).is_ok());
        assert!(check_redirect(&routes, "x.example.com", "shop", &to("www.example.com"), &settings).unwrap_err().to_string().contains("redirects itself"));
        assert!(check_redirect(&routes, "www.example.com", "shop", &to("elsewhere.com"), &settings).unwrap_err().to_string().contains("add it first"));
        assert!(check_redirect(&routes, "www.example.com", "shop", &DomainRedirect { to: "example.com".into(), status: 303 }, &settings).is_err());
        assert!(check_redirect(&routes, "example.com", "shop", &to("example.com"), &settings).is_err());
    }

    #[test]
    fn ops_parse() {
        let op: DomainsOp = serde_json::from_value(json!({"op": "update", "domain": "a.com", "redirect": null})).unwrap();
        assert!(matches!(op, DomainsOp::Update { redirect: Some(None), branch: None, .. }));
        let op: DomainsOp = serde_json::from_value(json!({"op": "add", "domain": "a.com", "project": "p", "redirect": {"to": "b.com"}})).unwrap();
        assert!(matches!(&op, DomainsOp::Add { redirect: Some(DomainRedirect { status: 308, .. }), .. }));
        assert!(op.changes() && !DomainsOp::List { project: None }.changes());
        assert!(serde_json::from_value::<DomainsOp>(json!({"op": "nuke"})).is_err());
    }
}
