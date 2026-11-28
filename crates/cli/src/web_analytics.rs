//! A project's Web Analytics setting (Vercel's, `/docs/analytics`): on or off, and off until
//! someone switches it on, as on Vercel (the author, 2026-11-22: "keep it off until switched on").
//! It lives in the routing table (`ProjectRouting::analytics`), so a change reaches every router
//! within seconds with no redeploy; the routers collect (the router's `insights.rs`). An app counts
//! only once its pages load the script, through `@vercel/analytics`, `@forkrail/analytics` or a
//! script tag.

use anyhow::Result;
use forkrail_common::routes::{Routes, WebAnalytics};

use crate::deploy::Ctx;
use crate::state::ROUTES_KEY;

/// What `forkrail project` says after a change.
pub const NOTE: &str = "web analytics applies within seconds; pages count once they load the script (@vercel/analytics, @forkrail/analytics or <script defer src=\"/_forkrail/insights/script.js\">)";

/// Whether the project has it on.
pub fn on(routes: &Routes, project: &str) -> bool {
    routes.projects.get(project).is_some_and(|p| p.analytics_on())
}

pub async fn get(ctx: &Ctx, project: &str) -> Result<bool> {
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    Ok(on(&routes, project))
}

/// Switch it on or off; the project must exist.
pub async fn set(ctx: &Ctx, project: &str, enabled: bool) -> Result<bool> {
    let routes = ctx
        .store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            routes.projects.entry(project.to_owned()).or_default().analytics = Some(WebAnalytics { enabled });
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    Ok(on(&routes, project))
}

pub fn words(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forkrail_common::routes::ProjectRouting;

    #[test]
    fn off_until_switched_on() {
        let mut routes = Routes::default();
        assert!(!on(&routes, "shop"));
        routes.projects.insert("shop".into(), ProjectRouting::default());
        assert!(!on(&routes, "shop"));
        routes.projects.get_mut("shop").unwrap().analytics = Some(WebAnalytics { enabled: true });
        assert!(on(&routes, "shop"));
        let json = serde_json::to_string(&routes).unwrap();
        assert!(json.contains(r#""analytics":{"enabled":true}"#), "{json}");
        let back: Routes = serde_json::from_str(&json).unwrap();
        assert!(on(&back, "shop") && !on(&back, "blog"));
    }
}
