//! Actual boot-granted authority exchange over canonical auth bytes. There is no token
//! parser, cached allow decision, guessed workspace or successful default verifier.
use core::cell::{Cell, RefCell};
use moruna_storage_format::{
    auth::{Authorization, Verdict, Verifier},
    wire::{MAX_HEADER, Refusal},
};
use moruna_svc::{Channel, Handle, PrivateMemory, Signals, WaitItem, wait};

/// One explicitly granted live authority endpoint and requester-charged private controls.
pub struct Authority {
    channel: RefCell<Channel>,
    scratch: RefCell<PrivateMemory>,
    next: Cell<u64>,
    timeout_ns: u64,
    broken: Cell<bool>,
}
impl Authority {
    /// Construct only from a real boot/session authority grant and real KB5 account.
    pub fn new(channel: Channel, account: &Handle, timeout_ns: u64) -> Result<Self, Refusal> {
        if timeout_ns == 0 || timeout_ns == u64::MAX {
            return Err(Refusal::Invalid);
        }
        let scratch =
            PrivateMemory::create_in(MAX_HEADER as u64, account).map_err(|_| Refusal::Capacity)?;
        Ok(Self {
            channel: RefCell::new(channel),
            scratch: RefCell::new(scratch),
            next: Cell::new(1),
            timeout_ns,
            broken: Cell::new(false),
        })
    }
}
impl Verifier for Authority {
    fn verify(&self, authorization: &Authorization<'_>) -> Result<(), Refusal> {
        if self.broken.get() {
            return Err(Refusal::Unavailable);
        }
        let id = self.next.get();
        self.next
            .set(id.checked_add(1).ok_or(Refusal::Unavailable)?);
        let channel = self
            .channel
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        let mut scratch = self
            .scratch
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        let exchange = scratch
            .with_bytes(|bytes| {
                let result = (|| {
                    let length = authorization
                        .encode_into(id, bytes)
                        .map_err(|_| Refusal::Invalid)?;
                    channel
                        .write_with(&bytes[..length], &mut [])
                        .map_err(|_| Refusal::Unavailable)?;
                    bytes.fill(0);
                    let deadline = moruna_svc::now()
                        .checked_add(self.timeout_ns)
                        .ok_or(Refusal::Unavailable)?;
                    let mut handles = [];
                    loop {
                        match channel.read(bytes, &mut handles) {
                            Ok((length, handle_count)) => {
                                if handle_count != 0 {
                                    return Err(Refusal::Invalid);
                                }
                                let verdict = Verdict::decode(&bytes[..length])
                                    .map_err(|_| Refusal::Invalid)?;
                                if verdict.id != id {
                                    return Err(Refusal::Invalid);
                                }
                                bytes.fill(0);
                                return Ok(verdict.result);
                            }
                            Err(moruna_svc::Error::ShouldWait) => {
                                let mut item = [WaitItem::new(
                                    &channel.0,
                                    Signals::READABLE | Signals::PEER_CLOSED,
                                )];
                                wait(&mut item, deadline).map_err(|_| Refusal::Unavailable)?;
                            }
                            Err(_) => return Err(Refusal::Unavailable),
                        }
                    }
                })();
                bytes.fill(0);
                result
            })
            .map_err(|_| Refusal::Unavailable)
            .and_then(|result| result);
        match exchange {
            Ok(verdict) => verdict,
            Err(cause) => {
                // An uncertain/corrupt reply must never be consumed as the next action's
                // verdict. A valid live denial above does not poison the connection.
                self.broken.set(true);
                Err(cause)
            }
        }
    }
}
