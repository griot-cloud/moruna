//! Attenuates only explicit trusted boot grants; public request identities mint no rights.
use super::{RUNNING, Shared, args};
use alloc::{boxed::Box, collections::BTreeMap, rc::Rc, vec, vec::Vec};
use core::cell::{Cell, RefCell};
use moruna_storage_format::{
    auth::{Admission, DecodedAuthorization, Verdict},
    grants::{GrantLease, GrantSet},
    wire::{self, Refusal},
};
use moruna_storage_service::protocol::delegate;
use moruna_svc::{
    Channel, Completions, Context, Decoder, Handle, Handler, Memory, ObjectType, Outcome,
    ProtocolId, Rights, Signals, Status, Watch, map,
};
use moruna_sys::Account;

const MAX_SESSIONS: usize = 32;
pub struct Authority {
    policy: RefCell<GrantSet>,
    clock: RefCell<moruna_node::BootClock>,
    bindings: RefCell<BTreeMap<&'static str, Admission>>,
    shared: Rc<RefCell<Shared>>,
    owner: Cell<Option<u64>>,
    sessions: Rc<Cell<usize>>,
}
impl Authority {
    pub fn load(shared: Rc<RefCell<Shared>>, handle: Handle) -> Result<Rc<Self>, ()> {
        let (kind, rights) = handle.info().map_err(|_| ())?;
        if kind != ObjectType::Memory
            || !rights.contains(Rights::READ | Rights::MAP)
            || rights.contains(Rights::WRITE)
        {
            return Err(());
        }
        let memory = Memory(handle);
        let first = memory.map(0, 4096, map::READ).map_err(|_| ())?;
        let mut prefix = [0; wire::PREFIX_BYTES];
        first.read(0, &mut prefix).map_err(|_| ())?;
        drop(first);
        let length = wire::header_bytes(&prefix).map_err(|_| ())?;
        let capacity = (length as u64).div_ceil(4096) * 4096;
        let mapping = memory.map(0, capacity, map::READ).map_err(|_| ())?;
        let mut bytes = vec![0; capacity as usize];
        mapping.read(0, &mut bytes).map_err(|_| ())?;
        let policy = GrantSet::decode_page(&bytes).map_err(|_| ())?;
        Ok(Rc::new(Self {
            policy: RefCell::new(policy),
            clock: RefCell::new(moruna_node::BootClock::new(moruna_svc::now())),
            bindings: RefCell::new(BTreeMap::new()),
            shared,
            owner: Cell::new(None),
            sessions: Rc::new(Cell::new(0)),
        }))
    }
    pub fn revoke(&self, id: u64) -> Result<(), ()> {
        self.policy
            .try_borrow_mut()
            .map_err(|_| ())?
            .revoke(id)
            .map_err(|_| ())
    }
    pub fn owner(&self, id: u64) {
        self.owner.set(Some(id));
    }
    fn live(&self) -> bool {
        let Ok(shared) = self.shared.try_borrow() else {
            return false;
        };
        if shared.closing || shared.fault {
            return false;
        }
        let Some(job) = self.owner.get().and_then(|id| shared.jobs.get(&id)) else {
            return false;
        };
        job.state == RUNNING
            && matches!(
                moruna_svc::wait_one(&job.process.0, Signals::TERMINATED, moruna_svc::now()),
                Err(moruna_svc::Error::TimedOut)
            )
    }
    fn elapsed(&self) -> Result<u64, Refusal> {
        self.clock
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?
            .elapsed(moruna_svc::now())
            .map_err(|_| Refusal::Denied)
    }
    fn lease_live(&self, lease: &GrantLease) -> bool {
        self.live()
            && self
                .elapsed()
                .ok()
                .and_then(|now| {
                    self.policy
                        .try_borrow()
                        .ok()?
                        .admit(lease.admission(), now)
                        .ok()
                })
                .is_some_and(|current| current.id() == lease.id())
    }
    pub fn register(&self, slot: &str, handle: &Handle) -> Result<(), ()> {
        let kind = match slot {
            "storage.binding" => "home",
            "checkpoint.binding" => "checkpoint",
            "cache.binding" => "cache",
            _ => return Ok(()),
        };
        let (object, rights) = handle.info().map_err(|_| ())?;
        if object != ObjectType::Memory
            || !rights.contains(Rights::READ | Rights::MAP)
            || rights.contains(Rights::WRITE)
        {
            return Err(());
        }
        let memory = Memory(
            handle
                .duplicate(Rights::READ | Rights::MAP)
                .map_err(|_| ())?,
        );
        let first = memory.map(0, 4096, map::READ).map_err(|_| ())?;
        let mut prefix = [0; wire::PREFIX_BYTES];
        first.read(0, &mut prefix).map_err(|_| ())?;
        drop(first);
        let length = wire::header_bytes(&prefix).map_err(|_| ())?;
        let capacity = (length as u64).div_ceil(4096) * 4096;
        let mapping = memory.map(0, capacity, map::READ).map_err(|_| ())?;
        let mut bytes = vec![0; capacity as usize];
        mapping.read(0, &mut bytes).map_err(|_| ())?;
        if bytes[length..].iter().any(|b| *b != 0) {
            return Err(());
        }
        let admission = Admission::decode(&bytes[..length]).map_err(|_| ())?;
        use moruna_storage_format::auth::SessionRole;
        let actual = match admission.role {
            SessionRole::BucketHome | SessionRole::SingleNodeHome => "home",
            SessionRole::CheckpointMetadata => "checkpoint",
            SessionRole::CacheScratch => "cache",
        };
        if kind != actual || self.bindings.borrow().contains_key(kind) {
            return Err(());
        }
        self.bindings.borrow_mut().insert(kind, admission);
        Ok(())
    }
    pub fn handler(
        self: &Rc<Self>,
        channel: &Channel,
        kind: &'static str,
    ) -> Result<Box<dyn Handler>, ()> {
        Ok(Box::new(Delegate(
            self.clone(),
            Rc::new(Master {
                kind,
                channel: channel.0.duplicate(Rights::WAIT).map_err(|_| ())?,
                alive: Cell::new(true),
            }),
        )))
    }
}
struct Master {
    kind: &'static str,
    channel: Handle,
    alive: Cell<bool>,
}
impl Master {
    fn live(&self) -> bool {
        self.alive.get()
            && matches!(
                moruna_svc::wait_one(&self.channel, Signals::PEER_CLOSED, moruna_svc::now()),
                Err(moruna_svc::Error::TimedOut)
            )
    }
}
struct Delegate(Rc<Authority>, Rc<Master>);
impl Handler for Delegate {
    fn protocol(&self) -> ProtocolId {
        delegate::ID
    }
    fn closed(&mut self) {
        self.1.alive.set(false);
    }
    fn handle(&mut self, _: &mut Context<'_>, ordinal: u16, body: &mut Decoder<'_>) -> Outcome {
        if ordinal != 1 {
            return Outcome::Status(Status::UnknownMethod);
        }
        let Some((control, length, account)) = args::<(Memory, u32, Handle)>(body) else {
            return Outcome::Status(Status::BadMessage);
        };
        match self.bind(control, length, account) {
            Ok(channel) => Outcome::reply(channel),
            Err(error) => Outcome::Failure(error),
        }
    }
}
impl Delegate {
    fn bind(&mut self, control: Memory, length: u32, handle: Handle) -> Result<Channel, u32> {
        if !self.0.live() || !self.1.live() {
            return Err(2);
        }
        if self.0.sessions.get() >= MAX_SESSIONS
            || !(wire::PREFIX_BYTES..=wire::MAX_HEADER).contains(&(length as usize))
        {
            return Err(3);
        }
        let (kind, rights) = handle.info().map_err(|_| 1u32)?;
        if kind != ObjectType::Account || !rights.contains(Rights::READ | Rights::TRANSFER) {
            return Err(1);
        }
        let account = Account(handle);
        if !account.matches(&control.0).map_err(|_| 3u32)? {
            return Err(2);
        }
        let (kind, rights) = control.0.info().map_err(|_| 1u32)?;
        if kind != ObjectType::Memory
            || !rights.contains(Rights::READ | Rights::MAP)
            || rights.contains(Rights::WRITE)
        {
            return Err(1);
        }
        let pages = u64::from(length).div_ceil(4096) * 4096;
        let mapping = control.map(0, pages, map::READ).map_err(|_| 1u32)?;
        let mut bytes = vec![0; length as usize];
        mapping.read(0, &mut bytes).map_err(|_| 1u32)?;
        let admission = Admission::decode(&bytes).map_err(|_| 1u32)?;
        let kind = match admission.role {
            moruna_storage_format::auth::SessionRole::BucketHome
            | moruna_storage_format::auth::SessionRole::SingleNodeHome => "home",
            moruna_storage_format::auth::SessionRole::CheckpointMetadata => "checkpoint",
            moruna_storage_format::auth::SessionRole::CacheScratch => "cache",
        };
        let permitted = match self.1.kind {
            "home" => kind == "home",
            "local" => kind == "checkpoint" || kind == "cache",
            "local-home" => admission.role != moruna_storage_format::auth::SessionRole::BucketHome,
            _ => false,
        };
        if !permitted
            || self.0.bindings.try_borrow().map_err(|_| 3u32)?.get(kind) != Some(&admission)
        {
            return Err(2);
        }
        let lease = self
            .0
            .policy
            .try_borrow()
            .map_err(|_| 3u32)?
            .admit(&admission, self.0.elapsed().map_err(|_| 2u32)?)
            .map_err(|_| 2u32)?;
        self.0
            .clock
            .try_borrow()
            .map_err(|_| 3u32)?
            .expires_at(lease.expires_ns())
            .map_err(|_| 2u32)?;
        let permit = self
            .0
            .shared
            .try_borrow()
            .map_err(|_| 3u32)?
            .watch_capacity
            .as_ref()
            .ok_or(3u32)?
            .reserve()
            .map_err(|_| 3u32)?;
        let (server, client) = Channel::create().map_err(|_| 3u32)?;
        self.0.sessions.set(self.0.sessions.get() + 1);
        self.0.shared.borrow_mut().watches.push((
            Box::new(Verifier {
                authority: self.0.clone(),
                master: self.1.clone(),
                channel: server,
                _account: account,
                lease,
                buffer: vec![0; wire::MAX_HEADER],
                pending: None,
            }),
            permit,
        ));
        Ok(client)
    }
}
struct Verifier {
    authority: Rc<Authority>,
    master: Rc<Master>,
    channel: Channel,
    _account: Account,
    lease: GrantLease,
    buffer: Vec<u8>,
    pending: Option<usize>,
}
impl Drop for Verifier {
    fn drop(&mut self) {
        self.buffer.fill(0);
        self.authority
            .sessions
            .set(self.authority.sessions.get() - 1);
    }
}
impl Watch for Verifier {
    fn handle(&self) -> &Handle {
        &self.channel.0
    }
    fn signals(&self) -> Signals {
        if self.pending.is_some() {
            Signals::WRITABLE | Signals::PEER_CLOSED
        } else {
            Signals::READABLE | Signals::PEER_CLOSED
        }
    }
    fn signaled(&mut self, seen: Signals, _: &mut Completions) -> bool {
        if seen.intersects(Signals::PEER_CLOSED) {
            return false;
        }
        if self.pending.is_none() {
            let mut handles: [Option<Handle>; 0] = [];
            let Ok((length, count)) = self.channel.read(&mut self.buffer, &mut handles) else {
                return false;
            };
            if count != 0 {
                return false;
            }
            let Ok(request) = DecodedAuthorization::decode(&self.buffer[..length]) else {
                return false;
            };
            let id = request.id();
            let result = if self.authority.live() && self.master.live() {
                self.authority
                    .policy
                    .try_borrow()
                    .map_err(|_| Refusal::Unavailable)
                    .and_then(|policy| {
                        self.authority.elapsed().and_then(|now| {
                            policy.verify(&self.lease, &request.authorization(), now)
                        })
                    })
            } else {
                Err(Refusal::Denied)
            };
            self.buffer.fill(0);
            let Ok(length) = (Verdict { id, result }).encode_into(&mut self.buffer) else {
                return false;
            };
            self.pending = Some(length);
        }
        let mut length = self.pending.unwrap_or(0);
        // An earlier allow waiting for queue space must not outlive revocation,
        // expiry, actual owner termination or master peer closure.
        let Ok(verdict) = Verdict::decode(&self.buffer[..length]) else {
            return false;
        };
        if verdict.result.is_ok()
            && (!self.master.live() || !self.authority.lease_live(&self.lease))
        {
            self.buffer.fill(0);
            let Ok(denied) = (Verdict {
                id: verdict.id,
                result: Err(Refusal::Denied),
            })
            .encode_into(&mut self.buffer) else {
                return false;
            };
            length = denied;
            self.pending = Some(length);
        }
        match self.channel.write_with(&self.buffer[..length], &mut []) {
            Ok(()) => {
                self.pending = None;
                self.buffer.fill(0);
                true
            }
            Err(moruna_svc::Error::ShouldWait) => true,
            Err(_) => false,
        }
    }
}
