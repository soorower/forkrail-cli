//! The AWS regions a project's functions may run in, as Vercel offers its function regions: each
//! with Vercel's name for it (`iad1`, `fra1`), where it is, and roughly where on the globe, so a
//! router can pick the nearest one a deployment runs in.
//!
//! A project's functions run in one region unless it asks for more (its setting, or `vercel.json`'s
//! `regions`), as on Vercel, whose default is `iad1`. A region needs no router of its own: the
//! router nearest the visitor invokes the function there, and its workers dial back to that router.
//! Where the region has a router, that router runs it instead, over the routers' link.

/// One region functions may run in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FunctionRegion {
    /// AWS's name: `us-east-1`.
    pub aws: &'static str,
    /// Vercel's: `iad1`.
    pub vercel: &'static str,
    /// Where it is, as Vercel describes it: `Washington, D.C.`, `USA`.
    pub city: &'static str,
    pub country: &'static str,
    /// AWS opens it only to accounts that turn it on (an opt-in region).
    pub opt_in: bool,
    /// Roughly where its data centers are, in degrees.
    pub lat: f64,
    pub lon: f64,
}

const fn region(aws: &'static str, vercel: &'static str, city: &'static str, country: &'static str, lat: f64, lon: f64) -> FunctionRegion {
    FunctionRegion { aws, vercel, city, country, opt_in: false, lat, lon }
}

const fn opt_in(r: FunctionRegion) -> FunctionRegion {
    FunctionRegion { opt_in: true, ..r }
}

/// Every region Vercel runs functions in, by AWS's name, in Vercel's order of its list.
pub const FUNCTION_REGIONS: [FunctionRegion; 20] = [
    region("us-east-1", "iad1", "Washington, D.C.", "USA", 39.04, -77.49),
    region("us-east-2", "cle1", "Cleveland", "USA", 39.96, -83.0),
    region("us-west-1", "sfo1", "San Francisco", "USA", 37.77, -122.42),
    region("us-west-2", "pdx1", "Portland", "USA", 45.84, -119.7),
    region("ca-central-1", "yul1", "Montréal", "Canada", 45.5, -73.57),
    region("sa-east-1", "gru1", "São Paulo", "Brazil", -23.55, -46.63),
    region("eu-central-1", "fra1", "Frankfurt", "Germany", 50.11, 8.68),
    region("eu-west-1", "dub1", "Dublin", "Ireland", 53.35, -6.26),
    region("eu-west-2", "lhr1", "London", "United Kingdom", 51.51, -0.13),
    region("eu-west-3", "cdg1", "Paris", "France", 48.86, 2.35),
    region("eu-north-1", "arn1", "Stockholm", "Sweden", 59.33, 18.07),
    region("ap-south-1", "bom1", "Mumbai", "India", 19.08, 72.88),
    region("ap-southeast-1", "sin1", "Singapore", "Singapore", 1.35, 103.82),
    region("ap-southeast-2", "syd1", "Sydney", "Australia", -33.87, 151.21),
    region("ap-northeast-1", "hnd1", "Tokyo", "Japan", 35.68, 139.69),
    region("ap-northeast-2", "icn1", "Seoul", "South Korea", 37.57, 126.98),
    region("ap-northeast-3", "kix1", "Osaka", "Japan", 34.69, 135.5),
    opt_in(region("ap-east-1", "hkg1", "Hong Kong", "Hong Kong", 22.32, 114.17)),
    opt_in(region("af-south-1", "cpt1", "Cape Town", "South Africa", -33.92, 18.42)),
    opt_in(region("me-central-1", "dxb1", "Dubai", "United Arab Emirates", 25.2, 55.27)),
];

/// The region `name` means, by AWS's name (`us-east-1`) or Vercel's (`iad1`), in any case.
pub fn function_region(name: &str) -> Option<&'static FunctionRegion> {
    let name = name.trim();
    FUNCTION_REGIONS.iter().find(|r| r.aws.eq_ignore_ascii_case(name) || r.vercel.eq_ignore_ascii_case(name))
}

/// Great-circle distance between two regions in km, or None if either isn't one of
/// `FUNCTION_REGIONS`.
pub fn distance_km(a: &str, b: &str) -> Option<f64> {
    let (a, b) = (function_region(a)?, function_region(b)?);
    if a.aws == b.aws {
        return Some(0.0);
    }
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let (dlat, dlon) = ((b.lat - a.lat).to_radians(), (b.lon - a.lon).to_radians());
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    Some(2.0 * 6371.0 * h.sqrt().min(1.0).asin())
}

/// How much farther a region with no router of its own counts when a router picks which region to
/// run a function in from afar (`nearest_reached`). Through the router of the function's region,
/// a request costs one round trip on the routers' link, which stays open; invoked from afar, a new
/// worker dials back across (2026-11-22, us-west-1 to us-east-1: 194 ms for a new worker, 62 for a
/// round trip), and most requests waited for one.
pub const DIRECT_FACTOR: f64 = 3.0;

/// Of `candidates`, the one nearest `here`; the first when none is nearer (an unknown region is
/// never nearer than a known one). None only for no candidates.
pub fn nearest<'a>(here: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    nearest_reached(here, candidates, |_| true)
}

/// `nearest`, with a region that has no router of its own (`routed` says which have one) counting
/// `DIRECT_FACTOR` times as far: a router reaches a function's region through that region's router
/// unless one without a router is much nearer.
pub fn nearest_reached<'a>(here: &str, candidates: impl IntoIterator<Item = &'a str>, routed: impl Fn(&str) -> bool) -> Option<&'a str> {
    let mut best: Option<(&'a str, Option<f64>)> = None;
    for c in candidates {
        let d = distance_km(here, c).map(|d| if routed(c) { d } else { d * DIRECT_FACTOR });
        match best {
            None => best = Some((c, d)),
            Some((_, Some(b))) if d.is_some_and(|d| d < b) => best = Some((c, d)),
            Some((_, None)) if d.is_some() => best = Some((c, d)),
            Some(_) => {}
        }
    }
    best.map(|(c, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(function_region("iad1").unwrap().aws, "us-east-1");
        assert_eq!(function_region("FRA1").unwrap().aws, "eu-central-1");
        assert_eq!(function_region(" ap-south-1 ").unwrap().vercel, "bom1");
        assert!(function_region("us-gov-west-1").is_none());
        assert!(function_region("").is_none());
        // Each name once, by either naming.
        for r in &FUNCTION_REGIONS {
            assert_eq!(FUNCTION_REGIONS.iter().filter(|o| o.aws == r.aws || o.vercel == r.vercel).count(), 1, "{}", r.aws);
            assert_eq!(crate::vercel_region(r.aws), Some(r.vercel));
        }
    }

    #[test]
    fn distances() {
        let km = |a, b| distance_km(a, b).unwrap();
        assert_eq!(km("us-east-1", "iad1"), 0.0);
        // Virginia to San Francisco is some 3,900 km; to Frankfurt some 6,600.
        assert!((3700.0..4100.0).contains(&km("us-east-1", "us-west-1")), "{}", km("us-east-1", "us-west-1"));
        assert!((6300.0..6900.0).contains(&km("us-east-1", "eu-central-1")));
        assert_eq!(km("ap-south-1", "eu-central-1"), km("eu-central-1", "ap-south-1"));
        assert!(distance_km("us-east-1", "mars-1").is_none());
    }

    #[test]
    fn nearest_region() {
        assert_eq!(nearest("us-west-1", ["us-east-1", "us-west-2"]), Some("us-west-2"));
        assert_eq!(nearest("us-west-1", ["eu-central-1", "ap-southeast-1"]), Some("eu-central-1"));
        assert_eq!(nearest("eu-west-1", ["us-east-1", "eu-central-1", "ap-south-1"]), Some("eu-central-1"));
        // Nothing known: the first. A known region beats an unknown one.
        assert_eq!(nearest("", ["us-east-1", "us-west-1"]), Some("us-east-1"));
        assert_eq!(nearest("us-west-1", ["somewhere-1", "us-east-1"]), Some("us-east-1"));
        assert_eq!(nearest("us-west-1", std::iter::empty()), None);
    }

    #[test]
    fn nearest_region_through_its_router() {
        let routed = |r: &str| ["us-east-1", "us-west-1", "eu-central-1"].contains(&r);
        // Virginia's router over Ohio's region with none, some 3,400 km from San Francisco against 3,900.
        assert_eq!(nearest_reached("us-west-1", ["us-east-2", "us-east-1"], routed), Some("us-east-1"));
        // But Oregon, 900 km away, over Virginia's router.
        assert_eq!(nearest_reached("us-west-1", ["us-west-2", "us-east-1"], routed), Some("us-west-2"));
        // With no router anywhere it's the nearest, as `nearest` has it.
        assert_eq!(nearest_reached("us-west-1", ["us-east-2", "us-east-1"], |_| false), Some("us-east-2"));
        assert_eq!(nearest_reached("eu-west-1", ["us-east-2", "eu-central-1"], routed), Some("eu-central-1"));
    }
}
