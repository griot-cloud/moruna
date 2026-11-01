//! Platform-independent exact-action verifier contract. No token syntax, implicit sharing,
//! owner-equality rule or permissive authority implementation is supplied here.
use crate::{
    Object, Scope, Version,
    wire::{MAX_CAPABILITY, Refusal},
};
/// One exact governed action. Listing never grants later per-object reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Read bytes or exact metadata, including encrypted cache/scratch hits.
    Read,
    /// Create one immutable whole object or ephemeral scratch.
    Write,
    /// Enumerate an exact prefix.
    List,
    /// Atomically replace a pointer/publication under its actual condition.
    Publish,
}
/// Requested proof mode, NEVER an authorization grant by itself. A native marker is
/// accepted only by the live verifier's actual SDK session/context binding.
pub enum Proof<'a> {
    /// Actual application-issued opaque credential, not interpreted by transport.
    Application(&'a [u8]),
    /// Request actual native session authority; no fabricated credential bytes exist.
    NativeSession,
}
impl<'a> Proof<'a> {
    /// Application bytes only. Native capabilities are never serialized as opaque identity.
    pub fn application(&self) -> Option<&'a [u8]> {
        match self {
            Self::Application(bytes) => Some(bytes),
            Self::NativeSession => None,
        }
    }
    /// Whether the designated verifier must authenticate actual native SDK context.
    pub fn native_session(&self) -> bool {
        matches!(self, Self::NativeSession)
    }
    pub(crate) fn borrowed(&self) -> Self {
        match self {
            Self::Application(bytes) => Self::Application(bytes),
            Self::NativeSession => Self::NativeSession,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), Refusal> {
        match self {
            Self::Application(bytes) if bytes.is_empty() || bytes.len() > MAX_CAPABILITY => {
                Err(Refusal::Denied)
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn encode(&self, out: &mut crate::wire::Out<'_>) -> crate::Result<()> {
        match self {
            Self::Application(bytes) => {
                out.u8(0)?;
                out.blob(bytes)
            }
            Self::NativeSession => out.u8(1),
        }
    }
    pub(crate) fn decode(input: &mut crate::wire::Input<'a>) -> crate::Result<Self> {
        let proof = match input.u8()? {
            0 => Self::Application(input.blob()?),
            1 => Self::NativeSession,
            _ => return Err(crate::Error::Invalid),
        };
        proof.validate().map_err(|_| crate::Error::Invalid)?;
        Ok(proof)
    }
}
/// Validated input to an injected authority. No secret Debug or serialization.
pub struct Authorization<'a> {
    scope: &'a Scope,
    object: &'a Object,
    action: Action,
    proof: Proof<'a>,
    version: Option<&'a Version>,
}
impl<'a> Authorization<'a> {
    /// Validate bounded identities/capability before dispatching to the live authority.
    pub fn new(
        scope: &'a Scope,
        object: &'a Object,
        action: Action,
        capability: &'a [u8],
    ) -> Result<Self, Refusal> {
        scope.validate().map_err(|_| Refusal::Invalid)?;
        object.validate().map_err(|_| Refusal::Invalid)?;
        let proof = Proof::Application(capability);
        proof.validate()?;
        Ok(Self {
            scope,
            object,
            action,
            proof,
            version: None,
        })
    }
    /// Requester's ledger/key scope, never silently substituted by resource owner.
    pub fn scope(&self) -> &Scope {
        self.scope
    }
    /// Exact owned resource/prefix.
    pub fn object(&self) -> &Object {
        self.object
    }
    /// Bind an actual pinned read/head or prior conditional publication version.
    pub fn with_version(mut self, version: &'a Version) -> Result<Self, Refusal> {
        version.validate().map_err(|_| Refusal::Invalid)?;
        self.version = Some(version);
        Ok(self)
    }
    /// Actual version context; absence never satisfies a version-specific grant.
    pub fn version(&self) -> Option<&Version> {
        self.version
    }
    /// Requested operation.
    pub fn action(&self) -> Action {
        self.action
    }
    /// Requested proof; the actual verifier supplies session context out of band.
    pub fn proof(&self) -> &Proof<'a> {
        &self.proof
    }
    /// Native request marker only. The live designated verifier must bind its actual
    /// SDK connection to Scope+Account; this constructor never grants permission.
    pub fn native(scope: &'a Scope, object: &'a Object, action: Action) -> Result<Self, Refusal> {
        scope.validate().map_err(|_| Refusal::Invalid)?;
        object.validate().map_err(|_| Refusal::Invalid)?;
        Ok(Self {
            scope,
            object,
            action,
            proof: Proof::NativeSession,
            version: None,
        })
    }
}
/// Injected actual authority, checked for every operation including cache hits. Native
/// implementations may call their designated authority service; absent service refuses.
pub trait Verifier {
    /// Verify this exact request against current authority, with no cached prior grant.
    fn verify(&self, authorization: &Authorization<'_>) -> Result<(), Refusal>;
}

impl Authorization<'_> {
    /// Encode the actual exact-action request to an explicitly granted authority service.
    /// No fake storage operation/version is synthesized. Caller wipes its secret header.
    pub fn encode_into(&self, id: u64, destination: &mut [u8]) -> crate::Result<usize> {
        let mut out = crate::wire::Out::new(destination)?;
        out.u8(3)?;
        out.u64(id)?;
        out.text(&self.scope.workspace)?;
        out.text(&self.scope.run)?;
        out.object(self.object)?;
        out.u8(match self.action {
            Action::Read => 1,
            Action::Write => 2,
            Action::List => 3,
            Action::Publish => 4,
        })?;
        self.proof.encode(&mut out)?;
        match self.version {
            Some(version) => {
                out.u8(1)?;
                out.version(version)?;
            }
            None => out.u8(0)?,
        }
        out.finish()
    }
}
/// Decoded authority message owns bounded identities but borrows secret capability bytes.
/// There is intentionally no Debug, Clone or credential serialization trait.
pub struct DecodedAuthorization<'a> {
    id: u64,
    scope: Scope,
    object: Object,
    action: Action,
    proof: Proof<'a>,
    version: Option<Version>,
}
impl<'a> DecodedAuthorization<'a> {
    /// Decode only the sole bounded exact-action authority envelope.
    pub fn decode(source: &'a [u8]) -> crate::Result<Self> {
        let mut input = crate::wire::Input::new(source)?;
        if input.u8()? != 3 {
            return Err(crate::Error::Invalid);
        }
        let id = input.u64()?;
        let scope = Scope {
            workspace: input.text()?,
            run: input.text()?,
        };
        let object = input.object()?;
        let action = match input.u8()? {
            1 => Action::Read,
            2 => Action::Write,
            3 => Action::List,
            4 => Action::Publish,
            _ => return Err(crate::Error::Invalid),
        };
        let proof = Proof::decode(&mut input)?;
        let version = match input.u8()? {
            0 => None,
            1 => Some(input.version()?),
            _ => return Err(crate::Error::Invalid),
        };
        input.finish()?;
        scope.validate()?;
        object.validate()?;
        proof.validate().map_err(|_| crate::Error::Invalid)?;
        Ok(Self {
            id,
            scope,
            object,
            action,
            proof,
            version,
        })
    }
    /// Exact correlation identity, checked by the service client against its pending call.
    pub fn id(&self) -> u64 {
        self.id
    }
    /// Borrow the validated exact input for the one actual verifier.
    pub fn authorization(&self) -> Authorization<'_> {
        Authorization {
            scope: &self.scope,
            object: &self.object,
            action: self.action,
            proof: self.proof.borrowed(),
            version: self.version.as_ref(),
        }
    }
}
/// Explicit native backend role requested through a private admission capability.
/// A role request is not authority and never chooses a fallback backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionRole {
    /// Actual configured portable bucket home; never a local disk alias.
    BucketHome,
    /// Explicit configured single-node raw home, not portable bucket authority.
    SingleNodeHome,
    /// Separate actual durable checkpoint metadata service.
    CheckpointMetadata,
    /// Encrypted local cache/scratch service.
    CacheScratch,
}
impl SessionRole {
    pub(crate) fn write(self, out: &mut crate::wire::Out<'_>) -> crate::Result<()> {
        out.u8(match self {
            Self::BucketHome => 1,
            Self::CheckpointMetadata => 2,
            Self::CacheScratch => 3,
            Self::SingleNodeHome => 4,
        })
    }
    pub(crate) fn read(input: &mut crate::wire::Input<'_>) -> crate::Result<Self> {
        match input.u8()? {
            1 => Ok(Self::BucketHome),
            2 => Ok(Self::CheckpointMetadata),
            3 => Ok(Self::CacheScratch),
            4 => Ok(Self::SingleNodeHome),
            _ => Err(crate::Error::Invalid),
        }
    }
}
/// Sole native admission control encoding. The real child Account travels as an SDK
/// capability alongside these bytes; no numeric account identity is trusted from wire.
/// Only a boot-granted designated admission endpoint may accept this request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Admission {
    /// Exact requester scope to bind to the actual supplied child account.
    pub scope: Scope,
    /// Explicit configured backend role, checked against admission authority.
    pub role: SessionRole,
}
impl Admission {
    /// Encode bounded validated identity; this does not grant or attenuate authority.
    pub fn encode_into(&self, destination: &mut [u8]) -> crate::Result<usize> {
        self.encode_kind(5, destination)
    }
    fn encode_kind(&self, kind: u8, destination: &mut [u8]) -> crate::Result<usize> {
        let mut out = crate::wire::Out::new(destination)?;
        out.u8(kind)?;
        self.write(&mut out)?;
        out.finish()
    }
    pub(crate) fn write(&self, out: &mut crate::wire::Out<'_>) -> crate::Result<()> {
        self.scope.validate()?;
        out.text(&self.scope.workspace)?;
        out.text(&self.scope.run)?;
        self.role.write(out)
    }
    pub(crate) fn read(input: &mut crate::wire::Input<'_>) -> crate::Result<Self> {
        let scope = Scope {
            workspace: input.text()?,
            run: input.text()?,
        };
        scope.validate()?;
        let role = SessionRole::read(input)?;
        Ok(Self { scope, role })
    }
    /// Decode exact control before admission. Scope is an asserted identity only; the
    /// designated authority must verify the actual account/caller capability context.
    pub fn decode(source: &[u8]) -> crate::Result<Self> {
        Self::decode_kind(5, source)
    }
    fn decode_kind(kind: u8, source: &[u8]) -> crate::Result<Self> {
        let mut input = crate::wire::Input::new(source)?;
        if input.u8()? != kind {
            return Err(crate::Error::Invalid);
        }
        let admission = Self::read(&mut input)?;
        input.finish()?;
        Ok(admission)
    }
}
/// Actual bound session facts returned only over the connected native service. These
/// bytes are not independently a grant: the consumed startup channel/SDK reply is the
/// authority source. Client assertions or object request Scope cannot set these facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    /// Actual configured and admitted scope/backend role for this exact connection.
    pub admission: Admission,
}
impl Session {
    /// Encode actual server-owned facts into the caller's admitted reply memory.
    pub fn encode_into(&self, destination: &mut [u8]) -> crate::Result<usize> {
        self.admission.encode_kind(6, destination)
    }
    /// Decode bound session facts, rejecting admission requests and other control kinds.
    pub fn decode(source: &[u8]) -> crate::Result<Self> {
        Ok(Self {
            admission: Admission::decode_kind(6, source)?,
        })
    }
}
/// Correlated typed authority verdict. The protocol supplies no default successful verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verdict {
    /// Original authorization request identity.
    pub id: u64,
    /// Actual live verifier result, no arbitrary diagnostic/secret payload.
    pub result: Result<(), Refusal>,
}
impl Verdict {
    /// Encode the actual authority decision into admitted caller space.
    pub fn encode_into(&self, destination: &mut [u8]) -> crate::Result<usize> {
        let mut out = crate::wire::Out::new(destination)?;
        out.u8(4)?;
        out.u64(self.id)?;
        match self.result {
            Ok(()) => out.u8(0)?,
            Err(code) => out.u8(crate::wire::refusal_code(code))?,
        }
        out.finish()
    }
    /// Decode an exact correlated verdict; unknown codes and protocol majors refuse.
    pub fn decode(source: &[u8]) -> crate::Result<Self> {
        let mut input = crate::wire::Input::new(source)?;
        if input.u8()? != 4 {
            return Err(crate::Error::Invalid);
        }
        let id = input.u64()?;
        let code = input.u8()?;
        let result = if code == 0 {
            Ok(())
        } else {
            Err(crate::wire::refusal_from_code(code)?)
        };
        input.finish()?;
        Ok(Self { id, result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec};
    #[test]
    fn native_proof_and_admission_are_distinct_from_application_credentials() {
        let scope = Scope {
            workspace: "requester".into(),
            run: "attempt".into(),
        };
        let object = Object {
            workspace: "foreign-owner".into(),
            bucket: "home".into(),
            key: "rows".into(),
        };
        let native = Authorization::native(&scope, &object, Action::Read).unwrap();
        let mut bytes = [0; 1024];
        let length = native.encode_into(9, &mut bytes).unwrap();
        let decoded = DecodedAuthorization::decode(&bytes[..length]).unwrap();
        assert!(decoded.authorization().proof().native_session());
        assert!(decoded.authorization().proof().application().is_none());
        assert!(Admission::decode(&bytes[..length]).is_err());
        for role in [
            SessionRole::BucketHome,
            SessionRole::CheckpointMetadata,
            SessionRole::CacheScratch,
            SessionRole::SingleNodeHome,
        ] {
            let admission = Admission {
                scope: scope.clone(),
                role,
            };
            let length = admission.encode_into(&mut bytes).unwrap();
            assert_eq!(Admission::decode(&bytes[..length]).unwrap(), admission);
            assert!(Session::decode(&bytes[..length]).is_err());
            let bound = Session {
                admission: admission.clone(),
            };
            let session_length = bound.encode_into(&mut bytes).unwrap();
            assert_eq!(Session::decode(&bytes[..session_length]).unwrap(), bound);
            assert!(Admission::decode(&bytes[..session_length]).is_err());
            let length = admission.encode_into(&mut bytes).unwrap();
            assert!(DecodedAuthorization::decode(&bytes[..length]).is_err());
            assert!(crate::wire::Request::decode(&bytes[..length]).is_err());
            for cut in 0..length {
                assert!(Admission::decode(&bytes[..cut]).is_err());
            }
            bytes[length - 1] = 255;
            assert!(Admission::decode(&bytes[..length]).is_err());
        }
    }
    #[test]
    fn authority_codec_carries_exact_action_and_borrows_capability_without_fake_operation() {
        let scope = Scope {
            workspace: "requester".to_string(),
            run: "run".to_string(),
        };
        let object = Object {
            workspace: "owner".to_string(),
            bucket: "home".to_string(),
            key: "key".to_string(),
        };
        for action in [Action::Read, Action::Write, Action::List, Action::Publish] {
            let auth = Authorization::new(&scope, &object, action, b"TEST-ONLY-OPAQUE").unwrap();
            let mut bytes = vec![0; crate::wire::MAX_HEADER];
            let length = auth.encode_into(19, &mut bytes).unwrap();
            let decoded = DecodedAuthorization::decode(&bytes[..length]).unwrap();
            assert_eq!(decoded.id(), 19);
            let actual = decoded.authorization();
            assert_eq!(actual.scope(), &scope);
            assert_eq!(actual.object(), &object);
            assert_eq!(actual.action(), action);
            assert_eq!(
                actual.proof().application().unwrap(),
                auth.proof().application().unwrap()
            );
            let start = bytes.as_ptr() as usize;
            let ptr = actual.proof().application().unwrap().as_ptr() as usize;
            assert!(
                ptr >= start && ptr + actual.proof().application().unwrap().len() <= start + length
            );
            for cut in 0..length {
                assert!(DecodedAuthorization::decode(&bytes[..cut]).is_err());
            }
            assert!(crate::wire::Request::decode(&bytes[..length]).is_err()); // no fake object RPC
        }
        for result in [Ok(()), Err(Refusal::Denied), Err(Refusal::Unavailable)] {
            let verdict = Verdict { id: 19, result };
            let mut bytes = [0; 64];
            let n = verdict.encode_into(&mut bytes).unwrap();
            assert_eq!(Verdict::decode(&bytes[..n]).unwrap(), verdict);
            bytes[n - 1] = 255;
            assert!(Verdict::decode(&bytes[..n]).is_err());
        }
        assert!(Authorization::new(&scope, &object, Action::Read, b"").is_err());
    }
}
