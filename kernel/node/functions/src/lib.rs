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
//!   SHA-256, whatever name or names it is registered under. On a node it is kept in a memory
//!   object of its own ([`MemoryImage`]), made once when it is fetched, in the cache's account of
//!   exactly `functions.cache_bytes` (PM, item 3.6), so the job bound is honest.
//!
//! What a job's start is handed ([`Cache::images_kept`]) is each kept image, in the order the job
//! names them; [`handover`] makes of them the handles `node-admission` gives the engine (PM, item
//! 3.6): each image read only under `function.N`, with no copy, and the index of them under
//! `functions`. An image evicted while a job still holds it lives until the job lets go.
//! `no_std`, with `alloc`, for `node-admission`.
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
/// from, and the image's SHA-256. The address is whole, to fetch; its `Debug` shows it as
/// [`moruna_api::address_shown`] does, never its query (3E).
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for Registered {
    /// The function, its address as the node shows it: never its query.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registered")
            .field("name", &self.name)
            .field("address", &moruna_api::address_shown(&self.address))
            .field("sha256", &self.sha256)
            .finish()
    }
}

/// The functions a job uses as `api` registered them, `registered` (each its name, address and
/// SHA-256 in hex), against `names`, those the job's check answered (item 3.6): one for each, in
/// that order, each a registration's (64 lowercase hex digits); otherwise why not.
pub fn registrations(
    names: &[String],
    registered: &[(String, String, String)],
) -> Result<Vec<Registered>, String> {
    if names.len() != registered.len() {
        return Err(format!(
            "the job uses {} functions, and {} registrations came with it",
            names.len(),
            registered.len()
        ));
    }
    names
        .iter()
        .zip(registered)
        .map(|(name, (given, address, sha256))| {
            if name != given {
                return Err(format!(
                    "the job uses the function `{name}`, and the registration that came for it \
                     is `{given}`'s"
                ));
            }
            Registered::new(given, address, sha256).ok_or_else(|| {
                format!(
                    "the function `{given}`'s registration has no SHA-256 of 64 lowercase hex \
                     digits"
                )
            })
        })
        .collect()
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
    /// The image was verified, and could not be kept: no memory for it in the cache's account,
    /// which images a running job still holds keep charged after they are evicted.
    Unkept {
        /// The function's name.
        name: String,
        /// Why.
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
            Refused::Unkept { name, why } => {
                write!(f, "the function `{name}`'s image could not be kept: {why}")
            }
        }
    }
}

/// What the cache keeps of a verified image, and hands to each job that uses it: its bytes, or on
/// a node a memory object holding them ([`MemoryImage`]).
pub trait Kept: Clone {
    /// What an image of `len` bytes takes of the cache: its bytes, or the whole pages of the
    /// memory object it is kept in.
    fn charge(len: u64) -> u64;
}

impl Kept for Arc<[u8]> {
    fn charge(len: u64) -> u64 {
        len
    }
}

/// One kept image.
#[derive(Debug)]
struct Entry<K> {
    sha256: [u8; 32],
    kept: K,
    /// What it takes of the cache.
    charge: u64,
    /// When it was last handed to a job, on the cache's own count.
    used_at: u64,
}

/// The images kept between jobs: at most `capacity` bytes of them, the least recently used
/// evicted first.
#[derive(Debug)]
pub struct Cache<K = Arc<[u8]>> {
    capacity: u64,
    held: u64,
    entries: Vec<Entry<K>>,
    /// Counts each image handed to a job, to order them by use.
    clock: u64,
    fetches: u64,
    hits: u64,
}

impl Cache {
    /// An empty cache of `capacity` bytes that keeps each image's bytes.
    pub fn new(capacity: u64) -> Self {
        Self::keeping(capacity)
    }

    /// The verified image of `function`, its bytes: the kept one when the cache holds its
    /// SHA-256, otherwise fetched through `fetch`, checked, and kept.
    pub fn image(
        &mut self,
        function: &Registered,
        fetch: &mut dyn Fetch,
    ) -> Result<Arc<[u8]>, Refused> {
        self.image_kept(function, fetch, &mut |bytes| Ok(Arc::from(bytes)))
    }

    /// The verified images of every function a job uses, in the order it names them; the first
    /// that is refused refuses the job.
    pub fn images(
        &mut self,
        functions: &[Registered],
        fetch: &mut dyn Fetch,
    ) -> Result<Vec<Arc<[u8]>>, Refused> {
        self.images_kept(functions, fetch, &mut |bytes| Ok(Arc::from(bytes)))
    }
}

impl<K: Kept> Cache<K> {
    /// An empty cache of `capacity` bytes: node config's `functions.cache_bytes`.
    pub fn keeping(capacity: u64) -> Self {
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

    /// The bytes the images it keeps take of it.
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
    /// otherwise fetched through `fetch`, checked, and kept as `keep` makes it of the bytes,
    /// once the least recently used images have made room for it.
    pub fn image_kept(
        &mut self,
        function: &Registered,
        fetch: &mut dyn Fetch,
        keep: &mut dyn FnMut(Vec<u8>) -> Result<K, String>,
    ) -> Result<K, Refused> {
        self.clock += 1;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.sha256 == function.sha256)
        {
            entry.used_at = self.clock;
            self.hits += 1;
            return Ok(entry.kept.clone());
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
        let charge = K::charge(bytes.len() as u64);
        if charge > self.capacity {
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
        self.make_room(charge);
        let kept = keep(bytes).map_err(|why| Refused::Unkept {
            name: function.name.clone(),
            why,
        })?;
        self.held += charge;
        self.entries.push(Entry {
            sha256: function.sha256,
            kept: kept.clone(),
            charge,
            used_at: self.clock,
        });
        Ok(kept)
    }

    /// The verified images of every function a job uses, in the order it names them, each kept
    /// as `keep` makes it; the first that is refused refuses the job.
    pub fn images_kept(
        &mut self,
        functions: &[Registered],
        fetch: &mut dyn Fetch,
        keep: &mut dyn FnMut(Vec<u8>) -> Result<K, String>,
    ) -> Result<Vec<K>, Refused> {
        functions
            .iter()
            .map(|function| self.image_kept(function, fetch, keep))
            .collect()
    }

    /// Evicts the least recently used images until `charge` more bytes fit.
    fn make_room(&mut self, charge: u64) {
        while self.held + charge > self.capacity {
            let Some((oldest, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.used_at)
            else {
                break;
            };
            let evicted = self.entries.swap_remove(oldest);
            self.held -= evicted.charge;
        }
    }
}

mod handover;
pub use handover::{MemoryImage, handover, handover_of};

pub mod worker;

pub mod https;

#[cfg(test)]
mod tests;
