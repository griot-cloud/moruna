//! An answer's body as the texts it is made of, written as it is sent. A large body, a job's
//! report or a page of its events, is held once, where it is, and is never copied whole: the
//! session writes it into its output a piece at a time as TLS takes it, an MCP tool's result
//! writes the same text twice (escaped as its text content, then as its structured content)
//! from the one copy, and a report stays shared with the job that keeps it.

use alloc::{string::String, sync::Arc, vec::Vec};

/// One text of a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Text {
    /// Text in the program.
    Static(&'static str),
    /// Text the body owns.
    Owned(String),
    /// Text kept elsewhere too, shared: a job's report.
    Shared(Arc<str>),
}

impl Text {
    /// The text.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Text::Static(text) => text,
            Text::Owned(text) => text,
            Text::Shared(text) => text,
        }
    }
}

/// How a text is written: as it is, or as the inside of a JSON string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    Raw,
    Escaped,
}

/// A body: its texts, the order they are written in, and how far the writing has got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Body {
    texts: Vec<Text>,
    /// Each step writes one text, by its index, in one form; a text may be written more than
    /// once.
    order: Vec<(usize, Form)>,
    /// The step being written, and how many of its text's bytes are written.
    step: usize,
    at: usize,
}

/// The escape JSON writes `byte` as inside a string, as `serde_json` writes it, or `None` when
/// it stands as it is.
fn escape(byte: u8) -> Option<[u8; 6]> {
    let short = |c: u8| Some([b'\\', c, 0, 0, 0, 0]);
    match byte {
        b'"' => short(b'"'),
        b'\\' => short(b'\\'),
        0x08 => short(b'b'),
        0x0c => short(b'f'),
        b'\n' => short(b'n'),
        b'\r' => short(b'r'),
        b'\t' => short(b't'),
        0..0x20 => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            Some([
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 0xf)],
            ])
        }
        _ => None,
    }
}

/// How many bytes of an escape: two, or six for `\u00XX`.
fn escape_len(escape: &[u8; 6]) -> usize {
    if escape[1] == b'u' { 6 } else { 2 }
}

impl Body {
    /// A body of one text, written as it is.
    pub(crate) fn of(text: Text) -> Self {
        let mut body = Self::default();
        body.push(text);
        body
    }

    /// A body of `bytes`, JSON as `serde_json` writes it: UTF-8, so never copied.
    pub(crate) fn json(bytes: Vec<u8>) -> Self {
        let text = String::from_utf8(bytes)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
        if text.is_empty() {
            return Self::default();
        }
        Self::of(Text::Owned(text))
    }

    /// Adds `text` without writing it yet, and gives its index for [`Body::write`].
    pub(crate) fn add(&mut self, text: Text) -> usize {
        self.texts.push(text);
        self.texts.len() - 1
    }

    /// Writes the text at `index` next, in `form`.
    pub(crate) fn write(&mut self, index: usize, form: Form) {
        self.order.push((index, form));
    }

    /// Writes `text` next, as it is.
    pub(crate) fn push(&mut self, text: Text) {
        let index = self.add(text);
        self.write(index, Form::Raw);
    }

    /// The body's one text, when it is one text written once as it is; otherwise the whole
    /// body, written into one text.
    pub(crate) fn into_text(mut self) -> Text {
        if let ([(0, Form::Raw)], [_]) = (self.order.as_slice(), self.texts.as_slice())
            && let Some(text) = self.texts.pop()
        {
            return text;
        }
        let mut whole = Vec::with_capacity(self.len());
        self.step = 0;
        self.at = 0;
        self.fill(&mut whole, usize::MAX);
        Text::Owned(String::from_utf8(whole).unwrap_or_default())
    }

    /// How many bytes the whole body is, as written.
    pub(crate) fn len(&self) -> usize {
        self.order
            .iter()
            .map(|&(index, form)| {
                let bytes = self.texts[index].as_str().as_bytes();
                match form {
                    Form::Raw => bytes.len(),
                    Form::Escaped => bytes
                        .iter()
                        .map(|&byte| escape(byte).map_or(1, |escape| escape_len(&escape)))
                        .sum(),
                }
            })
            .sum()
    }

    /// Writes the body on from where it has got to into `out`, until `out` holds `limit` bytes
    /// (an escape may pass it by five) or the body is all written; says whether it is.
    pub(crate) fn fill(&mut self, out: &mut Vec<u8>, limit: usize) -> bool {
        while let Some(&(index, form)) = self.order.get(self.step) {
            if out.len() >= limit {
                return false;
            }
            let bytes = self.texts[index].as_str().as_bytes();
            let rest = &bytes[self.at..];
            match form {
                Form::Raw => {
                    let count = rest.len().min(limit - out.len());
                    out.extend_from_slice(&rest[..count]);
                    self.at += count;
                }
                Form::Escaped => {
                    let mut written = 0;
                    while written < rest.len() && out.len() < limit {
                        // A run of bytes that stand as they are, then one escape.
                        let room = limit - out.len();
                        let run = rest[written..]
                            .iter()
                            .take(room)
                            .take_while(|&&byte| escape(byte).is_none())
                            .count();
                        out.extend_from_slice(&rest[written..written + run]);
                        written += run;
                        if let Some(&byte) = rest.get(written)
                            && out.len() < limit
                            && let Some(escape) = escape(byte)
                        {
                            out.extend_from_slice(&escape[..escape_len(&escape)]);
                            written += 1;
                        }
                    }
                    self.at += written;
                }
            }
            if self.at == bytes.len() {
                self.step += 1;
                self.at = 0;
            }
        }
        true
    }

    /// The whole body as written, from its start, whatever has been written so far.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut whole = self.clone();
        whole.step = 0;
        whole.at = 0;
        let mut out = Vec::new();
        whole.fill(&mut out, usize::MAX);
        out
    }
}

#[cfg(test)]
mod tests;
