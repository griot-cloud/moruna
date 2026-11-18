//! The little XML the ready report needs (work item 1.12): the text of the first element at a
//! few paths under a document's root, as Python's `ElementTree.findtext` reads it, and text
//! escaped for a document of its own.
//!
//! It reads elements, attributes (skipped), text, the five predefined entities and character
//! references, `CDATA` sections (as text), comments and processing instructions (skipped). A
//! `DOCTYPE` is refused, so no entity is ever defined or expanded. The document is UTF-8, and
//! an element's tags must match. The text of an element is what comes before its first child,
//! as `findtext` gives it.

use alloc::{string::String, vec::Vec};

/// How deep elements may nest.
pub const DEPTH_MAX: usize = 64;

/// One open element: its name, its text so far, and whether a child has begun.
struct Open<'a> {
    name: &'a str,
    text: String,
    child: bool,
}

/// The text of the first element at each of `paths` under the root element of `document`, or
/// `None` where there is none; or what is wrong with the document.
pub fn find_texts<const N: usize>(
    document: &[u8],
    paths: [&[&str]; N],
) -> Result<[Option<String>; N], &'static str> {
    let text = core::str::from_utf8(document).map_err(|_| "it is not UTF-8")?;
    let mut rest = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut found: [Option<String>; N] = core::array::from_fn(|_| None);
    let mut open: Vec<Open<'_>> = Vec::new();
    let mut root_seen = false;
    while !rest.is_empty() {
        let Some(at) = rest.find('<') else {
            // Text after the last tag.
            if !open.is_empty() {
                return Err("it ends inside an element");
            }
            if rest.trim_matches(BLANK).is_empty() {
                break;
            }
            return Err("it has text outside its root element");
        };
        let (before, tag) = rest.split_at(at);
        if !before.is_empty() {
            match open.last_mut() {
                Some(element) => {
                    if !element.child {
                        decode(before, &mut element.text)?;
                    }
                }
                None if before.trim_matches(BLANK).is_empty() => {}
                None => return Err("it has text outside its root element"),
            }
        }
        if let Some(after) = tag.strip_prefix("<!--") {
            let end = after.find("-->").ok_or("a comment does not end")?;
            rest = &after[end + 3..];
        } else if let Some(after) = tag.strip_prefix("<?") {
            let end = after
                .find("?>")
                .ok_or("a processing instruction does not end")?;
            rest = &after[end + 2..];
        } else if let Some(after) = tag.strip_prefix("<![CDATA[") {
            let end = after.find("]]>").ok_or("a CDATA section does not end")?;
            match open.last_mut() {
                Some(element) => {
                    if !element.child {
                        element.text.push_str(&after[..end]);
                    }
                }
                None => return Err("it has text outside its root element"),
            }
            rest = &after[end + 3..];
        } else if tag.starts_with("<!") {
            return Err("it has a DOCTYPE or a declaration this reader does not read");
        } else if let Some(after) = tag.strip_prefix("</") {
            let end = after.find('>').ok_or("an end tag does not end")?;
            let name = after[..end].trim_end_matches(BLANK);
            let element = open.pop().ok_or("an end tag closes nothing")?;
            if element.name != name {
                return Err("an end tag does not match its start tag");
            }
            record(&open, element.name, element.text, &paths, &mut found);
            rest = &after[end + 1..];
        } else {
            let after = &tag[1..];
            let end = tag_end(after).ok_or("a start tag does not end")?;
            let inside = &after[..end];
            let (inside, empty) = match inside.strip_suffix('/') {
                Some(inside) => (inside, true),
                None => (inside, false),
            };
            let name_end = inside.find(BLANK).unwrap_or(inside.len());
            let name = &inside[..name_end];
            if !is_name(name) {
                return Err("a tag's name is not an XML name");
            }
            if open.is_empty() {
                if root_seen {
                    return Err("it has more than one root element");
                }
                root_seen = true;
            }
            if let Some(parent) = open.last_mut() {
                parent.child = true;
            }
            if empty {
                record(&open, name, String::new(), &paths, &mut found);
            } else {
                if open.len() == DEPTH_MAX {
                    return Err("its elements nest too deep");
                }
                open.push(Open {
                    name,
                    text: String::new(),
                    child: false,
                });
            }
            rest = &after[end + 1..];
        }
    }
    if !open.is_empty() {
        return Err("it ends inside an element");
    }
    if !root_seen {
        return Err("it has no root element");
    }
    Ok(found)
}

const BLANK: [char; 4] = [' ', '\t', '\r', '\n'];

/// Keeps `text` as the element `name`'s, when it closes inside `open` at a path asked for and
/// none was found there before. The root itself is no path's.
fn record<const N: usize>(
    open: &[Open<'_>],
    name: &str,
    text: String,
    paths: &[&[&str]; N],
    found: &mut [Option<String>; N],
) {
    // The names from below the root to this element.
    let Some((_root, ancestors)) = open.split_first() else {
        return;
    };
    for (path, slot) in paths.iter().zip(found.iter_mut()) {
        let matches = path.len() == ancestors.len() + 1
            && path[..ancestors.len()]
                .iter()
                .zip(ancestors)
                .all(|(step, element)| *step == element.name)
            && path[ancestors.len()] == name;
        if matches && slot.is_none() {
            *slot = Some(text.clone());
        }
    }
}

/// Where a start tag's `>` is, past any quoted attribute value.
fn tag_end(after: &str) -> Option<usize> {
    let mut quote = None;
    for (at, character) in after.char_indices() {
        match (quote, character) {
            (None, '>') => return Some(at),
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), close) if open == close => quote = None,
            _ => {}
        }
    }
    None
}

/// Whether `name` is an XML name, as far as this reader needs: a letter, `_` or `:` first, then
/// letters, digits, `-`, `.`, `_` and `:`.
fn is_name(name: &str) -> bool {
    let mut characters = name.chars();
    let first = characters.next();
    first.is_some_and(|c| c.is_alphabetic() || c == '_' || c == ':')
        && characters.all(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'))
}

/// Appends `text` to `out` with its entities and character references replaced.
fn decode(text: &str, out: &mut String) -> Result<(), &'static str> {
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after.find(';').ok_or("an entity does not end")?;
        let entity = &after[..end];
        let character = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = match entity.strip_prefix("#x") {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => entity.strip_prefix('#').and_then(|n| n.parse().ok()),
                };
                number
                    .and_then(char::from_u32)
                    .ok_or("an entity is not one this reader knows")?
            }
        };
        out.push(character);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(())
}

/// `text` as an element's text in a document: `&`, `<` and `>` escaped, as `ElementTree` writes
/// them.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}
