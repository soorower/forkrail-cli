//! A port of path-to-regexp 6.1.0, the version Vercel's routing-utils compiles `source` patterns
//! and fills `destination` templates with. Kept to what routing-utils calls (`pathToRegexp` with
//! `strict`, `delimiter: "/"`; `compile` with `validate: false`), and faithful to its quirks, so a
//! pattern means here what it means on Vercel.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
enum Lexed {
    Modifier(char),
    Escaped(char),
    Open,
    Close,
    Name(String),
    Pattern(String),
    Char(char),
    End,
}

impl Lexed {
    fn kind(&self) -> &'static str {
        match self {
            Lexed::Modifier(_) => "MODIFIER",
            Lexed::Escaped(_) => "ESCAPED_CHAR",
            Lexed::Open => "OPEN",
            Lexed::Close => "CLOSE",
            Lexed::Name(_) => "NAME",
            Lexed::Pattern(_) => "PATTERN",
            Lexed::Char(_) => "CHAR",
            Lexed::End => "END",
        }
    }
}

fn lexer(s: &str) -> Result<Vec<(Lexed, usize)>, String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        match ch {
            '*' | '+' | '?' => {
                out.push((Lexed::Modifier(ch), i));
                i += 1;
            }
            '\\' => {
                // A trailing backslash escapes nothing; JavaScript would read `undefined` there.
                out.push((Lexed::Escaped(c.get(i + 1).copied().unwrap_or('\\')), i));
                i += 2;
            }
            '{' => {
                out.push((Lexed::Open, i));
                i += 1;
            }
            '}' => {
                out.push((Lexed::Close, i));
                i += 1;
            }
            ':' => {
                let mut j = i + 1;
                let mut name = String::new();
                while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
                    name.push(c[j]);
                    j += 1;
                }
                if name.is_empty() {
                    return Err(format!("Missing parameter name at {i}"));
                }
                out.push((Lexed::Name(name), i));
                i = j;
            }
            '(' => {
                let mut count = 1;
                let mut pattern = String::new();
                let mut j = i + 1;
                if c.get(j) == Some(&'?') {
                    return Err(format!("Pattern cannot start with \"?\" at {j}"));
                }
                while j < c.len() {
                    if c[j] == '\\' {
                        pattern.push(c[j]);
                        if let Some(next) = c.get(j + 1) {
                            pattern.push(*next);
                        }
                        j += 2;
                        continue;
                    }
                    if c[j] == ')' {
                        count -= 1;
                        if count == 0 {
                            j += 1;
                            break;
                        }
                    } else if c[j] == '(' {
                        count += 1;
                        if c.get(j + 1) != Some(&'?') {
                            return Err(format!("Capturing groups are not allowed at {j}"));
                        }
                    }
                    pattern.push(c[j]);
                    j += 1;
                }
                if count != 0 {
                    return Err(format!("Unbalanced pattern at {i}"));
                }
                if pattern.is_empty() {
                    return Err(format!("Missing pattern at {i}"));
                }
                out.push((Lexed::Pattern(pattern), i));
                i = j;
            }
            _ => {
                out.push((Lexed::Char(ch), i));
                i += 1;
            }
        }
    }
    out.push((Lexed::End, c.len()));
    Ok(out)
}

/// A parameter's name: its own, or its position among the unnamed ones (`(.*)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyName {
    Named(String),
    Index(usize),
    /// A `{...}` group with neither a name nor a pattern: not a parameter.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub name: KeyName,
    pub prefix: String,
    pub suffix: String,
    pub pattern: String,
    pub modifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Text(String),
    Key(Key),
}

/// `parse(str, { delimiter })`.
pub fn parse(s: &str, delimiter: &str) -> Result<Vec<Token>, String> {
    let tokens = lexer(s)?;
    let prefixes = "./";
    let default_pattern = format!("[^{}]+?", escape(delimiter));
    let mut result = Vec::new();
    let mut key = 0usize;
    let mut i = 0usize;
    let mut path = String::new();

    fn try_char(tokens: &[(Lexed, usize)], i: &mut usize) -> Option<char> {
        match tokens.get(*i) {
            Some((Lexed::Char(c), _)) => {
                *i += 1;
                Some(*c)
            }
            _ => None,
        }
    }
    fn try_escaped(tokens: &[(Lexed, usize)], i: &mut usize) -> Option<char> {
        match tokens.get(*i) {
            Some((Lexed::Escaped(c), _)) => {
                *i += 1;
                Some(*c)
            }
            _ => None,
        }
    }
    fn try_name(tokens: &[(Lexed, usize)], i: &mut usize) -> Option<String> {
        match tokens.get(*i) {
            Some((Lexed::Name(n), _)) => {
                *i += 1;
                Some(n.clone())
            }
            _ => None,
        }
    }
    fn try_pattern(tokens: &[(Lexed, usize)], i: &mut usize) -> Option<String> {
        match tokens.get(*i) {
            Some((Lexed::Pattern(p), _)) => {
                *i += 1;
                Some(p.clone())
            }
            _ => None,
        }
    }
    fn try_modifier(tokens: &[(Lexed, usize)], i: &mut usize) -> String {
        match tokens.get(*i) {
            Some((Lexed::Modifier(m), _)) => {
                *i += 1;
                m.to_string()
            }
            _ => String::new(),
        }
    }
    fn consume_text(tokens: &[(Lexed, usize)], i: &mut usize) -> String {
        let mut out = String::new();
        while let Some(c) = try_char(tokens, i).or_else(|| try_escaped(tokens, i)) {
            out.push(c);
        }
        out
    }

    while i < tokens.len() {
        let ch = try_char(&tokens, &mut i);
        let name = try_name(&tokens, &mut i);
        let pattern = try_pattern(&tokens, &mut i);
        if name.is_some() || pattern.is_some() {
            let mut prefix = ch.map(String::from).unwrap_or_default();
            if !prefix.is_empty() && !prefixes.contains(prefix.as_str()) {
                path.push_str(&prefix);
                prefix.clear();
            }
            if !path.is_empty() {
                result.push(Token::Text(std::mem::take(&mut path)));
            }
            let name = match name {
                Some(n) => KeyName::Named(n),
                None => {
                    key += 1;
                    KeyName::Index(key - 1)
                }
            };
            result.push(Token::Key(Key {
                name,
                prefix,
                suffix: String::new(),
                pattern: pattern.unwrap_or_else(|| default_pattern.clone()),
                modifier: try_modifier(&tokens, &mut i),
            }));
            continue;
        }
        if let Some(value) = ch.or_else(|| try_escaped(&tokens, &mut i)) {
            path.push(value);
            continue;
        }
        if !path.is_empty() {
            result.push(Token::Text(std::mem::take(&mut path)));
        }
        if matches!(tokens.get(i), Some((Lexed::Open, _))) {
            i += 1;
            let prefix = consume_text(&tokens, &mut i);
            let name = try_name(&tokens, &mut i).unwrap_or_default();
            let pattern = try_pattern(&tokens, &mut i).unwrap_or_default();
            let suffix = consume_text(&tokens, &mut i);
            match tokens.get(i) {
                Some((Lexed::Close, _)) => i += 1,
                Some((other, at)) => return Err(format!("Unexpected {} at {at}, expected CLOSE", other.kind())),
                None => return Err("Unexpected end, expected CLOSE".into()),
            }
            let key_name = if !name.is_empty() {
                KeyName::Named(name.clone())
            } else if !pattern.is_empty() {
                key += 1;
                KeyName::Index(key - 1)
            } else {
                KeyName::None
            };
            result.push(Token::Key(Key {
                name: key_name,
                pattern: if !name.is_empty() && pattern.is_empty() { default_pattern.clone() } else { pattern },
                prefix,
                suffix,
                modifier: try_modifier(&tokens, &mut i),
            }));
            continue;
        }
        match tokens.get(i) {
            Some((Lexed::End, _)) => i += 1,
            Some((other, at)) => return Err(format!("Unexpected {} at {at}, expected END", other.kind())),
            None => break,
        }
    }
    Ok(result)
}

/// `escapeString`: a backslash before each of `.+*?=^!:${}()[]|/\`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".+*?=^!:${}()[]|/\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `pathToRegexp(source, keys, { strict: true, sensitive: true, delimiter: "/" })`, as
/// routing-utils' `sourceToRegex` calls it: the regex's source and the parameters' names, unnamed
/// ones as `None`.
pub fn source_to_regex(source: &str) -> Result<(String, Vec<Option<String>>), String> {
    let tokens = parse(source, "/")?;
    let mut route = String::from("^");
    let mut keys = Vec::new();
    for token in &tokens {
        match token {
            Token::Text(t) => route.push_str(&escape(t)),
            Token::Key(k) => {
                let prefix = escape(&k.prefix);
                let suffix = escape(&k.suffix);
                if !k.pattern.is_empty() {
                    keys.push(match &k.name {
                        KeyName::Named(n) => Some(n.clone()),
                        _ => None,
                    });
                    if !prefix.is_empty() || !suffix.is_empty() {
                        if k.modifier == "+" || k.modifier == "*" {
                            let m = if k.modifier == "*" { "?" } else { "" };
                            let _ = write!(route, "(?:{prefix}((?:{p})(?:{suffix}{prefix}(?:{p}))*){suffix}){m}", p = k.pattern);
                        } else {
                            let _ = write!(route, "(?:{prefix}({}){suffix}){}", k.pattern, k.modifier);
                        }
                    } else {
                        let _ = write!(route, "({}){}", k.pattern, k.modifier);
                    }
                } else {
                    let _ = write!(route, "(?:{prefix}{suffix}){}", k.modifier);
                }
            }
        }
    }
    // `strict`, so no optional trailing delimiter; `end`, so anchored.
    route.push('$');
    Ok((route, keys))
}

/// The names of a template's parameters (`pathToRegexp(template, keys)` for its keys only), as
/// routing-utils reads which ones a destination uses.
pub fn param_names(template: &str) -> Result<Vec<String>, String> {
    Ok(parse(template, "/#?")?
        .into_iter()
        .filter_map(|t| match t {
            Token::Key(Key { name: KeyName::Named(n), pattern, .. }) if !pattern.is_empty() => Some(n),
            _ => None,
        })
        .collect())
}

/// `compile(template, { validate: false })(values)`: the template with each parameter replaced by
/// its value. A parameter with no value fails unless it's optional (`?`, `*`).
pub fn compile(template: &str, values: &[(String, String)]) -> Result<String, String> {
    let tokens = parse(template, "/#?")?;
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Text(t) => out.push_str(&t),
            Token::Key(k) => {
                let name = match &k.name {
                    KeyName::Named(n) => n.clone(),
                    KeyName::Index(i) => i.to_string(),
                    KeyName::None => "undefined".to_owned(),
                };
                let value = values.iter().rev().find(|(n, _)| *n == name).map(|(_, v)| v.as_str());
                match value {
                    Some(v) => {
                        out.push_str(&k.prefix);
                        out.push_str(v);
                        out.push_str(&k.suffix);
                    }
                    None if k.modifier == "?" || k.modifier == "*" => {}
                    None => return Err(format!("Expected \"{name}\" to be a string")),
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_compile_as_on_vercel() {
        // What path-to-regexp 6.1.0 gives, with routing-utils' options.
        let cases: &[(&str, &str, &[Option<&str>])] = &[
            ("/about", "^\\/about$", &[]),
            ("/blog/:slug", "^\\/blog(?:\\/([^\\/]+?))$", &[Some("slug")]),
            ("/blog/:path*", "^\\/blog(?:\\/((?:[^\\/]+?)(?:\\/(?:[^\\/]+?))*))?$", &[Some("path")]),
            ("/blog/:path+", "^\\/blog(?:\\/((?:[^\\/]+?)(?:\\/(?:[^\\/]+?))*))$", &[Some("path")]),
            ("/(.*)", "^(?:\\/(.*))$", &[None]),
            ("/:id(\\d{1,})", "^(?:\\/(\\d{1,}))$", &[Some("id")]),
            ("/((?!uk/).*)", "^(?:\\/((?!uk\\/).*))$", &[None]),
            ("/post-:id?", "^\\/post-([^\\/]+?)?$", &[Some("id")]),
            ("/:a.:b", "^(?:\\/([^\\/]+?))(?:\\.([^\\/]+?))$", &[Some("a"), Some("b")]),
            ("/files{/:name}?", "^\\/files(?:\\/([^\\/]+?))?$", &[Some("name")]),
        ];
        for (source, regex, keys) in cases {
            // JavaScript's `RegExp.source` escapes every `/`, which routing-utils undoes.
            let (got, got_keys) = source_to_regex(source).unwrap();
            assert_eq!(got.replace("\\/", "/"), regex.replace("\\/", "/"), "{source}");
            let keys: Vec<Option<String>> = keys.iter().map(|k| k.map(str::to_owned)).collect();
            assert_eq!(got_keys, keys, "{source}");
        }
        assert!(source_to_regex("/(foo").is_err());
        assert!(source_to_regex("/:").is_err());
        assert!(source_to_regex("/(a(b))").is_err());
    }

    #[test]
    fn templates_fill_in() {
        let v = |pairs: &[(&str, &str)]| pairs.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect::<Vec<_>>();
        assert_eq!(compile("/news/:slug", &v(&[("slug", "$1")])).unwrap(), "/news/$1");
        assert_eq!(compile("/docs/:path*", &v(&[("path", "$1")])).unwrap(), "/docs/$1");
        assert_eq!(compile("/docs/:path*", &v(&[])).unwrap(), "/docs");
        assert!(compile("/docs/:path", &v(&[])).is_err());
        assert_eq!(param_names("/a/:x/b/:y*").unwrap(), vec!["x", "y"]);
    }
}
