//! The registered functions a job uses, as the engine is given them (system design 3E and 5C;
//! item 3.6).
//!
//! `node-admission` fetches and verifies each function the job names and starts the engine
//! with one read-only memory object per function, the registered file itself, under
//! `function.N`, and an index under `functions` whose line `N NAME SHA256 SIZE` says which
//! function slot `N` holds (`node_protocol::functions`). The engine's supervisor hands both on
//! as they are (`grant functions functions`). Here the engine reads the index once, finds a
//! function by its registered name, and takes its slot the first time a stage asks: its file is
//! read in place, never copied, and never hashed again (admission checked its SHA-256).
//!
//! [`Index`] is the index's reading, the same on every target; [`function`] and [`Function`]
//! are the kernel's part.

use node_protocol::functions::{self as contract, Entry, IndexError};

pub use contract::{INDEX_MAX, INDEX_SLOT, MAX};

/// A job's functions as its index lists them, in slot order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    entries: Vec<Entry>,
}

/// What an index's refusal says.
fn refusal(e: IndexError) -> String {
    match e {
        IndexError::TooMany => format!(
            "the functions' index lists more than {MAX} functions, or is longer than \
             {INDEX_MAX} bytes"
        ),
        IndexError::Line(n, why) => format!("the functions' index, line {n}: {why}"),
        IndexError::Twice(name) => format!("the functions' index lists `{name}` twice"),
    }
}

impl Index {
    /// The index whose text is `text`, every line checked (`node_protocol::functions::parse`).
    pub fn parse(text: &str) -> Result<Index, String> {
        contract::parse(text)
            .map(|entries| Index { entries })
            .map_err(refusal)
    }

    /// The index an index's memory object holds, `bytes` from its first: its text ends at its
    /// first zero byte, within its first [`INDEX_MAX`] bytes.
    pub fn of_object(bytes: &[u8]) -> Result<Index, String> {
        let head = &bytes[..bytes.len().min(INDEX_MAX)];
        let Some(end) = head.iter().position(|b| *b == 0) else {
            if bytes.len() < INDEX_MAX {
                // A memory object is whole pages: an object this short holds its text whole.
                return Self::text(head);
            }
            return Err(format!(
                "the functions' index has no end within its first {INDEX_MAX} bytes"
            ));
        };
        Self::text(&head[..end])
    }

    fn text(bytes: &[u8]) -> Result<Index, String> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| String::from("the functions' index is not UTF-8"))?;
        Index::parse(text)
    }

    /// Its entries, in slot order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The function the job names `name`: its slot and its entry, or a refusal naming it when
    /// the engine was given none of that name.
    pub fn find(&self, name: &str) -> Result<(String, &Entry), String> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, e)| e.name == name)
            .map(|(n, e)| (contract::slot(n), e))
            .ok_or_else(|| {
                let given: Vec<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
                if given.is_empty() {
                    format!(
                        "the function `{name}` was not given to the engine: the node gave it no \
                         function"
                    )
                } else {
                    format!(
                        "the function `{name}` was not given to the engine: the node gave it {}",
                        given.join(", ")
                    )
                }
            })
    }
}

/// An entry's SHA-256 in lowercase hex, as the job's report and refusals name the file.
pub fn hex(entry: &Entry) -> String {
    entry.sha256.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use granted::{Function, function, index};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod granted {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, OnceLock};

    use moruna_sys::{Handle, Memory, Rights, map};

    use super::{Entry, INDEX_MAX, INDEX_SLOT, Index};
    use crate::layout::pages;

    /// The index the engine was granted under `functions`, read the first time a stage asks;
    /// an engine given no index has no functions.
    pub fn index() -> Result<&'static Index, String> {
        static INDEX: OnceLock<Result<Index, String>> = OnceLock::new();
        INDEX
            .get_or_init(|| {
                let Some(handle) = crate::start::take(INDEX_SLOT) else {
                    return Ok(Index::default());
                };
                let memory = Memory(handle);
                let view = memory
                    .map(0, INDEX_MAX as u64, map::READ)
                    .map_err(|e| format!("mapping the functions' index: {}", e.name()))?;
                let mut bytes = vec![0u8; INDEX_MAX];
                view.read(0, &mut bytes)
                    .map_err(|e| format!("reading the functions' index: {}", e.name()))?;
                Index::of_object(&bytes)
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// A registered function's file, as the engine was given it: a read-only memory object,
    /// mapped read only for as long as the engine runs.
    pub struct Function {
        /// Its slot, `function.N`.
        pub slot: String,
        /// Its line of the index: its name, SHA-256 and length.
        pub entry: Entry,
        memory: Memory,
        view: moruna_sys::Mapping<'static>,
    }

    impl std::fmt::Debug for Function {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Function")
                .field("slot", &self.slot)
                .field("name", &self.entry.name)
                .field("size", &self.entry.size)
                .finish()
        }
    }

    impl Function {
        /// The file's bytes, read in place: nothing writes a function's memory object once
        /// `node-admission` checked it, and the engine's handle cannot.
        pub fn bytes(&self) -> &[u8] {
            &self.view.bytes()[..self.entry.size as usize]
        }

        /// The memory object, for a call that reads it where it is (`process_map_image`).
        pub fn memory(&self) -> &Handle {
            &self.memory.0
        }

        /// A handle to the memory object with `rights`, to hand on.
        pub fn handle(&self, rights: Rights) -> Result<Handle, String> {
            self.memory.0.duplicate(rights).map_err(|e| {
                format!(
                    "the function `{}`'s memory object: {}",
                    self.entry.name,
                    e.name()
                )
            })
        }
    }

    /// The function the job names `name`, its slot taken and mapped the first time a stage
    /// asks: every stage after shares it.
    pub fn function(name: &str) -> Result<Arc<Function>, String> {
        static TAKEN: OnceLock<Mutex<BTreeMap<String, Arc<Function>>>> = OnceLock::new();
        let (slot, entry) = index()?.find(name)?;
        let mut taken = TAKEN
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| String::from("the functions taken: poisoned"))?;
        if let Some(function) = taken.get(&slot) {
            return Ok(Arc::clone(function));
        }
        let handle = crate::start::take(&slot).ok_or_else(|| {
            format!(
                "the functions' index lists `{name}` under `{slot}`, and the engine was not \
                 given `{slot}` (it holds {:?})",
                crate::start::slots()
            )
        })?;
        // Whole pages: a memory object is.
        let bytes = pages(entry.size)
            .ok_or_else(|| format!("the function `{name}` is {} bytes", entry.size))?;
        let memory = Memory(handle);
        let view = memory.map_detached(0, bytes, map::READ).map_err(|e| {
            format!(
                "the function `{name}` is {} bytes, and its memory object `{slot}` does not \
                 map as many: {}",
                entry.size,
                e.name()
            )
        })?;
        let function = Arc::new(Function {
            slot: slot.clone(),
            entry: entry.clone(),
            memory,
            view,
        });
        taken.insert(slot, Arc::clone(&function));
        Ok(function)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize, name: &str, byte: u8, size: u64) -> String {
        format!("{n} {name} {} {size}\n", format!("{byte:02x}").repeat(32))
    }

    #[test]
    fn a_function_is_found_by_its_registered_name_in_its_slot() {
        let text = line(0, "gate-score", 0xab, 4096) + &line(1, "taxi.clean", 7, 9_000_000);
        let index = Index::parse(&text).unwrap();
        assert_eq!(index.entries().len(), 2);
        let (slot, entry) = index.find("taxi.clean").unwrap();
        assert_eq!(slot, "function.1");
        assert_eq!((entry.size, entry.sha256), (9_000_000, [7; 32]));
        assert_eq!(hex(entry), "07".repeat(32));
        let (slot, _) = index.find("gate-score").unwrap();
        assert_eq!(slot, "function.0");
        let refused = index.find("taxi.score").unwrap_err();
        assert!(
            refused.contains("`taxi.score`") && refused.contains("gate-score, taxi.clean"),
            "{refused}"
        );
        let none = Index::default().find("taxi.score").unwrap_err();
        assert!(none.contains("gave it no function"), "{none}");
    }

    #[test]
    fn an_index_object_ends_at_its_first_zero_byte() {
        let text = line(0, "add_one", 1, 10);
        let mut object = vec![0u8; INDEX_MAX];
        object[..text.len()].copy_from_slice(text.as_bytes());
        let index = Index::of_object(&object).unwrap();
        assert_eq!(index.find("add_one").unwrap().0, "function.0");
        // An object with no function's line is an empty index.
        assert_eq!(Index::of_object(&[0u8; 64]).unwrap(), Index::default());
        // A short object holds its text whole.
        assert_eq!(Index::of_object(text.as_bytes()).unwrap(), index);
        // An object of text to its end, with no zero within the index's bytes, is refused.
        let full = vec![b'0'; INDEX_MAX + 1];
        assert!(Index::of_object(&full).unwrap_err().contains("no end"));
        let mut bad = object.clone();
        bad[0] = 0xff;
        assert!(Index::of_object(&bad).unwrap_err().contains("not UTF-8"));
    }

    #[test]
    fn every_refusal_of_an_index_names_what_is_wrong() {
        let twice = line(0, "a", 1, 1) + &line(1, "a", 2, 2);
        assert!(Index::parse(&twice).unwrap_err().contains("`a` twice"));
        let bad = Index::parse("0 a 00 1\n").unwrap_err();
        assert!(bad.contains("line 1") && bad.contains("SHA-256"), "{bad}");
        let many: String = (0..=MAX).map(|n| line(n, &format!("f{n}"), 1, 1)).collect();
        assert!(Index::parse(&many).unwrap_err().contains("more than 16"));
    }
}
