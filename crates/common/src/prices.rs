//! What each metered thing costs, at AWS's list price, for spend protection (C15): the router, the
//! control function and the CLI price usage from this one table.
//!
//! List prices on purpose, never less: free allowances are the whole account's (other workloads
//! use them too), and tiers below list start far above an install's use (egress past 10 TB a
//! month, Lambda past 6 billion GB-s). A budget is checked against what AWS could charge, so it
//! errs high, never low.
//!
//! Prices as read from AWS's price pages in November 2026 (us-east-1 and us-west-1 the same unless
//! a region says otherwise). The dashboard's copy (`web/src/server/costs.ts`) must say the same.

use serde::{Deserialize, Serialize};

/// Billed units, in nano-dollars (10⁻⁹ $) per unit, so a sum of many rows stays exact.
pub type Nanos = i64;

/// A thing that costs money, in the unit AWS bills it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Meter {
    /// Lambda arm64 duration, in MB-ms of configured memory (start-up included: Lambda bills it).
    FnMbMs,
    /// Lambda x86_64 duration, in MB-ms (the builder's image).
    FnX86MbMs,
    /// Lambda invocations.
    FnRequests,
    /// To visitors, in bytes on the wire (TCP payload and headers): EC2's data transfer out to the
    /// internet plus Global Accelerator's premium, by the visitor's zone (`Zone`).
    EgressBytes,
    /// From visitors, in bytes on the wire: Global Accelerator bills its premium on whichever
    /// direction carried more each hour, so the ledger charges what this adds over `EgressBytes`.
    IngressBytes,
    /// Between a router and its Lambda workers over the public network, both ways together.
    TunnelBytes,
    /// Out of the router's region to another (the peer cache, cross-region invokes, logs).
    InterRegionBytes,
    /// A function's own calls to the internet, from its environment.
    FnEgressBytes,
    S3Get,
    S3Put,
    /// KMS decrypts (each new environment opens its sealed env vars once).
    KmsDecrypts,
}

impl Meter {
    pub const ALL: [Meter; 11] = [
        Meter::FnMbMs,
        Meter::FnX86MbMs,
        Meter::FnRequests,
        Meter::EgressBytes,
        Meter::IngressBytes,
        Meter::TunnelBytes,
        Meter::InterRegionBytes,
        Meter::FnEgressBytes,
        Meter::S3Get,
        Meter::S3Put,
        Meter::KmsDecrypts,
    ];

    /// Its name in the ledger's rows.
    pub fn name(self) -> &'static str {
        match self {
            Meter::FnMbMs => "fn_mb_ms",
            Meter::FnX86MbMs => "fn_x86_mb_ms",
            Meter::FnRequests => "fn_requests",
            Meter::EgressBytes => "egress_bytes",
            Meter::IngressBytes => "ingress_bytes",
            Meter::TunnelBytes => "tunnel_bytes",
            Meter::InterRegionBytes => "inter_region_bytes",
            Meter::FnEgressBytes => "fn_egress_bytes",
            Meter::S3Get => "s3_get",
            Meter::S3Put => "s3_put",
            Meter::KmsDecrypts => "kms_decrypts",
        }
    }
}

const GB: f64 = 1_073_741_824.0;
/// $ → nano-dollars.
const NANO: f64 = 1e9;

/// Lambda arm64, per GB-second.
pub const LAMBDA_ARM_GB_S: f64 = 0.000_013_333_4;
/// Lambda x86_64, per GB-second.
pub const LAMBDA_X86_GB_S: f64 = 0.000_016_666_7;
/// Per invocation ($0.20 a million).
pub const LAMBDA_REQUEST: f64 = 0.2 / 1e6;
/// EC2 data transfer out to the internet, per GB (the first 10 TB of a month's tier).
pub const EGRESS_GB: f64 = 0.09;
/// Data transfer between a public or Elastic IP and AWS in the same region, per GB each way.
pub const SAME_REGION_PUBLIC_GB: f64 = 0.01;
/// Out of us-east-1 or us-west-1 to another region, per GB.
pub const INTER_REGION_GB: f64 = 0.02;
/// KMS, per request.
pub const KMS_REQUEST: f64 = 0.03 / 10_000.0;
/// Lambda's scratch disk past the first 512 MB, per GB-second.
pub const LAMBDA_SCRATCH_GB_S: f64 = 0.000_000_030_9;
/// The builder's images in the registry, a GB kept a month.
pub const ECR_GB_MONTH: f64 = 0.10;
/// DynamoDB on demand: a read unit (4 KB) and a write unit (1 KB), and a GB kept a month, with
/// point-in-time recovery's on top.
pub const DYNAMODB_READ_UNIT: f64 = 0.125 / 1e6;
pub const DYNAMODB_WRITE_UNIT: f64 = 0.625 / 1e6;
pub const DYNAMODB_GB_MONTH: f64 = 0.25;
pub const DYNAMODB_PITR_GB_MONTH: f64 = 0.20;

/// S3 Standard, a GB kept a month, by region.
pub fn s3_gb_month(region: &str) -> f64 {
    match region {
        "us-west-1" => 0.026,
        "ca-central-1" | "ap-south-1" | "ap-southeast-1" | "ap-southeast-2" | "ap-northeast-1" => 0.025,
        "eu-central-1" => 0.0245,
        "eu-west-2" => 0.024,
        "sa-east-1" => 0.0405,
        _ => 0.023,
    }
}

/// Bytes in a GB, as AWS bills storage and transfer.
pub const GB_BYTES: f64 = GB;

/// S3 standard requests by region: (GET, PUT) per request.
fn s3(region: &str) -> (f64, f64) {
    match region {
        "us-west-1" => (0.000_44 / 1000.0, 0.0055 / 1000.0),
        _ => (0.0004 / 1000.0, 0.005 / 1000.0),
    }
}

/// Global Accelerator's zones, as its price table names where traffic came in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Zone {
    NA,
    EU,
    AP,
    IN,
    KR,
    ME,
    SA,
    ZA,
    AU,
}

impl Zone {
    pub const ALL: [Zone; 9] = [Zone::NA, Zone::EU, Zone::AP, Zone::IN, Zone::KR, Zone::ME, Zone::SA, Zone::ZA, Zone::AU];

    pub fn name(self) -> &'static str {
        match self {
            Zone::NA => "NA",
            Zone::EU => "EU",
            Zone::AP => "AP",
            Zone::IN => "IN",
            Zone::KR => "KR",
            Zone::ME => "ME",
            Zone::SA => "SA",
            Zone::ZA => "ZA",
            Zone::AU => "AU",
        }
    }

    pub fn named(name: &str) -> Option<Zone> {
        Zone::ALL.into_iter().find(|z| z.name() == name)
    }

    /// The zone of a router's region (the price table's row).
    pub fn of_region(region: &str) -> Zone {
        match region {
            r if r.starts_with("eu-") => Zone::EU,
            r if r.starts_with("ap-south-") => Zone::IN,
            "ap-northeast-2" => Zone::KR,
            "ap-southeast-2" | "ap-southeast-4" => Zone::AU,
            r if r.starts_with("ap-") => Zone::AP,
            r if r.starts_with("sa-") => Zone::SA,
            r if r.starts_with("me-") || r.starts_with("il-") => Zone::ME,
            r if r.starts_with("af-") => Zone::ZA,
            _ => Zone::NA,
        }
    }

    /// The zone a visitor in `country` (ISO 3166-1 alpha-2) comes in through, by where its nearest
    /// edges are. An estimate the weekly check against the accelerator's own counts corrects; an
    /// unknown country is taken as the dearest zone from the router's (`None`).
    pub fn of_country(country: &str) -> Option<Zone> {
        let c = country.to_ascii_uppercase();
        let c = c.as_str();
        Some(match c {
            "US" | "CA" | "MX" | "GT" | "BZ" | "SV" | "HN" | "NI" | "CR" | "PA" | "CU" | "JM" | "HT" | "DO" | "PR" | "BS" | "TT" | "BB" | "AG" | "DM"
            | "GD" | "KN" | "LC" | "VC" | "AW" | "CW" | "KY" | "BM" | "VI" | "VG" | "TC" | "GL" | "PM" | "SX" | "BQ" | "MF" | "BL" | "AI" | "MS"
            | "GP" | "MQ" => Zone::NA,
            "AR" | "BO" | "BR" | "CL" | "CO" | "EC" | "GY" | "PY" | "PE" | "SR" | "UY" | "VE" | "GF" | "FK" => Zone::SA,
            "AE" | "SA" | "QA" | "BH" | "KW" | "OM" | "IL" | "JO" | "LB" | "IQ" | "IR" | "SY" | "YE" | "PS" | "EG" => Zone::ME,
            "IN" | "BD" | "LK" | "NP" | "PK" | "BT" | "MV" => Zone::IN,
            "KR" => Zone::KR,
            "AU" | "NZ" | "FJ" | "PG" | "NC" | "SB" | "VU" | "WS" | "TO" | "PF" => Zone::AU,
            "JP" | "CN" | "HK" | "MO" | "TW" | "SG" | "MY" | "TH" | "VN" | "PH" | "ID" | "KH" | "LA" | "MM" | "BN" | "TL" | "MN" | "KP" | "KZ" | "UZ"
            | "KG" | "TJ" | "TM" | "AF" | "GU" | "MP" | "PW" | "FM" | "MH" => Zone::AP,
            // Africa south of the Sahara (Johannesburg's and Cape Town's edges).
            "ZA" | "NA" | "BW" | "ZW" | "ZM" | "MZ" | "MW" | "LS" | "SZ" | "MG" | "MU" | "AO" | "KE" | "TZ" | "UG" | "RW" | "BI" | "ET" | "SO" | "DJ"
            | "ER" | "SS" | "SD" | "CD" | "CG" | "GA" | "CM" | "CF" | "TD" | "NG" | "GH" | "CI" | "SN" | "ML" | "BF" | "NE" | "GN" | "SL" | "LR" | "TG"
            | "BJ" | "GM" | "GW" | "CV" | "ST" | "GQ" | "MR" | "KM" | "SC" | "RE" | "YT" => Zone::ZA,
            // Europe, with Russia, Turkey, the Caucasus and North Africa (Europe's edges).
            c if c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()) && EUROPE.contains(&c) => Zone::EU,
            _ => return None,
        })
    }
}

const EUROPE: [&str; 57] = [
    "AD", "AL", "AM", "AT", "AX", "AZ", "BA", "BE", "BG", "BY", "CH", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FO", "FR", "GB", "GE", "GG", "GI", "GR",
    "HR", "HU", "IE", "IM", "IS", "IT", "JE", "LI", "LT", "LU", "LV", "MC", "MD", "ME", "MK", "MT", "NL", "NO", "PL", "PT", "RO", "RS", "RU", "SE", "SI",
    "SK", "SM", "TR", "UA", "VA", "XK", "MA",
];

/// Global Accelerator's premium per GB, from routers in `from` to visitors in `to` (Nov 2026).
pub fn accelerator_gb(from: Zone, to: Zone) -> f64 {
    use Zone::*;
    let row: [f64; 9] = match from {
        NA => [0.015, 0.015, 0.035, 0.035, 0.035, 0.035, 0.04, 0.079, 0.105],
        EU => [0.015, 0.015, 0.033, 0.033, 0.035, 0.035, 0.043, 0.067, 0.105],
        AP => [0.012, 0.043, 0.01, 0.032, 0.058, 0.055, 0.049, 0.065, 0.08],
        IN => [0.025, 0.023, 0.023, 0.023, 0.072, 0.074, 0.057, 0.058, 0.094],
        KR => [0.017, 0.02, 0.043, 0.067, 0.043, 0.05, 0.049, 0.06, 0.08],
        ME => [0.032, 0.029, 0.029, 0.029, 0.047, 0.057, 0.061, 0.067, 0.082],
        SA => [0.032, 0.035, 0.041, 0.059, 0.056, 0.057, 0.024, 0.067, 0.083],
        ZA => [0.043, 0.038, 0.045, 0.058, 0.057, 0.067, 0.071, 0.01, 0.091],
        AU => [0.07, 0.07, 0.074, 0.101, 0.091, 0.084, 0.088, 0.093, 0.007],
    };
    row[Zone::ALL.iter().position(|z| *z == to).expect("every zone is in ALL")]
}

/// The dearest zone to send to from `from`: what an unknown visitor's bytes are priced at.
pub fn dearest_zone(from: Zone) -> Zone {
    Zone::ALL.into_iter().max_by(|a, b| accelerator_gb(from, *a).total_cmp(&accelerator_gb(from, *b))).expect("zones")
}

/// The price of one unit of `meter` in `region` (the router's, or for Lambda the function's), in
/// dollars. `zone`: the visitor's, for bytes to and from visitors.
pub fn unit_price(meter: Meter, region: &str, zone: Option<Zone>) -> f64 {
    let from = Zone::of_region(region);
    let to = zone.unwrap_or_else(|| dearest_zone(from));
    match meter {
        Meter::FnMbMs => LAMBDA_ARM_GB_S / 1024.0 / 1000.0,
        Meter::FnX86MbMs => LAMBDA_X86_GB_S / 1024.0 / 1000.0,
        Meter::FnRequests => LAMBDA_REQUEST,
        Meter::EgressBytes => (EGRESS_GB + accelerator_gb(from, to)) / GB,
        // Only what's charged over the egress side (see `ingress_extra`).
        Meter::IngressBytes => accelerator_gb(from, to) / GB,
        Meter::TunnelBytes => SAME_REGION_PUBLIC_GB / GB,
        Meter::InterRegionBytes => INTER_REGION_GB / GB,
        Meter::FnEgressBytes => EGRESS_GB / GB,
        Meter::S3Get => s3(region).0,
        Meter::S3Put => s3(region).1,
        Meter::KmsDecrypts => KMS_REQUEST,
    }
}

/// `quantity` of `meter`, in nano-dollars, rounded up.
pub fn cost(meter: Meter, region: &str, zone: Option<Zone>, quantity: u64) -> Nanos {
    (quantity as f64 * unit_price(meter, region, zone) * NANO).ceil() as Nanos
}

/// Bytes on the wire for `payload` bytes of TCP payload one way: each segment's IP and TCP headers
/// (20 + 32 with timestamps) for a segment of at most `MSS` bytes, as the bill counts packets.
pub fn on_wire(payload: u64) -> u64 {
    const MSS: u64 = 1448;
    const HEADERS: u64 = 52;
    payload + payload.div_ceil(MSS) * HEADERS
}

/// The bytes the other way that acknowledge `payload` (a bare ACK of 52 bytes for every two
/// segments, as delayed ACKs go).
pub fn acks_for(payload: u64) -> u64 {
    const MSS: u64 = 1448;
    payload.div_ceil(MSS).div_ceil(2) * 52
}

/// Nano-dollars as dollars.
pub fn dollars(nanos: Nanos) -> f64 {
    nanos as f64 / NANO
}

/// The month a time (unix seconds, UTC) falls in, as `202611`: budgets' months.
pub fn month_of(secs: u64) -> u32 {
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year * 100 + month) as u32
}

/// The first second of `month` (`202611`), UTC.
pub fn month_start(month: u32) -> u64 {
    let (y, m) = (i64::from(month / 100), i64::from(month % 100));
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    ((era * 146_097 + doe - 719_468) * 86_400) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months() {
        assert_eq!(month_of(1_795_046_400), 202611); // 2026-11-19
        assert_eq!(month_start(202611), 1_793_491_200); // 2026-11-01
        assert_eq!(month_of(month_start(202612)), 202612);
        assert_eq!(month_of(month_start(202701) - 1), 202612);
        assert_eq!(month_start(202403) - month_start(202402), 29 * 86_400);
    }

    #[test]
    fn prices() {
        // A 1 GB function for an hour: $0.048.
        let hour = cost(Meter::FnMbMs, "us-east-1", None, 1024 * 3_600_000);
        assert!((dollars(hour) - 0.048).abs() < 1e-5, "{}", dollars(hour));
        // A million invocations: $0.20.
        assert!((dollars(cost(Meter::FnRequests, "us-east-1", None, 1_000_000)) - 0.2).abs() < 1e-9);
        // A GB to a visitor in the US from us-east-1: $0.09 + $0.015; to Australia, $0.09 + $0.105.
        let gb = 1u64 << 30;
        assert!((dollars(cost(Meter::EgressBytes, "us-east-1", Some(Zone::NA), gb)) - 0.105).abs() < 1e-9);
        assert!((dollars(cost(Meter::EgressBytes, "us-east-1", Some(Zone::AU), gb)) - 0.195).abs() < 1e-9);
        // Unknown visitors are taken as the dearest.
        assert_eq!(cost(Meter::EgressBytes, "us-west-1", None, gb), cost(Meter::EgressBytes, "us-west-1", Some(Zone::AU), gb));
        // S3 in N. California is dearer.
        assert!(cost(Meter::S3Put, "us-west-1", None, 1000) > cost(Meter::S3Put, "us-east-1", None, 1000));
        // Rounded up: even one byte costs something.
        assert_eq!(cost(Meter::TunnelBytes, "us-east-1", None, 1), 1);
        assert_eq!(cost(Meter::TunnelBytes, "us-east-1", None, 0), 0);
    }

    #[test]
    fn zones() {
        assert_eq!(Zone::of_country("us"), Some(Zone::NA));
        assert_eq!(Zone::of_country("BD"), Some(Zone::IN));
        assert_eq!(Zone::of_country("DE"), Some(Zone::EU));
        assert_eq!(Zone::of_country("JP"), Some(Zone::AP));
        assert_eq!(Zone::of_country("NG"), Some(Zone::ZA));
        assert_eq!(Zone::of_country("BR"), Some(Zone::SA));
        assert_eq!(Zone::of_country("NZ"), Some(Zone::AU));
        assert_eq!(Zone::of_country(""), None);
        assert_eq!(Zone::of_country("ZZ"), None);
        assert_eq!(Zone::of_region("us-west-1"), Zone::NA);
        assert_eq!(dearest_zone(Zone::NA), Zone::AU);
        for z in Zone::ALL {
            assert_eq!(Zone::named(z.name()), Some(z));
        }
    }

    #[test]
    fn wire() {
        assert_eq!(on_wire(0), 0);
        assert_eq!(on_wire(1), 53);
        assert_eq!(on_wire(1448), 1500);
        assert_eq!(on_wire(1449), 1553);
        assert_eq!(acks_for(1448 * 4), 104);
    }
}
