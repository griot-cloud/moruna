//! A node's function images (system design 3E, item 3.6): what a job's start does with the
//! functions the job uses.
//!
//! - **Fetched** from the `https://` address each was registered with, through a [`Fetch`]: on a
//!   node, by `node-admission` itself, over a channel it mints for the address's host on port
//!   443, with the network server's `start_tls` (system design 3D, 3E); in a test, anything that
//!   serves bytes. A fetch is told the most bytes it may bring: the cache's whole size, since no
//!   image larger than that is kept or run.
//! - **Verified:** the image's SHA-256 must be the one registered, whose signature `api` checked
//!   at registration. An image changed since then is refused, naming the function
//!   ([`Refused::Altered`]), and never kept.
//! - **Kept** in memory for the next job ([`Cache`]), up to node config's `functions.cache_bytes`,
//!   the least recently used evicted first, until the node stops. An image is kept once by its
//!   SHA-256, whatever name or names it is registered under.
//!
//! What a job's start is handed ([`Cache::images`]) is each image's bytes, shared, in the order
//! the job names them, for `node-admission` to give the engine as a read-only memory object, the
//! image stage's image with no copy: an image evicted while a job still holds it lives until the
//! job lets go. `no_std`, with `alloc`, for `node-admission`.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::fmt;
use moruna_imagefs::sha256;

/// A function a job uses, as it was registered: its name, the address its image is fetched
/// from, and the image's SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registered {
    /// The name the job names it by.
    pub name: String,
    /// Where its image is fetched from: an `https://` URL.
    pub address: String,
    /// Its image's SHA-256.
    pub sha256: [u8; 32],
}

impl Registered {
    /// The function `name` at `address` whose image's SHA-256 is `sha256`, 64 lowercase
    /// hexadecimal digits, as a registration gives it; `None` when it is not that.
    pub fn new(name: &str, address: &str, sha256: &str) -> Option<Self> {
        let digits = sha256.as_bytes();
        if digits.len() != 64 {
            return None;
        }
        let value = |d: u8| match d {
            b'0'..=b'9' => Some(d - b'0'),
            b'a'..=b'f' => Some(d - b'a' + 10),
            _ => None,
        };
        let mut bytes = [0u8; 32];
        for (i, pair) in digits.chunks(2).enumerate() {
            bytes[i] = (value(pair[0])? << 4) | value(pair[1])?;
        }
        Some(Self {
            name: name.to_string(),
            address: address.to_string(),
            sha256: bytes,
        })
    }
}

/// Why a fetch brought no image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The image is longer than the most the fetch was allowed.
    TooLarge,
    /// The address could not be reached or read; the reason, for the job's failure.
    Failed(String),
}

/// How an image is fetched: on a node, through the job's network (item 3.4); in a test, from
/// memory.
pub trait Fetch {
    /// The whole object at `address`, at most `limit` bytes of it: a longer one is
    /// [`FetchError::TooLarge`], and the fetch stops at the limit rather than bring it all.
    fn fetch(&mut self, address: &str, limit: u64) -> Result<Vec<u8>, FetchError>;
}

/// Why a function's image is not run: the job is refused, and the reason names the function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The image's SHA-256 is not the one registered: it was changed after registration.
    Altered {
        /// The function's name.
        name: String,
        /// The SHA-256 registered.
        expected: [u8; 32],
        /// The SHA-256 of what the address served.
        found: [u8; 32],
    },
    /// The image is larger than the whole cache, the most the node holds.
    TooLarge {
        /// The function's name.
        name: String,
        /// The cache's size, in bytes.
        limit: u64,
    },
    /// The image could not be fetched.
    Unfetched {
        /// The function's name.
        name: String,
        /// Why, as the fetch said.
        why: String,
    },
}

/// `bytes` as lowercase hexadecimal digits.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl fmt::Display for Refused {
    /// One sentence that names the function, for the job's failure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Altered {
                name,
                expected,
                found,
            } => write!(
                f,
                "the function `{name}`'s image has changed since it was registered: its SHA-256 \
                 is {}, not the registered {}",
                hex(found),
                hex(expected)
            ),
            Refused::TooLarge { name, limit } => write!(
                f,
                "the function `{name}`'s image is larger than the node's function cache of \
                 {limit} bytes (node config's functions.cache_bytes)"
            ),
            Refused::Unfetched { name, why } => {
                write!(
                    f,
                    "the function `{name}`'s image could not be fetched: {why}"
                )
            }
        }
    }
}

/// One kept image.
#[derive(Debug)]
struct Entry {
    sha256: [u8; 32],
    bytes: Arc<[u8]>,
    /// When it was last handed to a job, on the cache's own count.
    used_at: u64,
}

/// The images kept between jobs: at most `capacity` bytes of them, the least recently used
/// evicted first.
#[derive(Debug)]
pub struct Cache {
    capacity: u64,
    held: u64,
    entries: Vec<Entry>,
    /// Counts each image handed to a job, to order them by use.
    clock: u64,
    fetches: u64,
    hits: u64,
}

impl Cache {
    /// An empty cache of `capacity` bytes: node config's `functions.cache_bytes`.
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            held: 0,
            entries: Vec::new(),
            clock: 0,
            fetches: 0,
            hits: 0,
        }
    }

    /// Its size, in bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The bytes of the images it keeps.
    pub fn held(&self) -> u64 {
        self.held
    }

    /// How many images it keeps.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it keeps none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether it keeps the image whose SHA-256 is `sha256`.
    pub fn holds(&self, sha256: &[u8; 32]) -> bool {
        self.entries.iter().any(|entry| entry.sha256 == *sha256)
    }

    /// How many images it has fetched, and how many times one was found kept.
    pub fn counts(&self) -> (u64, u64) {
        (self.fetches, self.hits)
    }

    /// The verified image of `function`: the kept one when the cache holds its SHA-256,
    /// otherwise fetched through `fetch`, checked, and kept.
    pub fn image(
        &mut self,
        function: &Registered,
        fetch: &mut dyn Fetch,
    ) -> Result<Arc<[u8]>, Refused> {
        self.clock += 1;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.sha256 == function.sha256)
        {
            entry.used_at = self.clock;
            self.hits += 1;
            return Ok(entry.bytes.clone());
        }
        let too_large = || Refused::TooLarge {
            name: function.name.clone(),
            limit: self.capacity,
        };
        self.fetches += 1;
        let bytes = match fetch.fetch(&function.address, self.capacity) {
            Ok(bytes) => bytes,
            Err(FetchError::TooLarge) => return Err(too_large()),
            Err(FetchError::Failed(why)) => {
                return Err(Refused::Unfetched {
                    name: function.name.clone(),
                    why,
                });
            }
        };
        if bytes.len() as u64 > self.capacity {
            return Err(too_large());
        }
        let found = sha256::digest(&bytes);
        if found != function.sha256 {
            return Err(Refused::Altered {
                name: function.name.clone(),
                expected: function.sha256,
                found,
            });
        }
        let bytes: Arc<[u8]> = Arc::from(bytes);
        self.keep(function.sha256, bytes.clone());
        Ok(bytes)
    }

    /// The verified images of every function a job uses, in the order it names them; the first
    /// that is refused refuses the job.
    pub fn images(
        &mut self,
        functions: &[Registered],
        fetch: &mut dyn Fetch,
    ) -> Result<Vec<Arc<[u8]>>, Refused> {
        functions
            .iter()
            .map(|function| self.image(function, fetch))
            .collect()
    }

    /// Keeps `bytes` under `sha256`, evicting the least recently used until they fit.
    fn keep(&mut self, sha256: [u8; 32], bytes: Arc<[u8]>) {
        let size = bytes.len() as u64;
        while self.held + size > self.capacity {
            let Some((oldest, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.used_at)
            else {
                break;
            };
            let evicted = self.entries.swap_remove(oldest);
            self.held -= evicted.bytes.len() as u64;
        }
        self.held += size;
        self.entries.push(Entry {
            sha256,
            bytes,
            used_at: self.clock,
        });
    }
}

#[cfg(test)]
mod tests;
