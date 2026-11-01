//! Consumer of an actual boot/application authority capability. This implements no
//! issuer, token syntax, owner-equality rule or default authorization policy.
use crate::{authority::Authority, protocol::delegate};
use moruna_storage_format::{
    auth::Admission,
    wire::{MAX_HEADER, Refusal},
};
use moruna_svc::{Channel, Handle, Memory, ObjectType, PrivateMemory, Rights};

/// Real designated authority connection. Only that authority may decide whether the
/// supplied real child account and scope are admitted and mint its bound verifier end.
pub struct Delegator {
    client: Option<delegate::Client>,
    timeout_ns: u64,
}
impl Delegator {
    /// Hello on an explicitly granted CONNECTED authority endpoint; never a listener,
    /// environment credential, newly invented local issuer, or permissive fallback.
    pub fn new(channel: Channel, timeout_ns: u64) -> Result<Self, Refusal> {
        if timeout_ns == 0 || timeout_ns == u64::MAX {
            return Err(Refusal::Invalid);
        }
        let mut client = delegate::Client::connect(channel, moruna_svc::deadline_in(timeout_ns))
            .map_err(|_| Refusal::Unavailable)?;
        client.core().set_timeout(timeout_ns);
        Ok(Self {
            client: Some(client),
            timeout_ns,
        })
    }
    /// Ask the actual designated authority to bind this real account and request identity.
    /// An uncertain RPC permanently closes this consumer rather than replaying admission.
    pub fn bind(&mut self, admission: &Admission, account: &Handle) -> Result<Authority, Refusal> {
        let (kind, rights) = account.info().map_err(|_| Refusal::Unavailable)?;
        let needed = Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER;
        if kind != ObjectType::Account || !rights.contains(needed) {
            return Err(Refusal::Denied);
        }
        let mut control =
            PrivateMemory::create_in(MAX_HEADER as u64, account).map_err(|_| Refusal::Capacity)?;
        let length = control
            .with_bytes(|bytes| admission.encode_into(bytes))
            .map_err(|_| Refusal::Unavailable)?
            .map_err(|_| Refusal::Invalid)?;
        // Admission contains only bounded nonsecret scope/role. Export consumes the
        // exclusive mapping before any peer can map the immutable control loan.
        let memory = control.into_memory();
        let control = Memory(
            memory
                .0
                .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                .map_err(|_| Refusal::Unavailable)?,
        );
        let account_loan = account
            .duplicate(needed)
            .map_err(|_| Refusal::Unavailable)?;
        let result = self.client.as_mut().ok_or(Refusal::Unavailable)?.bind(
            control,
            length as u32,
            account_loan,
        );
        match result {
            Ok(channel) => Authority::new(channel, account, self.timeout_ns),
            Err(moruna_svc::CallError::Remote(delegate::Error::Denied)) => Err(Refusal::Denied),
            Err(moruna_svc::CallError::Remote(delegate::Error::Invalid)) => Err(Refusal::Invalid),
            Err(_) => {
                self.client = None;
                Err(Refusal::Unavailable)
            }
        }
    }
}
