//! `vercel.json`'s routing keys as Vercel's routes, the way routing-utils' `getTransformedRoutes`
//! converts them (`cleanUrls`, `trailingSlash`, `routes`, `redirects`, `headers`, then
//! `{ handle: "filesystem" }` and `rewrites`), including its handling of destinations: named
//! parameters become `$1`-style references, and a rewrite's parameters its destination doesn't use
//! are passed on in its query string.

use super::p2r;
use super::{Condition, Header, Matchable, Route, SourceRoute};

/// routing-utils' name for a parameter with no name of its own, such as `(.*)`.
pub const UNNAMED: &str = "__UN_NAMED_SEGMENT__";

/// A `source` pattern as a regex, anchored, and its parameters' names.
pub fn source_to_regex(source: &str) -> Result<(String, Vec<String>), String> {
    let (src, keys) = p2r::source_to_regex(source)?;
    Ok((normalize_src(&src), keys.into_iter().map(|k| k.unwrap_or_else(|| UNNAMED.to_owned())).collect()))
}

/// What `normalizeRoutes` does to every `src`: anchor it, and unescape `/`.
pub fn normalize_src(src: &str) -> String {
    let mut s = src.to_owned();
    if !s.starts_with('^') {
        s.insert(0, '^');
    }
    if !s.ends_with('$') {
        s.push('$');
    }
    s.replace("\\/", "/")
}

/// `cleanUrls`: `/about.html` and `/about/index.html` redirect to `/about`.
pub fn clean_urls(clean: bool, trailing_slash: Option<bool>) -> Vec<SourceRoute> {
    if !clean {
        return Vec::new();
    }
    let loc = if trailing_slash == Some(true) { "/$1/" } else { "/$1" };
    vec![
        redirect_route("^/(?:(.+)/)?index(?:\\.html)?/?$", loc, 308, "cleanUrls"),
        redirect_route("^/(.*)\\.html/?$", loc, 308, "cleanUrls"),
    ]
}

/// `trailingSlash`: true adds a slash to paths without a file extension, false removes it.
pub fn trailing_slash(enable: bool) -> Vec<SourceRoute> {
    if enable {
        vec![
            SourceRoute { src: "^/\\.well-known(?:/.*)?$".into(), from: Some("trailingSlash".into()), ..Default::default() },
            redirect_route("^/((?:[^/]+/)*[^/\\.]+)$", "/$1/", 308, "trailingSlash"),
            redirect_route("^/((?:[^/]+/)*[^/]+\\.\\w+)/$", "/$1", 308, "trailingSlash"),
        ]
    } else {
        vec![redirect_route("^/(.*)\\/$", "/$1", 308, "trailingSlash")]
    }
}

fn redirect_route(src: &str, location: &str, status: u16, from: &str) -> SourceRoute {
    SourceRoute {
        src: normalize_src(src),
        headers: vec![Header { key: "Location".into(), value: location.into() }],
        status: Some(status),
        from: Some(from.into()),
        ..Default::default()
    }
}

/// One of `redirects`.
pub struct Redirect<'a> {
    pub source: &'a str,
    pub destination: &'a str,
    pub permanent: Option<bool>,
    pub status: Option<u16>,
    pub has: Vec<Condition>,
    pub missing: Vec<Condition>,
}

pub fn redirect(r: Redirect<'_>, from: String) -> Result<SourceRoute, String> {
    let (src, segments) = source_to_regex(r.source)?;
    let has_segments = has_segments(&r.has);
    let loc = replace_segments(&segments, &has_segments, r.destination, true)?;
    let status = match (r.permanent, r.status) {
        (Some(true), _) => 308,
        (Some(false), _) => 307,
        (None, Some(code)) => code,
        (None, None) => 308,
    };
    Ok(SourceRoute {
        src,
        headers: vec![Header { key: "Location".into(), value: loc }],
        status: Some(status),
        has: lower_header_keys(r.has),
        missing: lower_header_keys(r.missing),
        from: Some(from),
        ..Default::default()
    })
}

/// One of `rewrites`.
pub struct Rewrite<'a> {
    pub source: &'a str,
    pub destination: &'a str,
    pub status: Option<u16>,
    pub has: Vec<Condition>,
    pub missing: Vec<Condition>,
}

pub fn rewrite(r: Rewrite<'_>, from: String) -> Result<SourceRoute, String> {
    let (src, segments) = source_to_regex(r.source)?;
    let has_segments = has_segments(&r.has);
    let dest = replace_segments(&segments, &has_segments, r.destination, false)?;
    Ok(SourceRoute {
        src,
        dest: Some(dest),
        check: true,
        status: r.status,
        has: lower_header_keys(r.has),
        missing: lower_header_keys(r.missing),
        from: Some(from),
        ..Default::default()
    })
}

/// One of `headers`.
pub struct Headers<'a> {
    pub source: &'a str,
    pub headers: Vec<(String, String)>,
    pub has: Vec<Condition>,
    pub missing: Vec<Condition>,
}

pub fn headers(h: Headers<'_>, from: String) -> Result<SourceRoute, String> {
    let (src, segments) = source_to_regex(h.source)?;
    let has_segments = has_segments(&h.has);
    let named = segments.iter().any(|s| s != UNNAMED);
    let mut indexes: Vec<(String, String)> = Vec::new();
    for (i, name) in segments.iter().enumerate() {
        set_index(&mut indexes, name, format!("${}", i + 1));
    }
    for name in &has_segments {
        set_index(&mut indexes, name, format!("${name}"));
    }
    let mut out = Vec::new();
    for (mut key, mut value) in h.headers {
        if named || !has_segments.is_empty() {
            if key.contains(':') {
                key = safely_compile(&key, &indexes, false)?;
            }
            if value.contains(':') {
                value = safely_compile(&value, &indexes, false)?;
            }
        }
        // A key given twice keeps the last value, as in the JavaScript object routing-utils builds.
        out.retain(|h: &Header| h.key != key);
        out.push(Header { key, value });
    }
    Ok(SourceRoute {
        src,
        headers: out,
        cont: true,
        has: lower_header_keys(h.has),
        missing: lower_header_keys(h.missing),
        from: Some(from),
        ..Default::default()
    })
}

/// Which parameters a `has` list adds: a key with no value is one, and so is each named group of
/// a value's regex (and `host`, for a host matched by a value).
pub fn has_segments(has: &[Condition]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_owned());
        }
    };
    for item in has {
        if item.value.is_none()
            && let Some(key) = &item.key
        {
            add(key);
        }
        let regex = match &item.value {
            Some(Matchable::Text(text)) => Some(text.as_str()),
            Some(Matchable::Ops(ops)) => ops.re.as_deref(),
            None => None,
        };
        if let Some(regex) = regex {
            for name in named_groups(regex) {
                add(&name);
            }
            if item.kind == super::ConditionKind::Host {
                add("host");
            }
        }
    }
    out
}

/// The names of a regex's `(?<name>...)` groups.
pub fn named_groups(regex: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = regex;
    while let Some(at) = rest.find("(?<") {
        let after = &rest[at + 3..];
        let name: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !name.is_empty() && name.starts_with(|c: char| c.is_ascii_alphabetic()) && after[name.len()..].starts_with('>') {
            out.push(name);
        }
        rest = after;
    }
    out
}

fn lower_header_keys(mut items: Vec<Condition>) -> Vec<Condition> {
    for item in &mut items {
        if item.kind == super::ConditionKind::Header
            && let Some(key) = &mut item.key
        {
            *key = key.to_ascii_lowercase();
        }
    }
    items
}

/// Set `name` in an insertion-ordered map, as a JavaScript object would.
fn set_index(indexes: &mut Vec<(String, String)>, name: &str, value: String) {
    match indexes.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = value,
        None => indexes.push((name.to_owned(), value)),
    }
}

/// routing-utils' `replaceSegments`: `destination` with each named parameter replaced by its
/// `$1`-style reference. For a rewrite, the parameters it doesn't use go on its query string.
pub fn replace_segments(segments: &[String], has_segments: &[String], destination: &str, is_redirect: bool) -> Result<String, String> {
    let named = segments.iter().any(|s| s != UNNAMED);
    let can_need_replacing = (destination.contains(':') && named) || !has_segments.is_empty() || !is_redirect;
    if !can_need_replacing {
        return Ok(destination.to_owned());
    }
    let mut escaped = destination.to_owned();
    let mut indexes: Vec<(String, String)> = Vec::new();
    for (i, name) in segments.iter().enumerate() {
        set_index(&mut indexes, name, format!("${}", i + 1));
        escaped = escaped.replace(&format!(":{name}"), &format!("__ESC_COLON_{name}"));
    }
    for name in has_segments {
        set_index(&mut indexes, name, format!("${name}"));
        escaped = escaped.replace(&format!(":{name}"), &format!("__ESC_COLON_{name}"));
    }
    let mut url = NodeUrl::parse(&escaped);
    let pathname = unescape_segments(url.pathname.as_deref().unwrap_or(""));
    let hash = unescape_segments(url.hash.as_deref().unwrap_or(""));
    let hostname = unescape_segments(url.hostname.as_deref().unwrap_or(""));

    let mut dest_params: Vec<String> = Vec::new();
    for part in [&pathname, &hash, &hostname] {
        if let Ok(names) = p2r::param_names(part) {
            dest_params.extend(names);
        }
    }
    let pathname = safely_compile(&pathname, &indexes, true)?;
    let hash = if hash.is_empty() { None } else { Some(safely_compile(&hash, &indexes, true)?) };
    let hostname = if hostname.is_empty() { None } else { Some(safely_compile(&hostname, &indexes, true)?) };
    for (_, values) in &mut url.query {
        for value in values.iter_mut() {
            *value = safely_compile(&unescape_segments(value), &indexes, true)?;
        }
    }
    let needs_query = !is_redirect && !indexes.iter().any(|(param, _)| dest_params.contains(param));
    if needs_query {
        for (param, value) in &indexes {
            if param != UNNAMED && !url.query.iter().any(|(k, _)| k == param) {
                url.query.push((param.clone(), vec![value.clone()]));
            }
        }
    }
    url.pathname = Some(pathname);
    url.hash = hash;
    url.hostname = hostname;
    Ok(url.format().replace("%24", "$"))
}

fn unescape_segments(s: &str) -> String {
    // Case-insensitively, as routing-utils does: `url.parse` lowercases a hostname.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    const MARK: &str = "__esc_colon_";
    while let Some(at) = rest.to_ascii_lowercase().find(MARK) {
        out.push_str(&rest[..at]);
        out.push(':');
        rest = &rest[at + MARK.len()..];
    }
    out.push_str(rest);
    out
}

/// routing-utils' `safelyCompile`: fill in the template directly if path-to-regexp can read it;
/// otherwise escape everything but the known parameters and fill in that.
pub fn safely_compile(value: &str, indexes: &[(String, String)], attempt_direct: bool) -> Result<String, String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if attempt_direct && let Ok(out) = p2r::compile(value, indexes) {
        return Ok(out);
    }
    let mut value = value.to_owned();
    for (key, _) in indexes {
        let mark = format!(":{key}");
        if !value.contains(&mark) {
            continue;
        }
        value = value
            .replace(&format!(":{key}*"), &format!(":{key}--ESCAPED_PARAM_ASTERISK"))
            .replace(&format!(":{key}?"), &format!(":{key}--ESCAPED_PARAM_QUESTION"))
            .replace(&format!(":{key}+"), &format!(":{key}--ESCAPED_PARAM_PLUS"));
        value = replace_not_before_word(&value, &mark, &format!("--ESCAPED_PARAM_COLON{key}"));
    }
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, ':' | '*' | '?' | '+' | '(' | ')' | '{' | '}') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    let value = escaped
        .replace("--ESCAPED_PARAM_PLUS", "+")
        .replace("--ESCAPED_PARAM_COLON", ":")
        .replace("--ESCAPED_PARAM_QUESTION", "?")
        .replace("--ESCAPED_PARAM_ASTERISK", "*");
    let out = p2r::compile(&format!("/{value}"), indexes)?;
    Ok(out[1..].to_owned())
}

/// Replace `needle` wherever the character after it isn't a word character (`(?!\w)`).
fn replace_not_before_word(haystack: &str, needle: &str, with: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(at) = rest.find(needle) {
        let after = &rest[at + needle.len()..];
        out.push_str(&rest[..at]);
        if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            out.push_str(needle);
        } else {
            out.push_str(with);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The parts of Node's legacy `url.parse(text, true)` and `url.format` that routing-utils relies on
/// to take a destination apart and put it back together.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct NodeUrl {
    pub protocol: Option<String>,
    pub slashes: bool,
    pub auth: Option<String>,
    pub hostname: Option<String>,
    pub port: Option<String>,
    pub pathname: Option<String>,
    /// The query string, parsed: keys in order of first appearance, each with its values.
    pub query: Vec<(String, Vec<String>)>,
    pub hash: Option<String>,
}

const AUTO_ESCAPE: &[char] = &['{', '}', '|', '\\', '^', '`', '\'', '<', '>', '"', ' ', '\r', '\n', '\t'];

impl NodeUrl {
    pub fn parse(text: &str) -> NodeUrl {
        let mut url = NodeUrl::default();
        let mut rest = text.trim().to_owned();
        // The protocol.
        let proto_len = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if proto_len > 0 && rest[proto_len..].starts_with(':') {
            url.protocol = Some(rest[..=proto_len].to_ascii_lowercase());
            rest = rest[proto_len + 1..].to_owned();
        }
        let slashed = matches!(url.protocol.as_deref(), Some("http:" | "https:" | "ftp:" | "gopher:" | "file:" | "ws:" | "wss:"));
        if url.protocol.is_some() && rest.starts_with("//") {
            url.slashes = true;
            rest = rest[2..].to_owned();
        }
        if url.slashes || (url.protocol.is_some() && !slashed) {
            // The host ends at the first character that can't be in one.
            let end = rest.find(['/', '?', '#', '\\', ';', '%', ' ', '<', '>', '"', '`', '{', '}', '|', '^', '\'']).unwrap_or(rest.len());
            let mut host = rest[..end].to_owned();
            rest = rest[end..].to_owned();
            if let Some(at) = host.rfind('@') {
                url.auth = Some(percent_decode(&host[..at]));
                host = host[at + 1..].to_owned();
            }
            if let Some(colon) = host.rfind(':')
                && host[colon + 1..].bytes().all(|b| b.is_ascii_digit())
            {
                let port = &host[colon + 1..];
                if !port.is_empty() {
                    url.port = Some(port.to_owned());
                }
                host.truncate(colon);
            }
            // An IPv6 literal's brackets aren't the hostname's (Node's `url.parse`); `format` adds
            // them back.
            if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
                host = inner.to_owned();
            }
            url.hostname = Some(host.to_ascii_lowercase());
        }
        let mut escaped = String::with_capacity(rest.len());
        for c in rest.chars() {
            if AUTO_ESCAPE.contains(&c) {
                escaped.push_str(&format!("%{:02X}", c as u32));
            } else {
                escaped.push(c);
            }
        }
        let mut rest = escaped.as_str();
        if let Some(at) = rest.find('#') {
            url.hash = Some(rest[at..].to_owned());
            rest = &rest[..at];
        }
        if let Some(at) = rest.find('?') {
            url.query = parse_query(&rest[at + 1..]);
            rest = &rest[..at];
        }
        if !rest.is_empty() {
            url.pathname = Some(rest.to_owned());
        } else if slashed && url.hostname.is_some() {
            url.pathname = Some("/".to_owned());
        }
        url
    }

    pub fn format(&self) -> String {
        let auth = self.auth.as_ref().map(|a| format!("{}@", query_escape(a).replacen("%3A", ":", 1))).unwrap_or_default();
        let mut protocol = self.protocol.clone().unwrap_or_default();
        if !protocol.is_empty() && !protocol.ends_with(':') {
            protocol.push(':');
        }
        let mut pathname = self.pathname.clone().unwrap_or_default().replace('?', "%3F").replace('#', "%23");
        let host = self.hostname.as_ref().map(|h| {
            let mut host = format!("{auth}{}", if h.contains(':') { format!("[{h}]") } else { h.clone() });
            if let Some(port) = &self.port {
                host.push(':');
                host.push_str(port);
            }
            host
        });
        let slashed = matches!(protocol.as_str(), "http:" | "https:" | "ftp:" | "gopher:" | "file:" | "ws:" | "wss:");
        let host = if self.slashes || ((protocol.is_empty() || slashed) && host.is_some()) {
            if !pathname.is_empty() && !pathname.starts_with('/') {
                pathname.insert(0, '/');
            }
            format!("//{}", host.unwrap_or_default())
        } else {
            host.unwrap_or_default()
        };
        let query = stringify_query(&self.query);
        let search = if query.is_empty() { String::new() } else { format!("?{query}").replacen('#', "%23", 1) };
        let hash = match &self.hash {
            Some(h) if !h.is_empty() && !h.starts_with('#') => format!("#{h}"),
            Some(h) => h.clone(),
            None => String::new(),
        };
        format!("{protocol}{host}{pathname}{search}{hash}")
    }
}

/// Node's `querystring.parse`: `+` is a space, `%XX` decoded, a key with no `=` has the value "".
pub fn parse_query(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for piece in text.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = piece.split_once('=').unwrap_or((piece, ""));
        let (k, v) = (query_unescape(k), query_unescape(v));
        match out.iter_mut().find(|(key, _)| *key == k) {
            Some((_, values)) => values.push(v),
            None => out.push((k, vec![v])),
        }
    }
    out
}

/// Node's `querystring.stringify`.
pub fn stringify_query(query: &[(String, Vec<String>)]) -> String {
    let mut parts = Vec::new();
    for (k, values) in query {
        for v in values {
            parts.push(format!("{}={}", query_escape(k), query_escape(v)));
        }
    }
    parts.join("&")
}

/// Node's `querystring.escape`: everything but letters, digits and `-_.!~*'()` percent-encoded.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Node's `querystring.unescape`: `+` as a space, then `%XX` decoded where it makes valid UTF-8.
pub fn query_unescape(s: &str) -> String {
    percent_decode(&s.replace('+', " "))
}

/// `%XX` decoded; malformed escapes stay as they are, and bytes that aren't UTF-8 become U+FFFD.
pub fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// A route from `routes` as `normalizeRoutes` leaves it: `src` anchored, `source`, `destination`
/// and `statusCode` accepted for `src`, `dest` and `status`.
pub fn legacy_route(mut route: SourceRoute) -> SourceRoute {
    route.src = normalize_src(&route.src);
    route
}

/// `{ handle: "filesystem" }` between what runs before the filesystem and the rewrites.
pub fn filesystem() -> Route {
    Route::Handle { handle: super::Phase::Filesystem }
}
