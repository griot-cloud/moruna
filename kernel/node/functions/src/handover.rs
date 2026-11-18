//! What a job's engine is given for the functions it uses (PM, item 3.6; the contract is
//! `node_protocol::functions`): each verified image, read only, under `function.N` in the order
//! the job names them, and the index of them under `functions`, one line `N NAME SHA256 SIZE`
//! each. The engine's supervisor hands both on as they are (`grant functions functions`).
//!
//! Each image is a memory object of its own ([`MemoryImage`]), written once when it was fetched
//! and charged to the cache's account; a job is given a handle to it that may read, map,
//! duplicate and move it, never write it, so nothing is copied per job.

use alloc::{format, rc::Rc, string::String, vec::Vec};

use moruna_svc::{Handle, Memory, Rights, map};
use node_protocol::functions::{self as contract, Entry};

use crate::{Kept, Registered};

/// The rights a job's handles carry: its engine reads and maps them, and its supervisor
/// duplicates them for each run and moves them on. Never `WRITE`.
pub const GRANTED: Rights = Rights::READ
    .union(Rights::MAP)
    .union(Rights::DUPLICATE)
    .union(Rights::TRANSFER);

const PAGE: u64 = 4096;

fn pages(len: u64) -> u64 {
    len.div_ceil(PAGE) * PAGE
}

/// A verified image in a memory object of its own, written once and never again.
#[derive(Debug)]
pub struct MemoryImage {
    memory: Memory,
    len: u64,
}

impl MemoryImage {
    /// `bytes` in a new memory object charged to `account`, the cache's: refused when it is
    /// empty or the account has no room for its pages.
    pub fn new(bytes: &[u8], account: &Handle) -> Result<MemoryImage, String> {
        if bytes.is_empty() {
            return Err(String::from("the image is empty"));
        }
        let len = bytes.len() as u64;
        let memory = Memory::create_in(pages(len), account).map_err(|e| {
            format!(
                "no memory object of {} bytes in the function cache's account: {}",
                pages(len),
                e.name()
            )
        })?;
        {
            let mapping = memory
                .map(0, pages(len), map::WRITE)
                .map_err(|e| format!("mapping the image's memory object: {}", e.name()))?;
            mapping
                .write(0, bytes)
                .map_err(|e| format!("writing the image's memory object: {}", e.name()))?;
        }
        Ok(MemoryImage { memory, len })
    }

    /// The image's length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether it is empty: never, since an empty image is refused.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A handle to it for a job: [`GRANTED`], never `WRITE`.
    pub fn grant(&self) -> Result<Handle, String> {
        self.memory
            .0
            .duplicate(GRANTED)
            .map_err(|e| format!("a handle to the image: {}", e.name()))
    }
}

impl Kept for Rc<MemoryImage> {
    fn charge(len: u64) -> u64 {
        pages(len)
    }
}

/// The handles a job's engine is given for `functions`, whose verified images are `images` in
/// the same order: `function.N` for each, and `functions`, the index, a memory object of its own;
/// nothing for a job that uses none. Refused when the two do not match one for one, or the index
/// is not one (more than `node_protocol::functions::MAX` functions, or a name that is not a
/// registered function's).
pub fn handover(
    functions: &[Registered],
    images: &[Rc<MemoryImage>],
) -> Result<Vec<(String, Handle)>, String> {
    if functions.is_empty() && images.is_empty() {
        return Ok(Vec::new());
    }
    if functions.len() != images.len() {
        return Err(format!(
            "{} functions and {} images",
            functions.len(),
            images.len()
        ));
    }
    let entries: Vec<Entry> = functions
        .iter()
        .zip(images)
        .map(|(function, image)| Entry {
            name: function.name.clone(),
            sha256: function.sha256,
            size: image.len(),
        })
        .collect();
    let text = contract::index(&entries).map_err(|e| format!("the functions' index: {e:?}"))?;
    // The text ends at the object's first zero byte, so the object is longer than it.
    let room = pages(text.len() as u64 + 1);
    let index = Memory::create(room)
        .map_err(|e| format!("the functions' index's memory object: {}", e.name()))?;
    {
        let mapping = index
            .map(0, room, map::WRITE)
            .map_err(|e| format!("mapping the functions' index: {}", e.name()))?;
        mapping
            .write(0, text.as_bytes())
            .map_err(|e| format!("writing the functions' index: {}", e.name()))?;
    }
    let mut out = Vec::with_capacity(images.len() + 1);
    for (n, image) in images.iter().enumerate() {
        out.push((contract::slot(n), image.grant()?));
    }
    let index = index
        .0
        .duplicate(GRANTED)
        .map_err(|e| format!("a handle to the functions' index: {}", e.name()))?;
    out.push((String::from(contract::INDEX_SLOT), index));
    Ok(out)
}
