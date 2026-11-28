//! Applying a decision's `transforms`: the request's headers, query and path before it goes on,
//! the answer's headers on the way back.

use http::{HeaderMap, HeaderName, HeaderValue};

use super::engine::Query;
use super::{KeyMatch, Ops, Transform, TransformKind, TransformOp};

/// Whether a key's name passes a transform's target.
fn key_matches(key: &KeyMatch, name: &str, header: bool) -> bool {
    match key {
        KeyMatch::Name(n) if header => n.eq_ignore_ascii_case(name),
        KeyMatch::Name(n) => n == name,
        KeyMatch::Ops(ops) => ops_hold(ops, name),
    }
}

/// The comparisons of a transform's `target.key`, on a name. Text comparisons ignore case, as
/// in `has`.
fn ops_hold(ops: &Ops, value: &str) -> bool {
    let lower = value.to_lowercase();
    let same = |s: &String| s.to_lowercase() == lower;
    let number = value.trim().parse::<f64>().ok();
    ops.eq.as_ref().is_none_or(|eq| match eq {
        serde_json::Value::String(s) => same(s),
        serde_json::Value::Number(n) => number.is_some_and(|v| Some(v) == n.as_f64()),
        _ => false,
    }) && ops.neq.as_ref().is_none_or(|s| !same(s))
        && ops.inc.as_ref().is_none_or(|list| list.iter().any(same))
        && ops.ninc.as_ref().is_none_or(|list| !list.iter().any(same))
        && ops.pre.as_ref().is_none_or(|p| lower.starts_with(&p.to_lowercase()))
        && ops.suf.as_ref().is_none_or(|s| lower.ends_with(&s.to_lowercase()))
        && ops.gt.is_none_or(|g| number.is_some_and(|v| v > g))
        && ops.gte.is_none_or(|g| number.is_some_and(|v| v >= g))
        && ops.lt.is_none_or(|l| number.is_some_and(|v| v < l))
        && ops.lte.is_none_or(|l| number.is_some_and(|v| v <= l))
}

/// Apply the transforms of `kind` (`request.headers` or `response.headers`) to `headers`.
/// `set` replaces the header with `args`; `append` adds `args` after its values (setting it if
/// it's missing); `delete` removes it, or with `args` only those values.
pub fn apply_headers(transforms: &[Transform], kind: TransformKind, headers: &mut HeaderMap) {
    // A request's headers the router sets or reads (its loop mark, a schedule's run, the client's
    // address) stay as it made them, as they do for a middleware's overrides.
    let reserved = |name: &str| kind == TransformKind::RequestHeaders && (super::middleware::keeps(name) || matches!(name, "forwarded" | "true-client-ip" | "x-forwarded-port"));
    for t in transforms.iter().filter(|t| t.kind == kind) {
        let Some(target) = &t.target else { continue };
        let values = || t.args.iter().filter_map(|a| HeaderValue::from_str(a).ok());
        match (t.op, &target.key) {
            (TransformOp::Set, KeyMatch::Name(name)) => {
                let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else { continue };
                if reserved(name.as_str()) {
                    continue;
                }
                headers.remove(&name);
                for v in values() {
                    headers.append(name.clone(), v);
                }
            }
            (TransformOp::Set, KeyMatch::Ops(_)) => {}
            (TransformOp::Append, key) => {
                let mut names: Vec<HeaderName> = headers.keys().filter(|n| key_matches(key, n.as_str(), true) && !reserved(n.as_str())).cloned().collect();
                if names.is_empty()
                    && let KeyMatch::Name(name) = key
                    && let Ok(name) = HeaderName::from_bytes(name.as_bytes())
                    && !reserved(name.as_str())
                {
                    names.push(name);
                }
                for name in names {
                    for v in values() {
                        headers.append(name.clone(), v);
                    }
                }
            }
            (TransformOp::Delete, key) => {
                let names: Vec<HeaderName> = headers.keys().filter(|n| key_matches(key, n.as_str(), true) && !reserved(n.as_str())).cloned().collect();
                for name in names {
                    if t.args.is_empty() {
                        headers.remove(&name);
                        continue;
                    }
                    let kept: Vec<HeaderValue> = headers
                        .get_all(&name)
                        .iter()
                        .filter_map(|v| {
                            let text = v.to_str().ok()?;
                            let rest: Vec<&str> = text.split(',').map(str::trim).filter(|p| !t.args.iter().any(|a| a == p)).collect();
                            (!rest.is_empty()).then(|| HeaderValue::from_str(&rest.join(", ")).ok()).flatten()
                        })
                        .collect();
                    headers.remove(&name);
                    for v in kept {
                        headers.append(name.clone(), v);
                    }
                }
            }
        }
    }
}

/// The query string after the `request.query` transforms, encoded as Vercel passes queries on;
/// `None` when there are none.
pub fn apply_query(transforms: &[Transform], raw: &str) -> Option<String> {
    let mut list = transforms.iter().filter(|t| t.kind == TransformKind::RequestQuery).peekable();
    list.peek()?;
    let mut query = Query::parse(raw);
    for t in list {
        apply_one(t, &mut query);
    }
    Some(query.encode())
}

/// One `request.query` transform, on the query as it stands.
pub(super) fn apply_one(t: &Transform, query: &mut Query) {
    let as_text = |k: &[u8]| String::from_utf8_lossy(k).into_owned();
    {
        let Some(target) = &t.target else { return };
        let args = || t.args.iter().map(|a| a.as_bytes().to_vec());
        match (t.op, &target.key) {
            (TransformOp::Set, KeyMatch::Name(name)) => {
                let key = name.as_bytes().to_vec();
                let at = query.0.iter().position(|(k, _)| *k == key);
                query.0.retain(|(k, _)| *k != key);
                let new: Vec<_> = args().map(|v| (key.clone(), Some(v))).collect();
                let at = at.unwrap_or(query.0.len()).min(query.0.len());
                query.0.splice(at..at, new);
            }
            (TransformOp::Set, KeyMatch::Ops(_)) => {}
            (TransformOp::Append, key) => {
                let mut keys: Vec<Vec<u8>> = Vec::new();
                for (k, _) in &query.0 {
                    if key_matches(key, &as_text(k), false) && !keys.contains(k) {
                        keys.push(k.clone());
                    }
                }
                if keys.is_empty()
                    && let KeyMatch::Name(name) = key
                {
                    keys.push(name.as_bytes().to_vec());
                }
                for k in keys {
                    let last = query.0.iter().rposition(|(q, _)| *q == k).map(|i| i + 1).unwrap_or(query.0.len());
                    let new: Vec<_> = args().map(|v| (k.clone(), Some(v))).collect();
                    query.0.splice(last..last, new);
                }
            }
            (TransformOp::Delete, key) => {
                query.0.retain(|(k, v)| {
                    if !key_matches(key, &as_text(k), false) {
                        return true;
                    }
                    if t.args.is_empty() {
                        return false;
                    }
                    let value = v.as_deref().map(as_text).unwrap_or_default();
                    !t.args.contains(&value)
                });
            }
        }
    }
}

/// The path the function sees, if a `request.path` transform sets one: the last one does.
pub fn request_path(transforms: &[Transform]) -> Option<&str> {
    transforms.iter().rev().find(|t| t.kind == TransformKind::RequestPath).and_then(|t| t.args.first()).map(String::as_str)
}
