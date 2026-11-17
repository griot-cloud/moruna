//! JSON read strictly, as node config and the hand-over need it: a field written twice is refused
//! (a plain JSON reader keeps the last and says nothing), an object is only ever an object (never
//! a list standing in for one), and every refusal says which field it is about.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::Refusal;

/// A JSON value, with an object's fields in the order they were written.
#[derive(Debug)]
pub(crate) enum Json {
    Null,
    Bool,
    /// A whole number from 0 to 2^64 - 1.
    Unsigned(u64),
    /// Any other number: negative, fractional, or too large.
    OtherNumber,
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Json, E> {
        Ok(Json::Bool)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Json, E> {
        Ok(Json::Unsigned(value))
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Json, E> {
        Ok(Json::OtherNumber)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Json, E> {
        Ok(Json::OtherNumber)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Json, E> {
        Ok(Json::String(value.to_string()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Json, A::Error> {
        let mut array = Vec::new();
        while let Some(item) = items.next_element()? {
            array.push(item);
        }
        Ok(Json::Array(array))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Json, A::Error> {
        let mut fields: Vec<(String, Json)> = Vec::new();
        while let Some((name, value)) = entries.next_entry::<String, Json>()? {
            if fields.iter().any(|(seen, _)| *seen == name) {
                return Err(de::Error::custom(format_args!(
                    "the field `{}` is written twice",
                    printable(&name, NAME_SHOWN)
                )));
            }
            fields.push((name, value));
        }
        Ok(Json::Object(fields))
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Json, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

/// How much of a field's name, or of the JSON reader's message, a refusal shows.
const NAME_SHOWN: usize = 32;
const MESSAGE_SHOWN: usize = 120;

/// `text` fit for one console line: at most `most` characters, each printable ASCII (anything
/// else becomes `?`). What a refusal quotes comes from outside the node.
pub(crate) fn printable(text: &str, most: usize) -> String {
    let mut shown: String = text
        .chars()
        .take(most)
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c
            } else {
                '?'
            }
        })
        .collect();
    if text.chars().nth(most).is_some() {
        shown.push_str("...");
    }
    shown
}

/// Reads `bytes` as one JSON value.
pub(crate) fn parse(bytes: &[u8]) -> Result<Json, Refusal> {
    serde_json::from_slice(bytes).map_err(|error| {
        let message = printable(&error.to_string(), MESSAGE_SHOWN);
        if error.is_data() {
            Refusal::Twice(message)
        } else {
            Refusal::NotJson(message)
        }
    })
}

/// An object's fields, and what the object is called in a refusal.
#[derive(Clone, Copy)]
pub(crate) struct Fields<'a> {
    within: &'static str,
    fields: &'a [(String, Json)],
}

impl<'a> Fields<'a> {
    /// The value of the field `name`, when the object has it.
    pub(crate) fn get(&self, name: &str) -> Option<&'a Json> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }

    /// The value of the field `name`, which the object must have; `path` names it in the refusal.
    pub(crate) fn need(&self, name: &str, path: &'static str) -> Result<&'a Json, Refusal> {
        self.get(name).ok_or(Refusal::Absent(path))
    }

    /// Refuses the object when it has a field that is none of `allowed`.
    pub(crate) fn only(&self, allowed: &[&str]) -> Result<(), Refusal> {
        match self
            .fields
            .iter()
            .find(|(field, _)| !allowed.contains(&field.as_str()))
        {
            None => Ok(()),
            Some((field, _)) => Err(Refusal::Unknown {
                within: self.within,
                field: printable(field, NAME_SHOWN),
            }),
        }
    }
}

impl Json {
    /// This value as an object; `within` names it in a refusal.
    pub(crate) fn object(&self, within: &'static str) -> Result<Fields<'_>, Refusal> {
        match self {
            Json::Object(fields) => Ok(Fields { within, fields }),
            _ => Err(Refusal::NotObject(within)),
        }
    }

    /// This value as a string; `path` names it in a refusal.
    pub(crate) fn string(&self, path: &'static str) -> Result<&str, Refusal> {
        match self {
            Json::String(text) => Ok(text),
            _ => Err(Refusal::NotString(path)),
        }
    }

    /// This value as a list of strings; `path` names it in a refusal.
    pub(crate) fn strings(&self, path: &'static str) -> Result<Vec<&str>, Refusal> {
        let Json::Array(items) = self else {
            return Err(Refusal::NotStrings(path));
        };
        items
            .iter()
            .map(|item| match item {
                Json::String(text) => Ok(text.as_str()),
                _ => Err(Refusal::NotStrings(path)),
            })
            .collect()
    }

    /// This value as a whole number; `path` names it in a refusal.
    pub(crate) fn unsigned(&self, path: &'static str) -> Result<u64, Refusal> {
        match self {
            Json::Unsigned(number) => Ok(*number),
            _ => Err(Refusal::NotNumber(path)),
        }
    }
}

/// A value's kind, for the refusal of a version that is not a whole number.
pub(crate) fn kind(json: &Json) -> String {
    match json {
        Json::Unsigned(number) => format!("{number}"),
        Json::String(_) => "a string".to_string(),
        Json::OtherNumber => "not a whole number".to_string(),
        Json::Null | Json::Bool | Json::Array(_) | Json::Object(_) => "not a number".to_string(),
    }
}
