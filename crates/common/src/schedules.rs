//! Schedules: requests the platform makes to a production deployment on a cron expression, from
//! `vercel.json`'s `crons` and `schedules` (path targets).
//!
//! How a run happens: EventBridge Scheduler (one schedule per entry, in the group
//! `forkrail-<project>`) sends a [`Message`] to the SQS queue [`QUEUE`]. A router takes it, finds
//! the entry in the project's current production deployment in the routing table, and makes the
//! request as it would a visitor's (logged with the entry's key). The runtime adds
//! `Authorization: Bearer $CRON_SECRET` from the deployment's own environment.
//!
//! Shared by the CLI (checking, keys, Scheduler's expression) and the router (the message, the
//! late-run rule).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The SQS queue Scheduler sends runs to, in the home region.
pub const QUEUE: &str = "forkrail-schedules";
/// The role Scheduler sends them as.
pub const SCHEDULER_ROLE: &str = "forkrail-scheduler";
/// A project's schedules are in the Scheduler group `forkrail-<project>`.
pub fn group(project: &str) -> String {
    format!("forkrail-{project}")
}

/// Most schedules a project may have, `crons` and `schedules` together.
pub const MAX_PER_PROJECT: usize = 100;
/// Longest path, and longest `schedules` name.
pub const MAX_PATH: usize = 512;
pub const MAX_NAME: usize = 256;
/// Scheduler's limit on a schedule's name.
const MAX_SCHEDULER_NAME: usize = 64;

/// On the request a router makes for a run: the entry's key. The router takes it off every
/// visitor's request, so a worker that sees it knows the router sent it, and the runtime then adds
/// `Authorization: Bearer $CRON_SECRET` from its own environment.
pub const SCHEDULE_HEADER: &str = "x-forkrail-schedule";
/// The variable whose value goes in `Authorization` (the name apps already use).
pub const SECRET_ENV: &str = "CRON_SECRET";
/// The user agent a run's request has: forkrail's, with the one apps moved over check for.
pub const USER_AGENT: &str = "forkrail-cron/1.0 (compatible; vercel-cron/1.0)";
/// The cron expression, in forkrail's header and the one apps moved over read.
pub const EXPRESSION_HEADERS: [&str; 2] = ["x-forkrail-cron-schedule", "x-vercel-cron-schedule"];

/// One schedule of a deployment, as `vercel.json` gave it: in the build, the deployment's record
/// and its routing-table entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Its name in Scheduler, in messages and in the logs (see [`cron_key`], [`schedule_key`]).
    pub key: String,
    /// A `schedules` entry's own name; none for a `crons` entry, which has no name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub path: String,
    /// `GET` or `POST`.
    pub method: String,
    /// Five fields, as written.
    pub expression: String,
    /// An IANA time zone; UTC when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Minutes a run may start late, spread so runs don't start together (1-15); none for a
    /// `crons` entry, which runs within its minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<u32>,
}

impl Schedule {
    /// What people call it: a `schedules` entry's name, or a cron's path.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.path)
    }

    /// How late a run may still start, in seconds; 0 means never too late. As Vercel's rule: the
    /// jitter window plus the shortest interval between runs (at least 10 minutes), and never for
    /// a cron that sets a day of the month or a month (monthly, yearly).
    pub fn late_s(&self) -> u64 {
        let Ok(cron) = Cron::parse(&self.expression) else { return 0 };
        if cron.dom_set || cron.month_set {
            return 0;
        }
        u64::from(self.jitter.unwrap_or(0)) * 60 + cron.min_interval_s().max(600)
    }
}

/// The key of a `crons` entry: crons have no names, so the path and expression name it.
pub fn cron_key(path: &str, expression: &str) -> String {
    format!("c-{}", short_hash(&format!("{path} {expression}")))
}

/// The key of a `schedules` entry: its name when Scheduler takes it as one, else its hash.
pub fn schedule_key(name: &str) -> String {
    let fits = name.len() <= MAX_SCHEDULER_NAME && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    // `c-` and `s-` are the hashed keys' own; a name like them is hashed too.
    if fits && !name.starts_with("c-") && !name.starts_with("s-") {
        name.to_owned()
    } else {
        format!("s-{}", short_hash(name))
    }
}

/// 16 hex digits of FNV-1a 64: stable across releases and platforms (keys are stored), and with
/// no dependency the runtime would carry. Not for secrets.
fn short_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// What Scheduler sends to the queue for a run, and what a manual run sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Always 1 so far.
    pub v: u32,
    pub project: String,
    pub key: String,
    /// When the expression matched (RFC 3339, jitter excluded); a manual run has its own time.
    #[serde(default)]
    pub scheduled: String,
    /// The run's id: the same for every delivery of one run.
    pub run: String,
    /// How late it may still start, in seconds; 0 never too late.
    #[serde(default)]
    pub late_s: u64,
    /// Asked for by someone (Run now), not by the schedule.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
}

/// Valid `schedules` names: 1-256 of `A-Za-z0-9._-`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// A five-field cron expression, read the way Vercel reads them: digits, `*`, `,`, `-` and `/`
/// only, one space between fields, no names (`MON`, `JAN`), and not both a day of the month and a
/// day of the week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    pub minutes: BTreeSet<u8>,
    pub hours: BTreeSet<u8>,
    pub days: BTreeSet<u8>,
    pub months: BTreeSet<u8>,
    /// 0-6, Sunday first.
    pub weekdays: BTreeSet<u8>,
    /// The fields as written.
    fields: [String; 5],
    /// Whether the day of the month, the month and the day of the week are anything but `*`.
    pub dom_set: bool,
    pub month_set: bool,
    pub dow_set: bool,
}

const FIELDS: [(&str, u8, u8); 5] = [("minute", 0, 59), ("hour", 0, 23), ("day of the month", 1, 31), ("month", 1, 12), ("day of the week", 0, 6)];

impl Cron {
    pub fn parse(expression: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expression.split(' ').collect();
        if parts.len() != 5 || parts.iter().any(|p| p.is_empty()) {
            return Err(format!("{expression:?}: a cron expression has five fields, one space apart (minute hour day-of-month month day-of-week)"));
        }
        let mut sets: Vec<BTreeSet<u8>> = Vec::with_capacity(5);
        for (part, (what, min, max)) in parts.iter().zip(FIELDS) {
            sets.push(field(part, min, max).map_err(|e| format!("{expression:?}: the {what} field {part:?}: {e}"))?);
        }
        let (dom_set, month_set, dow_set) = (parts[2] != "*", parts[3] != "*", parts[4] != "*");
        if dom_set && dow_set {
            return Err(format!("{expression:?}: set a day of the month or a day of the week, not both: the other must be *"));
        }
        let fields = [0, 1, 2, 3, 4].map(|i| parts[i].to_owned());
        let mut sets = sets.into_iter();
        let mut next = || sets.next().expect("five fields");
        Ok(Cron { minutes: next(), hours: next(), days: next(), months: next(), weekdays: next(), fields, dom_set, month_set, dow_set })
    }

    /// Whether it runs on a day: its month, and its day of the month or of the week (whichever is
    /// set; both `*` is every day). `weekday` is 0-6, Sunday first.
    pub fn runs_on(&self, month: u8, day: u8, weekday: u8) -> bool {
        self.months.contains(&month) && self.days.contains(&day) && self.weekdays.contains(&weekday)
    }

    /// Its times of day, in minutes since midnight, in order.
    pub fn times(&self) -> Vec<u32> {
        let mut times = Vec::with_capacity(self.hours.len() * self.minutes.len());
        for &h in &self.hours {
            for &m in &self.minutes {
                times.push(u32::from(h) * 60 + u32::from(m));
            }
        }
        times
    }

    /// The shortest time between two runs, in seconds, over four years of days (leap days
    /// included). Times are in the schedule's own zone: a daylight-saving change can make one gap
    /// an hour shorter or longer.
    pub fn min_interval_s(&self) -> u64 {
        let times = self.times();
        let (first, last) = (u64::from(times[0]), u64::from(*times.last().expect("a time")));
        let mut best = times.windows(2).map(|w| u64::from(w[1] - w[0])).min().unwrap_or(u64::MAX);
        // Day numbers from 2028-01-01, a Saturday, through 2031-12-31.
        let mut previous: Option<u64> = None;
        let mut day_no = 0u64;
        for year in 2028..2032u32 {
            for month in 1..=12u8 {
                for day in 1..=days_in_month(year, month) {
                    let weekday = ((day_no + 6) % 7) as u8;
                    if self.runs_on(month, day, weekday) {
                        if let Some(p) = previous {
                            best = best.min((day_no - p) * 1440 + first - last);
                        }
                        previous = Some(day_no);
                    }
                    day_no += 1;
                }
            }
        }
        // A day that never comes (February 30) runs never: nothing to space.
        if best == u64::MAX { u64::MAX } else { best * 60 }
    }

    /// Whether it ever runs (`0 0 30 2 *` doesn't).
    pub fn ever_runs(&self) -> bool {
        (1..=12u8).any(|m| self.months.contains(&m) && self.days.iter().any(|&d| d <= days_in_month(2028, m)))
    }

    /// The expression as EventBridge Scheduler takes it: `cron(min hour dom month dow year)`, with
    /// `?` in whichever day field is `*` (it allows only one), the day of the week counted from 1
    /// (Sunday), and lists for anything with a step but `*/n`.
    pub fn aws(&self) -> String {
        let plain = |i: usize, set: &BTreeSet<u8>, min: u8| -> String {
            let f = &self.fields[i];
            if f == "*" {
                return "*".to_owned();
            }
            if let Some(step) = f.strip_prefix("*/") {
                return format!("{min}/{step}");
            }
            compress(set.iter().copied())
        };
        let minutes = plain(0, &self.minutes, 0);
        let hours = plain(1, &self.hours, 0);
        let months = plain(3, &self.months, 1);
        let (days, weekdays) = if self.dow_set {
            ("?".to_owned(), compress(self.weekdays.iter().map(|d| d + 1)))
        } else {
            (plain(2, &self.days, 1), "?".to_owned())
        };
        format!("cron({minutes} {hours} {days} {months} {weekdays} *)")
    }
}

/// Seconds since the epoch of an RFC 3339 time in UTC (`2026-11-14T10:00:00Z`, fractions and
/// `+00:00` too), as Scheduler writes `<aws.scheduler.scheduled-time>`.
pub fn parse_time(text: &str) -> Option<f64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (year, month, day, hour, minute, second) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let rest = &text[19..];
    let (fraction, zone) = match rest.strip_prefix('.') {
        Some(r) => {
            let digits = r.bytes().take_while(u8::is_ascii_digit).count();
            (format!("0.{}", &r[..digits]).parse::<f64>().ok()?, &r[digits..])
        }
        None => (0.0, rest),
    };
    if !matches!(zone, "Z" | "z" | "+00:00" | "-00:00") {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second) as f64 + fraction)
}

pub fn days_in_month(year: u32, month: u8) -> u8 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// One field: `*`, `n`, `a-b`, with `/step` on `*`, `a` or `a-b`, and lists of those.
fn field(text: &str, min: u8, max: u8) -> Result<BTreeSet<u8>, String> {
    if !text.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'*' | b',' | b'-' | b'/')) {
        return Err("use digits, *, commas, - and / only (no names such as MON or JAN)".to_owned());
    }
    let number = |s: &str| -> Result<u8, String> {
        let n: u8 = s.parse().map_err(|_| format!("{s:?} isn't a number"))?;
        if n < min || n > max {
            return Err(format!("{n} is outside {min}-{max}"));
        }
        Ok(n)
    };
    let mut set = BTreeSet::new();
    for item in text.split(',') {
        if item.is_empty() {
            return Err("an empty item in a list".to_owned());
        }
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u8 = step.parse().map_err(|_| format!("the step {step:?} isn't a number"))?;
                if step == 0 || step > max {
                    return Err(format!("the step {step} must be 1-{max}"));
                }
                (range, step)
            }
            None => (item, 1),
        };
        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (a, b) = (number(a)?, number(b)?);
            if a > b {
                return Err(format!("the range {a}-{b} runs backwards"));
            }
            (a, b)
        } else {
            let a = number(range)?;
            // `a/step` runs from `a` to the end, as cron reads it.
            (a, if item.contains('/') { max } else { a })
        };
        set.extend((from..=to).step_by(usize::from(step)));
    }
    Ok(set)
}

/// A list of numbers in order, with runs of three or more as ranges: `1,2,3,5` → `1-3,5`.
fn compress(values: impl Iterator<Item = u8>) -> String {
    let values: Vec<u8> = values.collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < values.len() {
        let mut j = i;
        while j + 1 < values.len() && values[j + 1] == values[j] + 1 {
            j += 1;
        }
        if j >= i + 2 {
            out.push(format!("{}-{}", values[i], values[j]));
        } else {
            out.extend(values[i..=j].iter().map(u8::to_string));
        }
        i = j + 1;
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[u8]) -> BTreeSet<u8> {
        v.iter().copied().collect()
    }

    #[test]
    fn parses_fields() {
        let c = Cron::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(c.minutes, set(&[0, 15, 30, 45]));
        assert_eq!(c.hours, (9..=17).collect());
        assert_eq!(c.weekdays, set(&[1, 2, 3, 4, 5]));
        assert!(c.dow_set && !c.dom_set);
        let c = Cron::parse("5,10-12,20/20 0 1 */3 *").unwrap();
        assert_eq!(c.minutes, set(&[5, 10, 11, 12, 20, 40]));
        assert_eq!(c.months, set(&[1, 4, 7, 10]));
        assert_eq!(Cron::parse("0 0 * * 0").unwrap().weekdays, set(&[0]));
        assert_eq!(Cron::parse("1-10/3 * * * *").unwrap().minutes, set(&[1, 4, 7, 10]));
    }

    #[test]
    fn refuses_what_vercel_refuses() {
        for bad in [
            "* * * *",
            "* * * * * *",
            "*  * * * *",
            " * * * * *",
            "0 9 * * MON",
            "0 9 * JAN *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 7",
            "0 0 1 * 1",
            "*/0 * * * *",
            "5-1 * * * *",
            "1,,2 * * * *",
            "? * * * *",
            "L * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn aws_expressions() {
        let aws = |e: &str| Cron::parse(e).unwrap().aws();
        assert_eq!(aws("* * * * *"), "cron(* * * * ? *)");
        assert_eq!(aws("0 5 * * *"), "cron(0 5 * * ? *)");
        assert_eq!(aws("*/5 * * * *"), "cron(0/5 * * * ? *)");
        assert_eq!(aws("0 9 * * 1-5"), "cron(0 9 ? * 2-6 *)");
        assert_eq!(aws("0 9 * * 0"), "cron(0 9 ? * 1 *)");
        assert_eq!(aws("0 9 * * 6,0"), "cron(0 9 ? * 1,7 *)");
        assert_eq!(aws("0 9 * * */2"), "cron(0 9 ? * 1,3,5,7 *)");
        assert_eq!(aws("0 0 1 * *"), "cron(0 0 1 * ? *)");
        assert_eq!(aws("0 0 */2 * *"), "cron(0 0 1/2 * ? *)");
        assert_eq!(aws("10-40/10 1,2,3,7 * 1-3 *"), "cron(10,20,30,40 1-3,7 * 1-3 ? *)");
        assert_eq!(aws("15 * * */6 *"), "cron(15 * * 1/6 ? *)");
    }

    /// A small reader of Scheduler's cron, enough for what `aws` writes, so each translation can be
    /// checked against the original minute by minute over a year.
    fn aws_matches(aws: &str, minute: u8, hour: u8, day: u8, month: u8, weekday0: u8) -> bool {
        let inner = aws.strip_prefix("cron(").unwrap().strip_suffix(')').unwrap();
        let f: Vec<&str> = inner.split(' ').collect();
        let has = |text: &str, value: u8, min: u8, max: u8| -> bool {
            if text == "*" || text == "?" {
                return true;
            }
            text.split(',').any(|item| {
                if let Some((a, step)) = item.split_once('/') {
                    let (a, step): (u8, u8) = (a.parse().unwrap(), step.parse().unwrap());
                    value >= a && value <= max && (value - a).is_multiple_of(step)
                } else if let Some((a, b)) = item.split_once('-') {
                    (a.parse::<u8>().unwrap()..=b.parse().unwrap()).contains(&value)
                } else {
                    item.parse::<u8>().unwrap() == value && value >= min
                }
            })
        };
        has(f[0], minute, 0, 59) && has(f[1], hour, 0, 23) && has(f[2], day, 1, 31) && has(f[3], month, 1, 12) && has(f[4], weekday0 + 1, 1, 7)
    }

    #[test]
    fn aws_runs_when_the_original_does() {
        for e in [
            "* * * * *",
            "*/7 */5 * * *",
            "0 9 * * 1-5",
            "30 23 * * 0,6",
            "0 0 1,15 * *",
            "0 12 */10 */4 *",
            "5-50/15 2-20/6 * * 3/2",
            "0 0 29 2 *",
            "59 23 31 12 *",
        ] {
            let c = Cron::parse(e).unwrap();
            let aws = c.aws();
            let mut weekday = 6u8; // 2028-01-01 is a Saturday
            for month in 1..=12u8 {
                for day in 1..=days_in_month(2028, month) {
                    for hour in 0..24u8 {
                        for minute in 0..60u8 {
                            let ours = c.runs_on(month, day, weekday) && c.hours.contains(&hour) && c.minutes.contains(&minute);
                            assert_eq!(ours, aws_matches(&aws, minute, hour, day, month, weekday), "{e} → {aws} at {month}-{day} {hour}:{minute}");
                        }
                    }
                    weekday = (weekday + 1) % 7;
                }
            }
        }
    }

    #[test]
    fn intervals_and_lateness() {
        let i = |e: &str| Cron::parse(e).unwrap().min_interval_s();
        assert_eq!(i("* * * * *"), 60);
        assert_eq!(i("*/5 * * * *"), 300);
        assert_eq!(i("0 * * * *"), 3600);
        assert_eq!(i("0 9 * * *"), 86400);
        assert_eq!(i("0 9 * * 1-5"), 86400);
        assert_eq!(i("0 9,10 * * *"), 3600);
        assert_eq!(i("50 23 * * *"), 86400);
        assert_eq!(i("0 0 * * 1"), 7 * 86400);
        let s = |e: &str, jitter: Option<u32>| Schedule {
            key: "k".into(),
            name: None,
            path: "/".into(),
            method: "GET".into(),
            expression: e.into(),
            timezone: None,
            jitter,
        };
        assert_eq!(s("* * * * *", None).late_s(), 600);
        assert_eq!(s("* * * * *", Some(1)).late_s(), 660);
        assert_eq!(s("0 9 * * 1-5", Some(5)).late_s(), 300 + 86400);
        assert_eq!(s("0 0 1 * *", None).late_s(), 0);
        assert_eq!(s("0 0 * 6 *", None).late_s(), 0);
        assert!(!Cron::parse("0 0 30 2 *").unwrap().ever_runs());
        assert!(Cron::parse("0 0 29 2 *").unwrap().ever_runs());
    }

    #[test]
    fn keys() {
        assert_eq!(cron_key("/api/sync", "0 5 * * *"), cron_key("/api/sync", "0 5 * * *"));
        assert_ne!(cron_key("/api/sync", "0 5 * * *"), cron_key("/api/sync", "0 6 * * *"));
        assert!(cron_key("/a", "* * * * *").starts_with("c-") && cron_key("/a", "* * * * *").len() == 18);
        assert_eq!(schedule_key("daily-report"), "daily-report");
        assert!(schedule_key("c-sneaky").starts_with("s-"));
        assert!(schedule_key(&"a".repeat(65)).starts_with("s-"));
        assert!(valid_name("a.b_c-1") && !valid_name("-a") && !valid_name("") && !valid_name("a b") && !valid_name(&"a".repeat(257)));
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_time("2026-11-14T10:00:00Z"), Some(1_794_650_400.0));
        assert_eq!(parse_time("2026-11-14T10:00:00.250+00:00"), Some(1_794_650_400.25));
        assert_eq!(parse_time("2028-02-29T00:00:00Z"), Some(1_835_395_200.0));
        assert_eq!(parse_time("2026-11-14T10:00:00+01:00"), None);
        assert_eq!(parse_time("<aws.scheduler.scheduled-time>"), None);
        let m: Message = serde_json::from_str(r#"{"v":1,"project":"p","key":"k","scheduled":"2026-11-14T10:00:00Z","run":"r","late_s":600}"#).unwrap();
        assert!(!m.manual);
        assert_eq!(m.late_s, 600);
    }
}
