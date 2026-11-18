//! A node's function images fetched on a thread of their own (PM, item 3.6), so that
//! `node-admission`'s dispatcher answers checks, `bound` and the rest while a fetch runs, and a
//! job's launch is answered when its functions are ready.
//!
//! The worker ([`serve`]) holds the cache and the cache's account, and fetches through what it
//! is given. It serves frames on a channel, one request at a time:
//!
//! - [`CACHE`]: the cache's size, and its account's handle: the cache made, once;
//! - [`IMAGES`]: a job's functions as registered: answered [`READY`] with each image's length
//!   and a read-only handle to it, in the same order, or [`REFUSED`] with the sentence that
//!   names the function.
//!
//! `node-admission` holds the other end ([`Fetcher`]) and waits on it with its dispatcher.

use alloc::{rc::Rc, string::String, vec::Vec};

use moruna_svc::{
    Account, Channel, Decoder, Encoder, Handle, Signals, Wire,
    frame::{self, Frame, Kind},
};

use crate::{Cache, Fetch, MemoryImage, Registered, hex, registrations};

/// The cache's size and its account: `(u64, Handle)`.
pub const CACHE: u16 = 1;
/// A job's functions as registered: `Vec<(name, address, sha256 hex)>`.
pub const IMAGES: u16 = 2;
/// The images ready: `Vec<(u64, Handle)>`, each one's length and a read-only handle.
pub const READY: u16 = 3;
/// A refusal: `String`, the sentence.
pub const REFUSED: u16 = 4;

fn send(channel: &Channel, ordinal: u16, body: impl Wire) -> Result<(), String> {
    let mut e = Encoder::new();
    body.encode(&mut e)
        .map_err(|e| alloc::format!("the fetch worker's message: {e:?}"))?;
    frame::send(channel, Kind::Event, ordinal, 0, e)
        .map_err(|e| alloc::format!("the fetch worker's channel: {e:?}"))
}

fn body<T: Wire>(frame: &mut Frame) -> Option<T> {
    let mut d: Decoder<'_> = frame.decoder();
    let value = T::decode(&mut d).ok()?;
    d.finish().ok()?;
    Some(value)
}

/// What the worker answers `IMAGES` with, ready to send.
enum Answer {
    Ready(Vec<(u64, Handle)>),
    Refused(String),
}

/// The worker's state: the cache, once `CACHE` has made it, with its account.
struct Worker {
    kept: Option<(Cache<Rc<MemoryImage>>, Account)>,
}

impl Worker {
    fn images(
        &mut self,
        functions: Vec<(String, String, String)>,
        fetch: &mut dyn Fetch,
        say: &mut dyn FnMut(&str),
    ) -> Answer {
        let Some((cache, account)) = self.kept.as_mut() else {
            return Answer::Refused(String::from(
                "the node's function cache is not set up: api has not sent its size",
            ));
        };
        let names: Vec<String> = functions.iter().map(|f| f.0.clone()).collect();
        let functions = match registrations(&names, &functions) {
            Ok(functions) => functions,
            Err(why) => return Answer::Refused(why),
        };
        let mut keep = |bytes: Vec<u8>| MemoryImage::new(&bytes, &account.0).map(Rc::new);
        let mut ready = Vec::with_capacity(functions.len());
        for function in &functions {
            let (fetches, _) = cache.counts();
            let image = match cache.image_kept(function, fetch, &mut keep) {
                Ok(image) => image,
                Err(refused) => return Answer::Refused(alloc::format!("{refused}")),
            };
            // What the console says of each: fetched, or kept from an earlier job.
            let how = if cache.counts().0 > fetches {
                "fetched"
            } else {
                "kept from an earlier job"
            };
            say(&alloc::format!(
                "node-admission: function {}: {} bytes, {}",
                function.name,
                image.len(),
                how
            ));
            match image.grant() {
                Ok(handle) => ready.push((image.len(), handle)),
                Err(why) => return Answer::Refused(why),
            }
        }
        Answer::Ready(ready)
    }
}

/// The worker: serves `channel` until its other end closes, fetching through `fetch`, and
/// saying each function's line (fetched, or kept from an earlier job) through `say`.
pub fn serve(channel: &Channel, fetch: &mut dyn Fetch, say: &mut dyn FnMut(&str)) {
    let mut worker = Worker { kept: None };
    loop {
        let frame = match frame::recv(channel) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                match moruna_svc::wait_one(
                    &channel.0,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    moruna_svc::DEADLINE_INFINITE,
                ) {
                    Ok(seen) if seen.contains(Signals::READABLE) => continue,
                    _ => return,
                }
            }
            Err(_) => return,
        };
        let mut frame = frame;
        match frame.ordinal {
            CACHE => {
                if let Some((capacity, account)) = body::<(u64, Handle)>(&mut frame)
                    && worker.kept.is_none()
                {
                    worker.kept = Some((Cache::keeping(capacity), Account(account)));
                }
            }
            IMAGES => {
                let answer = match body::<Vec<(String, String, String)>>(&mut frame) {
                    Some(functions) => worker.images(functions, fetch, say),
                    None => Answer::Refused(String::from(
                        "node-admission asked the fetch worker for something it cannot read",
                    )),
                };
                let sent = match answer {
                    Answer::Ready(ready) => send(channel, READY, ready),
                    Answer::Refused(why) => send(channel, REFUSED, why),
                };
                if sent.is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// `node-admission`'s end of the worker's channel.
pub struct Fetcher {
    channel: Channel,
}

impl Fetcher {
    /// Over `channel`, whose other end the worker serves.
    pub fn new(channel: Channel) -> Fetcher {
        Fetcher { channel }
    }

    /// The channel, for a handle to wait on.
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// The cache: `capacity` bytes, in `account`.
    pub fn cache(&self, capacity: u64, account: Handle) -> Result<(), String> {
        send(&self.channel, CACHE, (capacity, account))
    }

    /// Asks for the images of `functions`, which [`Fetcher::answer`] then takes.
    pub fn images(&self, functions: &[Registered]) -> Result<(), String> {
        let functions: Vec<(String, String, String)> = functions
            .iter()
            .map(|f| (f.name.clone(), f.address.clone(), hex(&f.sha256)))
            .collect();
        send(&self.channel, IMAGES, functions)
    }

    /// The worker's answer, if it has come: each image's length and handle, or the refusal's
    /// sentence; `None` while it is still to come. A worker that has gone is a refusal.
    pub fn answer(&self) -> Option<Result<Vec<(u64, Handle)>, String>> {
        let gone = || String::from("node-admission's fetch worker has ended");
        let mut frame = match frame::recv(&self.channel) {
            Ok(Some(frame)) => frame,
            Ok(None) => return None,
            Err(_) => return Some(Err(gone())),
        };
        Some(match frame.ordinal {
            READY => body::<Vec<(u64, Handle)>>(&mut frame).ok_or_else(gone),
            REFUSED => Err(body::<String>(&mut frame).unwrap_or_else(gone)),
            _ => Err(gone()),
        })
    }
}
