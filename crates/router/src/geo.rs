//! Where a client is, from its address: DB-IP's free City and ASN databases ("IP Geolocation by
//! DB-IP", https://db-ip.com, CC BY 4.0), as `city.mmdb` and `asn.mmdb` in `FORKRAIL_GEOIP_DIR`
//! (default `/var/lib/forkrail/geoip`). A timer on the instance fetches each month's files
//! (`forkrail-geoip-update`, installed by `test-env.sh push`), and the router reopens them within
//! an hour of a change. Without the files, requests just have no location.
//!
//! The files are memory-mapped, so blue and green share one copy in the page cache, and only the
//! parts lookups touch are read. A lookup and its decoding take about 1 µs.
//!
//! The router is the edge, so the connecting address is the client (Global Accelerator keeps it).
//! It logs the location with each request and passes it to the app the way Vercel does
//! (`x-vercel-ip-country`, `-city`, `-latitude`, `-longitude`, `-continent`), after taking off any
//! the client sent. DB-IP's free City file has the region's name but not its code, so
//! `x-vercel-ip-country-region` isn't sent, and it has no time zone or postal code.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use maxminddb::Reader;
use memmap2::Mmap;
use serde::Deserialize;
use tracing::{info, warn};

pub const DEFAULT_DIR: &str = "/var/lib/forkrail/geoip";
const CITY_FILE: &str = "city.mmdb";
const ASN_FILE: &str = "asn.mmdb";
const CHECK_EVERY: Duration = Duration::from_secs(3600);

/// What the databases say about one address. Empty where they say nothing (private addresses).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Location {
    /// ISO 3166-1 alpha-2, `US`.
    pub country: Option<String>,
    /// `NA`, `EU`, `AS`...
    pub continent: Option<String>,
    /// The first subdivision's English name, `California`.
    pub subdivision: Option<String>,
    pub city: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub asn: Option<u32>,
    pub as_org: Option<String>,
}

pub struct Geo {
    dir: PathBuf,
    open: RwLock<Arc<Databases>>,
}

#[derive(Default)]
struct Databases {
    city: Option<Reader<Mmap>>,
    asn: Option<Reader<Mmap>>,
    /// Modification times of the files opened, to notice replacements.
    stamps: (Option<SystemTime>, Option<SystemTime>),
}

impl Geo {
    /// Open whatever of the two files `dir` has.
    pub fn open(dir: PathBuf) -> Arc<Self> {
        let geo = Arc::new(Self { open: RwLock::new(Arc::new(Databases::default())), dir });
        geo.reload_if_changed();
        geo
    }

    /// Look the files over every hour, and reopen them when they've been replaced.
    pub fn watch(self: &Arc<Self>) {
        let geo = self.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(CHECK_EVERY);
            every.tick().await;
            loop {
                every.tick().await;
                let geo = geo.clone();
                let _ = tokio::task::spawn_blocking(move || geo.reload_if_changed()).await;
            }
        });
    }

    fn reload_if_changed(&self) {
        let city_path = self.dir.join(CITY_FILE);
        let asn_path = self.dir.join(ASN_FILE);
        let stamps = (modified(&city_path), modified(&asn_path));
        if self.open.read().unwrap().stamps == stamps {
            return;
        }
        let databases = Databases { city: open(&city_path), asn: open(&asn_path), stamps };
        let describe = |r: &Option<Reader<Mmap>>| {
            r.as_ref().map(|r| format!("{} built {}", r.metadata().database_type, r.metadata().build_epoch))
        };
        match (describe(&databases.city), describe(&databases.asn)) {
            (None, None) => warn!(dir = %self.dir.display(), "no GeoIP databases; requests have no location"),
            (city, asn) => info!(dir = %self.dir.display(), city, asn, "GeoIP databases opened"),
        }
        *self.open.write().unwrap() = Arc::new(databases);
    }

    pub fn lookup(&self, ip: IpAddr) -> Location {
        let databases = self.open.read().unwrap().clone();
        let mut location = Location::default();
        if let Some(city) = databases.city.as_ref().and_then(|r| r.lookup(ip).ok()?.decode::<CityRecord>().ok().flatten()) {
            location.country = city.country.and_then(|c| c.iso_code).map(str::to_owned);
            location.continent = city.continent.and_then(|c| c.code).map(str::to_owned);
            location.subdivision =
                city.subdivisions.and_then(|s| s.into_iter().next()).and_then(|s| s.names?.en).map(str::to_owned);
            location.city = city.city.and_then(|c| c.names?.en).map(str::to_owned);
            if let Some(at) = city.location {
                (location.latitude, location.longitude) = (at.latitude, at.longitude);
            }
        }
        if let Some(asn) = databases.asn.as_ref().and_then(|r| r.lookup(ip).ok()?.decode::<AsnRecord>().ok().flatten()) {
            location.asn = asn.autonomous_system_number;
            location.as_org = asn.autonomous_system_organization.map(str::to_owned);
        }
        location
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn open(path: &Path) -> Option<Reader<Mmap>> {
    if !path.exists() {
        return None;
    }
    // SAFETY: the mapping must not change under the reader. The updater replaces a file by
    // renaming a new one over it, which leaves the mapped inode as it was until it's dropped.
    match unsafe { Reader::open_mmap(path) } {
        Ok(reader) => Some(reader),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "GeoIP database unreadable");
            None
        }
    }
}

#[derive(Deserialize)]
struct Names<'a> {
    #[serde(borrow)]
    en: Option<&'a str>,
}

#[derive(Deserialize)]
struct Named<'a> {
    #[serde(borrow)]
    names: Option<Names<'a>>,
}

#[derive(Deserialize)]
struct Coded<'a> {
    #[serde(borrow)]
    iso_code: Option<&'a str>,
    #[serde(borrow)]
    code: Option<&'a str>,
}

#[derive(Deserialize)]
struct Coordinates {
    latitude: Option<f64>,
    longitude: Option<f64>,
}

#[derive(Deserialize)]
struct CityRecord<'a> {
    #[serde(borrow)]
    city: Option<Named<'a>>,
    #[serde(borrow)]
    country: Option<Coded<'a>>,
    #[serde(borrow)]
    continent: Option<Coded<'a>>,
    #[serde(borrow)]
    subdivisions: Option<Vec<Named<'a>>>,
    location: Option<Coordinates>,
}

#[derive(Deserialize)]
struct AsnRecord<'a> {
    autonomous_system_number: Option<u32>,
    #[serde(borrow)]
    autonomous_system_organization: Option<&'a str>,
}

// The client's own `x-vercel-ip-*` headers are taken off with every other platform header
// (`proxy::platform_only`) before these are set.

/// The location as Vercel's request headers: (name, value), the city percent-encoded as Vercel
/// does it.
pub fn headers(location: &Location) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(country) = &location.country {
        out.push(("x-vercel-ip-country", country.clone()));
    }
    if let Some(continent) = &location.continent {
        out.push(("x-vercel-ip-continent", continent.clone()));
    }
    if let Some(city) = &location.city {
        out.push(("x-vercel-ip-city", percent_encode(city)));
    }
    if let (Some(lat), Some(lon)) = (location.latitude, location.longitude) {
        out.push(("x-vercel-ip-latitude", lat.to_string()));
        out.push(("x-vercel-ip-longitude", lon.to_string()));
    }
    out
}

pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_follow_vercel() {
        let location = Location {
            country: Some("DE".into()),
            continent: Some("EU".into()),
            subdivision: Some("Hesse".into()),
            city: Some("Frankfurt am Main".into()),
            latitude: Some(50.1109),
            longitude: Some(8.68213),
            asn: Some(15169),
            as_org: Some("Google LLC".into()),
        };
        assert_eq!(
            headers(&location),
            vec![
                ("x-vercel-ip-country", "DE".to_owned()),
                ("x-vercel-ip-continent", "EU".to_owned()),
                ("x-vercel-ip-city", "Frankfurt%20am%20Main".to_owned()),
                ("x-vercel-ip-latitude", "50.1109".to_owned()),
                ("x-vercel-ip-longitude", "8.68213".to_owned()),
            ]
        );
        assert!(headers(&Location::default()).is_empty());
        assert_eq!(percent_encode("São Paulo"), "S%C3%A3o%20Paulo");
    }

    #[test]
    fn without_files_nothing_is_known() {
        let geo = Geo::open(PathBuf::from("/nonexistent/forkrail-geoip"));
        assert_eq!(geo.lookup("8.8.8.8".parse().unwrap()), Location::default());
    }

    /// With the real files: `FORKRAIL_GEOIP_DIR=... cargo test -p forkrail-router geo -- --ignored`.
    #[test]
    #[ignore]
    fn real_files() {
        let geo = Geo::open(PathBuf::from(std::env::var("FORKRAIL_GEOIP_DIR").unwrap()));
        let google = geo.lookup("8.8.8.8".parse().unwrap());
        assert_eq!((google.country.as_deref(), google.asn), (Some("US"), Some(15169)));
        assert_eq!(geo.lookup("127.0.0.1".parse().unwrap()), Location::default());
        let start = std::time::Instant::now();
        for i in 0..100_000u32 {
            std::hint::black_box(geo.lookup(IpAddr::from(i.wrapping_mul(2_654_435_761).to_be_bytes())));
        }
        eprintln!("{:.2} µs a lookup", start.elapsed().as_secs_f64() * 10.0);
    }
}
