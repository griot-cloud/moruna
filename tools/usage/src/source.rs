//! What the report reads from the source tree the usage build was built from: a dispatcher's
//! match arms, a module of numbered constants, and functions' spans and the call numbers they
//! name. Plain text scanning of rustfmt-formatted code, enough for the files it is pointed at.

use std::collections::BTreeMap;

/// One match arm of a dispatcher: `P::A | P::B [if ...] => body`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arm {
    /// The constants the arm matches, without the prefix.
    pub names: Vec<String>,
    /// Its line, 1-based.
    pub line: usize,
    /// The 1-based column of its `=>`.
    pub arrow: usize,
    /// The text after `=>` on its line.
    pub body: String,
}

/// A function's span in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub name: String,
    /// First and last line, 1-based.
    pub first: usize,
    pub last: usize,
}

impl Span {
    pub fn lines(&self) -> usize {
        self.last + 1 - self.first
    }
}

/// The text with string literals and line comments blanked to spaces, so braces in them do
/// not count. Lines and columns are kept.
fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            match c {
                '\\' => {
                    out.push(' ');
                    if let Some(n) = chars.next() {
                        out.push(if n == '\n' { '\n' } else { ' ' });
                    }
                }
                '"' => {
                    in_string = false;
                    out.push(' ');
                }
                '\n' => out.push('\n'),
                _ => out.push(' '),
            }
        } else if c == '"' {
            in_string = true;
            out.push(' ');
        } else if c == '/' && chars.peek() == Some(&'/') {
            out.push(' ');
            for n in chars.by_ref() {
                if n == '\n' {
                    out.push('\n');
                    break;
                }
                out.push(' ');
            }
        } else if c == '\'' {
            // A char literal such as '{' (not a lifetime): blank it.
            let rest: String = chars.clone().take(3).collect();
            let close = if rest.starts_with('\\') {
                rest.get(2..3)
            } else {
                rest.get(1..2)
            };
            if close == Some("'") {
                out.push(' ');
                let n = if rest.starts_with('\\') { 3 } else { 2 };
                for _ in 0..n {
                    chars.next();
                    out.push(' ');
                }
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The 0-based line range of the body of the first `fn NAME` in `code` (from its `fn` line to
/// the line of its closing brace).
fn body_lines(lines: &[&str], name: &str) -> Option<(usize, usize)> {
    let needle_paren = format!("fn {name}(");
    let needle_generic = format!("fn {name}<");
    let start = lines
        .iter()
        .position(|l| l.contains(&needle_paren) || l.contains(&needle_generic))?;
    let mut depth = 0i64;
    // Parentheses and brackets in the signature, so `[u64; 6]` is not a declaration's end.
    let mut nest = 0i64;
    let mut opened = false;
    for (i, line) in lines.iter().enumerate().skip(start) {
        for c in line.chars() {
            match c {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                '(' | '[' => nest += 1,
                ')' | ']' => nest -= 1,
                ';' if !opened && depth == 0 && nest == 0 => return None,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return Some((start, i));
        }
    }
    None
}

/// The span of the first `fn NAME` in `text`.
#[cfg(test)]
pub fn function(text: &str, name: &str) -> Option<Span> {
    let code = code_only(text);
    let lines: Vec<&str> = code.lines().collect();
    body_lines(&lines, name).map(|(a, b)| Span {
        name: name.to_string(),
        first: a + 1,
        last: b + 1,
    })
}

/// Every function in `text` with its span and the distinct `PREFIX::NAME` constants its body
/// names, in order of appearance.
pub fn functions_naming(text: &str, prefix: &str) -> Vec<(Span, Vec<String>)> {
    let code = code_only(text);
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(at) = line.find("fn ") else { continue };
        if at > 0 && !line[..at].ends_with(' ') {
            continue;
        }
        let name: String = line[at + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let Some((first, last)) = body_lines(&lines[i..], &name) else {
            continue;
        };
        let (first, last) = (first + i, last + i);
        let mut names = Vec::new();
        for l in &lines[first..=last] {
            for n in constants(l, prefix) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        out.push((
            Span {
                name,
                first: first + 1,
                last: last + 1,
            },
            names,
        ));
    }
    out
}

/// The `PREFIX::NAME` constants on a line.
fn constants(line: &str, prefix: &str) -> Vec<String> {
    let needle = format!("{prefix}::");
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find(&needle) {
        let before_ok = at == 0
            || !rest[..at]
                .chars()
                .last()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':');
        let tail = &rest[at + needle.len()..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if before_ok && !name.is_empty() {
            out.push(name.clone());
        }
        rest = &tail[name.len()..];
    }
    out
}

/// The match arms on `PREFIX::NAME` constants in the body of `fn FUNCTION`.
pub fn arms(text: &str, function: &str, prefix: &str) -> Vec<Arm> {
    let code = code_only(text);
    let lines: Vec<&str> = code.lines().collect();
    let Some((first, last)) = body_lines(&lines, function) else {
        return Vec::new();
    };
    let original: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate().take(last + 1).skip(first) {
        let Some(arrow) = line.find("=>") else {
            continue;
        };
        let pattern = line[..arrow].trim();
        let pattern = match pattern.find(" if ") {
            Some(at) => &pattern[..at],
            None => pattern,
        };
        let parts: Vec<&str> = pattern.split('|').map(str::trim).collect();
        let mut names = Vec::new();
        for part in &parts {
            let Some(name) = part.strip_prefix(&format!("{prefix}::")) else {
                names.clear();
                break;
            };
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                names.clear();
                break;
            }
            names.push(name.to_string());
        }
        if names.is_empty() {
            continue;
        }
        let body = original
            .get(i)
            .map_or("", |l| l.get(arrow + 2..).unwrap_or(""))
            .trim();
        out.push(Arm {
            names,
            line: i + 1,
            arrow: arrow + 1,
            body: body.to_string(),
        });
    }
    out
}

/// The numbered constants of `pub mod MODULE { pub const NAME: u64 = EXPR; }` in `text`, where
/// EXPR is a literal (decimal, hex, with `_`), a constant named before, or a sum of those.
pub fn numbers(text: &str, module: &str) -> BTreeMap<String, u64> {
    let code = code_only(text);
    let lines: Vec<&str> = code.lines().collect();
    let mut out = BTreeMap::new();
    let header = format!("pub mod {module} {{");
    let Some(start) = lines
        .iter()
        .position(|l| l.trim_start().starts_with(&header))
    else {
        return out;
    };
    let mut depth = 0i64;
    let mut statement = String::new();
    for line in &lines[start..] {
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        let t = line.trim();
        if t.starts_with("pub const ") || !statement.is_empty() {
            statement.push(' ');
            statement.push_str(t);
            if t.ends_with(';') {
                if let Some((name, value)) = constant(&statement, &out) {
                    out.insert(name, value);
                }
                statement.clear();
            }
        }
        if depth <= 0 {
            break;
        }
    }
    out
}

fn literal(text: &str) -> Option<u64> {
    let t = text.trim().replace('_', "");
    let t = t.strip_suffix("u64").unwrap_or(&t);
    match t.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => t.parse().ok(),
    }
}

fn constant(statement: &str, known: &BTreeMap<String, u64>) -> Option<(String, u64)> {
    let rest = statement.trim().strip_prefix("pub const ")?;
    let (name, rest) = rest.split_once(':')?;
    let (ty, expr) = rest.split_once('=')?;
    if !matches!(ty.trim(), "u64" | "u32" | "usize" | "u16" | "u8") {
        return None;
    }
    let expr = expr.trim().strip_suffix(';')?;
    let mut sum = 0u64;
    for term in expr.split('+') {
        let term = term.trim();
        let value = literal(term).or_else(|| known.get(term).copied())?;
        sum = sum.checked_add(value)?;
    }
    Some((name.trim().to_string(), sum))
}

/// The function an arm's body calls first, as a path (`datapath::thread_set_cpu`,
/// `handle_close`, `self.read`), if it starts with a call.
pub fn called(body: &str) -> Option<String> {
    let body = body
        .trim_start_matches("return ")
        .trim_start_matches('{')
        .trim();
    let body = body.strip_prefix("match ").unwrap_or(body);
    let end = body.find('(')?;
    let path = body[..end].trim();
    let ok = !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '.');
    (ok && !path.ends_with("::") && path.chars().next().is_some_and(|c| c.is_lowercase()))
        .then(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPATCH: &str = r#"
pub fn dispatch(thread: &Thread, ctx: &mut UserContext) -> Flow {
    let s = "{ not a brace }"; // nor this {
    let c = '{';
    let result = match number {
        nr::ABI_VERSION => one(ABI_VERSION),
        nr::HANDLE_CLOSE => handle_close(p, a[0]),
        nr::PROCESS_EXIT => return Flow::ProcessExit(a[0] as i64),
        nr::THREAD_SET_CPU => datapath::thread_set_cpu(thread, a).map_err(Into::into),
        nr::READ if stream == 0 => return self.read_plane(token),
        nr::FSYNC | nr::FDATASYNC => self.with_fd(a[0], |_| Ok(0)).into(),
        other::THING => nothing(),
        _ => Err(Error::BadSyscall.into()),
    };
}

fn handle_close(p: &Process, v: u64) -> Ret {
    p.close(v)
}
"#;

    #[test]
    fn a_dispatchers_arms_are_found_with_their_lines() {
        let arms = arms(DISPATCH, "dispatch", "nr");
        let names: Vec<_> = arms.iter().map(|a| a.names.join("|")).collect();
        assert_eq!(
            names,
            [
                "ABI_VERSION",
                "HANDLE_CLOSE",
                "PROCESS_EXIT",
                "THREAD_SET_CPU",
                "READ",
                "FSYNC|FDATASYNC"
            ]
        );
        assert_eq!(arms[1].line, 7);
        assert_eq!(arms[1].arrow, 26);
        assert_eq!(arms[1].body, "handle_close(p, a[0]),");
        assert_eq!(called(&arms[1].body).as_deref(), Some("handle_close"));
        assert_eq!(
            called(&arms[3].body).as_deref(),
            Some("datapath::thread_set_cpu")
        );
        assert_eq!(called(&arms[4].body).as_deref(), Some("self.read_plane"));
        assert_eq!(called(&arms[2].body), None);
        assert_eq!(
            called("match self.eventfd_of(a[0]) {").as_deref(),
            Some("self.eventfd_of")
        );
        assert!(super::arms(DISPATCH, "missing", "nr").is_empty());
    }

    #[test]
    fn a_functions_span_skips_braces_in_strings_and_comments() {
        let span = function(DISPATCH, "dispatch").unwrap();
        assert_eq!((span.first, span.last, span.lines()), (2, 15, 14));
        let close = function(DISPATCH, "handle_close").unwrap();
        assert_eq!((close.first, close.last), (17, 19));
        assert_eq!(function("fn decl(a: u8);\n", "decl"), None);
        let array = "fn call(a: [u64; 6]) -> u64 {\n    a[0]\n}\n";
        assert_eq!(function(array, "call").map(|s| s.last), Some(3));
        assert_eq!(function("fn open(a: u8) {\n", "open"), None);
        let escaped = "fn e() {\n    let s = \"a\\\"{\";\n    let c = '\\'';\n}\n";
        assert_eq!(function(escaped, "e").map(|s| s.last), Some(4));
    }

    #[test]
    fn functions_name_their_constants() {
        let text = "pub fn now() -> u64 {\n    raw::call0(nr::CLOCK_GET).0\n}\n\
                    pub fn both() {\n    x(nr::A, nr::B, nr::A, snr::C);\n}\n\
                    pub fn generic<T>(t: T) {\n    y(nr::WAIT)\n}\n";
        let found = functions_naming(text, "nr");
        let pairs: Vec<_> = found
            .iter()
            .map(|(s, n)| (s.name.as_str(), n.clone()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("now", vec!["CLOCK_GET".to_string()]),
                ("both", vec!["A".to_string(), "B".to_string()]),
                ("generic", vec!["WAIT".to_string()]),
            ]
        );
        assert_eq!(found[0].0.first, 1);
        assert!(functions_naming("let f = |x| x;\nconfn x() {}\n", "nr").is_empty());
    }

    #[test]
    fn numbered_constants_are_evaluated() {
        let text = "pub mod nr {\n    /// The first.\n    pub const BASE: u64 = 0x1_0000;\n\
                    pub const ABI_VERSION: u64 = BASE;\n    pub const HANDLE_CLOSE: u64 = BASE + 1;\n\
                    pub const LONG: u64 =\n        BASE + 0x10;\n    pub const NAME: &str = \"x\";\n\
                    pub const BAD: u64 = OTHER + 1;\n}\npub const OUTSIDE: u64 = 9;\n";
        let n = numbers(text, "nr");
        assert_eq!(n["BASE"], 0x1_0000);
        assert_eq!(n["ABI_VERSION"], 0x1_0000);
        assert_eq!(n["HANDLE_CLOSE"], 0x1_0001);
        assert_eq!(n["LONG"], 0x1_0010);
        assert!(!n.contains_key("NAME") && !n.contains_key("BAD") && !n.contains_key("OUTSIDE"));
        assert!(numbers(text, "missing").is_empty());
        assert_eq!(literal("12u64"), Some(12));
    }
}
