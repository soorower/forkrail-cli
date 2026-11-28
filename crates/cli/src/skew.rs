//! A project's skew protection setting (Vercel's, `/docs/skew-protection`, read 2026-11-16): on or
//! off, the maximum age, a threshold, the sites allowed to send skew-protected requests from
//! elsewhere. It lives in the routing table (`ProjectRouting::skew`), so a change reaches every
//! router within seconds with no redeploy; the routers do the pinning (the router's `skew.rs`).
//!
//! Builds and apps get `VERCEL_SKEW_PROTECTION_ENABLED=1` (and `FORKRAIL_SKEW_PROTECTION_ENABLED`)
//! when it's on as the build starts, which tells frameworks to put the deployment's id on their
//! requests. Like Vercel's, turning it on reaches apps with their next deployment.
//!
//! Vercel turns it on for projects created after 2024-11-19. Its docs say "with a supported
//! framework", but every project made on the author's account in 2026-11 has it (15 of 15: a Vite
//! site, Qwik on the `vite` preset, SolidStart, TanStack Start, Gatsby, Remix, FastAPI, Create
//! React App and ones with no framework among them, `skewProtectionMaxAge` 43,200 and
//! `VERCEL_SKEW_PROTECTION_ENABLED=1` where they show it). Here `ON_FOR_NEW_PROJECTS` says whether a
//! project that never set it gets it on with its first build that succeeds (`at_build`), whatever
//! its framework; publishing that build then records it as on (`default_after_build`), so it stays
//! so. Projects that existed before keep what they have.

use anyhow::{Result, bail};
use forkrail_common::routes::{
    Routes, SKEW_DEFAULT_MAX_AGE_S, SKEW_MAX_AGE_S, SKEW_MAX_ALLOWED_DOMAINS, SkewProtection, SkewThreshold, normalize_host, valid_skew_domain,
};
use serde::{Deserialize, Serialize};

use crate::deploy::Ctx;
use crate::state::{self, Project, ROUTES_KEY, Status};

/// Whether a new project's first build turns skew protection on, as Vercel's new projects have it:
/// on (the author, 2026-11-16: "okay agree with your decisions").
pub const ON_FOR_NEW_PROJECTS: bool = true;

/// The frameworks Vercel's docs name for it, by the name a build records (`framework::Framework`):
/// what a build request from before every new project got it (`skew_if_framework`) checks.
pub const FRAMEWORKS: [&str; 6] = ["Next.js", "SvelteKit", "Qwik", "Astro", "Nuxt", "React Router"];

/// The variable builds and apps get when it's on, by Vercel's name and forkrail's.
pub const ENV: &str = "VERCEL_SKEW_PROTECTION_ENABLED";
pub const FORKRAIL_ENV: &str = "FORKRAIL_SKEW_PROTECTION_ENABLED";

pub fn supports(framework: &str) -> bool {
    FRAMEWORKS.contains(&framework)
}

/// What a build is told (`build::Request`'s `skew` and `skew_if_framework`): the project's
/// setting, or for a new project without one `ON_FOR_NEW_PROJECTS`. The second, on only if the
/// build finds a framework of `FRAMEWORKS`, is no longer given: Vercel turns it on whatever the
/// framework.
pub fn at_build(setting: Option<&SkewProtection>, project: Option<&Project>) -> (bool, bool) {
    match setting {
        Some(s) => (s.enabled, false),
        None => (ON_FOR_NEW_PROJECTS && is_new(project), false),
    }
}

/// A project that has never had a deployment ready (a failed first build doesn't count).
fn is_new(project: Option<&Project>) -> bool {
    project.is_none_or(|p| !p.deployments.iter().any(|d| d.status == Status::Ready))
}

/// After a build made with skew protection on by the default, the project records it as on, so
/// the routers pin its requests and later builds keep it. A setting already there stays.
pub fn default_after_build(routes: &mut Routes, project: &str, built_with_skew: bool) {
    if built_with_skew {
        routes.projects.entry(project.to_owned()).or_default().skew.get_or_insert_with(|| SkewProtection { enabled: true, ..Default::default() });
    }
}

/// What to change; `None` leaves a setting as it is, and `Some(None)` clears it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Change {
    pub enabled: Option<bool>,
    /// Seconds; `Some(None)` back to the default (12 hours).
    pub max_age_s: Option<Option<u64>>,
    /// A deployment of the project; `Some(None)` removes the threshold.
    pub threshold: Option<Option<String>>,
    pub allowed_domains: Option<Vec<String>>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.max_age_s.is_none() && self.threshold.is_none() && self.allowed_domains.is_none()
    }

    /// Check what can be checked without the install, with the allowed domains normalised as
    /// Vercel does: a URL becomes its host, without port or path, lowercased.
    pub fn check(&mut self) -> Result<()> {
        if let Some(Some(s)) = self.max_age_s
            && !SKEW_MAX_AGE_S.contains(&s)
        {
            bail!("skew protection's maximum age: from 1 minute to {} days, not {}", SKEW_MAX_AGE_S.end() / 86_400, words(s));
        }
        if let Some(domains) = &mut self.allowed_domains {
            let mut normalised: Vec<String> = Vec::new();
            for d in domains.iter().map(|d| normalize_domain(d)).filter(|d| !d.is_empty()) {
                if !valid_skew_domain(&d) {
                    bail!("allowed domain {d:?}: a domain name (app.example.com), a wildcard one label deep (*.example.com), or * for every site");
                }
                if !normalised.contains(&d) {
                    normalised.push(d);
                }
            }
            if normalised.len() > SKEW_MAX_ALLOWED_DOMAINS {
                bail!("at most {SKEW_MAX_ALLOWED_DOMAINS} allowed domains for cross-site requests, not {}", normalised.len());
            }
            *domains = normalised;
        }
        if let Some(Some(id)) = &self.threshold
            && !forkrail_common::routes::valid_id(id)
        {
            bail!("{id:?} isn't a deployment id");
        }
        Ok(())
    }
}

/// `https://App.Example.com:8443/x` as `app.example.com`; `*.Example.com` as `*.example.com`.
pub fn normalize_domain(text: &str) -> String {
    let text = text.trim();
    let rest = text.split_once("://").map_or(text, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    match authority.strip_prefix("*.") {
        Some(name) => format!("*.{}", normalize_host(name)),
        None if authority == "*" => "*".to_owned(),
        None => normalize_host(authority),
    }
}

/// A project's skew protection as the dashboard, the CLI and the API show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub enabled: bool,
    /// Whether the project has a setting of its own (off when it hasn't).
    pub set: bool,
    /// In force, in seconds, and whether that's the default.
    pub max_age_s: u64,
    pub max_age_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<SkewThreshold>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

pub fn view(routes: &Routes, project: &str) -> View {
    let own = routes.projects.get(project).and_then(|p| p.skew.as_ref());
    let s = own.cloned().unwrap_or_default();
    View {
        enabled: s.enabled,
        set: own.is_some(),
        max_age_s: s.max_age(),
        max_age_default: s.max_age_s.is_none(),
        threshold: s.threshold,
        allowed_domains: s.allowed_domains,
    }
}

/// A project's skew protection as the routers apply it.
pub async fn get(ctx: &Ctx, project: &str) -> Result<View> {
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    Ok(view(&routes, project))
}

/// Change a project's skew protection; the project must exist. A threshold names one of its
/// deployments, whose creation time is kept with it.
pub async fn update(ctx: &Ctx, project: &str, change: &Change) -> Result<View> {
    let mut change = change.clone();
    change.check()?;
    let threshold = match &change.threshold {
        Some(Some(id)) => {
            let record = crate::deploy::record(ctx, id).await?;
            if record.project != project {
                bail!("deployment {id} is {}'s, not {project}'s", record.project);
            }
            let Some(created) = state::parse_rfc3339(&record.created) else { bail!("deployment {id} has no creation time to set a threshold by") };
            Some(Some(SkewThreshold { deployment: id.clone(), created }))
        }
        Some(None) => Some(None),
        None => None,
    };
    let routes = ctx
        .store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            let entry = routes.projects.entry(project.to_owned()).or_default();
            let s = entry.skew.get_or_insert_with(SkewProtection::default);
            apply(s, &change, threshold.clone());
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    Ok(view(&routes, project))
}

fn apply(s: &mut SkewProtection, change: &Change, threshold: Option<Option<SkewThreshold>>) {
    if let Some(on) = change.enabled {
        s.enabled = on;
    }
    if let Some(age) = change.max_age_s {
        // The default is kept as unset, so it follows the default if that changes.
        s.max_age_s = age.filter(|&a| a != SKEW_DEFAULT_MAX_AGE_S);
    }
    if let Some(t) = threshold {
        s.threshold = t;
    }
    if let Some(domains) = &change.allowed_domains {
        s.allowed_domains = domains.clone();
    }
}

/// `30m`, `12h`, `7d`, `2w`, a plain number of seconds, or `default` (None).
pub fn parse_age(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim().to_ascii_lowercase();
    if text == "default" {
        return Ok(None);
    }
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &text[digits.len()..];
    let n: u64 = digits.parse().map_err(|_| format!("{text:?}: a duration like 30m, 12h, 7d or 2w, or `default`"))?;
    let per = match unit {
        "" | "s" => 1,
        "m" | "min" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(format!("{text:?}: a duration like 30m, 12h, 7d or 2w, or `default`")),
    };
    n.checked_mul(per).map(Some).ok_or_else(|| format!("{text:?} is too long"))
}

/// Seconds in words: "12 hours", "7 days", "90 minutes".
pub fn words(s: u64) -> String {
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if s % 86_400 == 0 {
        plural(s / 86_400, "day")
    } else if s % 3_600 == 0 {
        plural(s / 3_600, "hour")
    } else if s % 60 == 0 {
        plural(s / 60, "minute")
    } else {
        plural(s, "second")
    }
}

/// One line for `forkrail project`.
pub fn describe(v: &View) -> String {
    if !v.enabled {
        return if v.set { "off".to_owned() } else { "off (never set)".to_owned() };
    }
    let mut text = format!("on, maximum age {}{}", words(v.max_age_s), if v.max_age_default { " (default)" } else { "" });
    if let Some(t) = &v.threshold {
        text.push_str(&format!(", threshold {}", t.deployment));
    }
    if !v.allowed_domains.is_empty() {
        text.push_str(&format!(", cross-site from {}", v.allowed_domains.join(", ")));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Summary, Target};

    #[test]
    fn checks_and_normalises() {
        let mut c = Change { allowed_domains: Some(vec!["https://App.Example.com:8443/x".into(), "*.Example.org".into(), "app.example.com".into(), " ".into(), "*".into()]), ..Default::default() };
        c.check().unwrap();
        assert_eq!(c.allowed_domains.unwrap(), ["app.example.com", "*.example.org", "*"]);
        for bad in [
            Change { max_age_s: Some(Some(30)), ..Default::default() },
            Change { allowed_domains: Some(vec!["*.com".into()]), ..Default::default() },
            Change { allowed_domains: Some((0..13).map(|i| format!("a{i}.example.com")).collect()), ..Default::default() },
            Change { threshold: Some(Some("Not An Id".into())), ..Default::default() },
        ] {
            assert!(bad.clone().check().is_err(), "{bad:?}");
        }
        assert!(Change::default().is_empty() && !Change { enabled: Some(false), ..Default::default() }.is_empty());
    }

    #[test]
    fn applies_a_change_and_shows_it() {
        let mut routes = Routes::default();
        assert_eq!(view(&routes, "shop"), View { enabled: false, set: false, max_age_s: SKEW_DEFAULT_MAX_AGE_S, max_age_default: true, threshold: None, allowed_domains: vec![] });
        let s = routes.projects.entry("shop".into()).or_default().skew.get_or_insert_with(SkewProtection::default);
        let t = SkewThreshold { deployment: "a1b2c3d4".into(), created: 5 };
        apply(s, &Change { enabled: Some(true), max_age_s: Some(Some(7 * 86_400)), allowed_domains: Some(vec!["*".into()]), ..Default::default() }, Some(Some(t.clone())));
        let v = view(&routes, "shop");
        assert_eq!((v.enabled, v.set, v.max_age_s, v.max_age_default, v.threshold.as_ref()), (true, true, 7 * 86_400, false, Some(&t)));
        assert_eq!(describe(&v), "on, maximum age 7 days, threshold a1b2c3d4, cross-site from *");
        // The default age is kept as unset; a threshold goes with None.
        let s = routes.projects.get_mut("shop").unwrap().skew.as_mut().unwrap();
        apply(s, &Change { max_age_s: Some(Some(SKEW_DEFAULT_MAX_AGE_S)), ..Default::default() }, Some(None));
        assert_eq!((s.max_age_s, s.threshold.as_ref()), (None, None));
        assert_eq!(describe(&view(&routes, "shop")), "on, maximum age 12 hours (default), cross-site from *");
        assert!(routes.validate().is_ok());
    }

    #[test]
    fn new_projects_and_the_default() {
        let summary = |status| Summary { id: "a1".into(), target: Target::Production, created: String::new(), version: "1".into(), status };
        let failed = Project { deployments: vec![summary(Status::Error)], ..Project::default() };
        let ready = Project { deployments: vec![summary(Status::Ready)], ..Project::default() };
        assert!(is_new(None) && is_new(Some(&Project::default())) && is_new(Some(&failed)) && !is_new(Some(&ready)));
        // A setting wins, on or off; without one, only a new project may get the default.
        let on = SkewProtection { enabled: true, ..Default::default() };
        assert_eq!(at_build(Some(&on), Some(&ready)), (true, false));
        assert_eq!(at_build(Some(&SkewProtection::default()), None), (false, false));
        assert_eq!(at_build(None, Some(&ready)), (false, false));
        assert_eq!(at_build(None, None), (ON_FOR_NEW_PROJECTS, false));
        assert_eq!(at_build(None, Some(&failed)), (ON_FOR_NEW_PROJECTS, false));
        assert!(ON_FOR_NEW_PROJECTS, "on, as on Vercel (the author, 2026-11-16)");
        // Publishing a build made with it records it, never over a setting.
        let mut routes = Routes::default();
        default_after_build(&mut routes, "shop", false);
        assert!(routes.projects.is_empty());
        default_after_build(&mut routes, "shop", true);
        assert_eq!(routes.projects["shop"].skew, Some(on));
        routes.projects.get_mut("shop").unwrap().skew = Some(SkewProtection::default());
        default_after_build(&mut routes, "shop", true);
        assert_eq!(routes.projects["shop"].skew, Some(SkewProtection::default()));
        assert!(supports("SvelteKit") && supports("Next.js") && !supports("Vite") && !supports("Express"));
    }

    #[test]
    fn ages() {
        assert_eq!(parse_age("12h"), Ok(Some(43_200)));
        assert_eq!(parse_age("30m"), Ok(Some(1_800)));
        assert_eq!(parse_age("7d"), Ok(Some(604_800)));
        assert_eq!(parse_age("2w"), Ok(Some(1_209_600)));
        assert_eq!(parse_age("90"), Ok(Some(90)));
        assert_eq!(parse_age("Default"), Ok(None));
        assert!(parse_age("12x").is_err() && parse_age("h").is_err() && parse_age("").is_err());
        assert_eq!((words(43_200), words(86_400), words(5_400), words(61)), ("12 hours".into(), "1 day".into(), "90 minutes".into(), "61 seconds".into()));
    }
}
