//! Explicit trusted boot/application capability attenuation. This codec creates NO
//! rights from public claims, opaque application bytes, resource ownership or names.
use crate::{
    Error, Object, Result, Version,
    auth::{Action, Admission, Authorization},
    wire::{self, Refusal},
};
use alloc::{collections::BTreeSet, vec::Vec};
/// Maximum explicit grant records in one bounded trusted control.
pub const MAX_GRANTS: usize = 64;
/// Maximum explicit rules in one grant; wire control remains limited to 64 KiB total.
pub const MAX_RULES: usize = 256;
/// Explicit action bits, never inferred from another action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Actions(u8);
impl Actions {
    /// Read/head authority.
    pub const READ: u8 = 1;
    /// Immutable create authority.
    pub const WRITE: u8 = 2;
    /// Prefix enumeration authority, not later read permission.
    pub const LIST: u8 = 4;
    /// Conditional pointer/publication authority.
    pub const PUBLISH: u8 = 8;
    /// Refuse empty/unknown bits instead of widening them.
    pub fn new(bits: u8) -> Result<Self> {
        if bits == 0 || bits & !15 != 0 {
            Err(Error::Invalid)
        } else {
            Ok(Self(bits))
        }
    }
    /// Exact admitted bits.
    pub fn bits(self) -> u8 {
        self.0
    }
    /// Whether this exact action was explicitly granted.
    pub fn allows(self, action: Action) -> bool {
        self.0
            & match action {
                Action::Read => Self::READ,
                Action::Write => Self::WRITE,
                Action::List => Self::LIST,
                Action::Publish => Self::PUBLISH,
            }
            != 0
    }
}
/// Exact resource or literal bounded namespace. No wildcard, regex or owner inference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resource {
    /// One exact object, optionally restricted to an actual immutable/prior version.
    Exact {
        /// Exact owner/bucket/key.
        object: Object,
        /// None means explicitly any version of this exact object.
        version: Option<Version>,
    },
    /// Descendants of an explicit NONEMPTY directory prefix ending `/`, in the exact
    /// configured owner/bucket. It does not match adjacent `prefix-other` namespaces.
    Prefix {
        /// Explicit bounded namespace, not an empty whole-bucket wildcard.
        prefix: Object,
    },
}
/// One exact resource/action rule in trusted boot/application configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rule {
    /// Actual resource boundary.
    pub resource: Resource,
    /// Explicit action bits.
    pub actions: Actions,
}
/// One trusted pre-admitted scope/role with finite monotonic lifetime and revocation id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    /// Unique nonzero revocation identity, never a credential.
    pub id: u64,
    /// Exact scope and actual backend role to attenuate.
    pub admission: Admission,
    /// Elapsed monotonic ns since ONE successful authority load this boot, inclusive.
    /// Reconnect/redecode cannot renew this anchor; each new boot needs explicit admission.
    pub valid_from_ns: u64,
    /// Same single-load elapsed clock, exclusive; infinity is forbidden.
    /// This is native capability lifetime, never application-token expiry syntax.
    pub expires_ns: u64,
    /// Explicit resource/action boundaries.
    pub rules: Vec<Rule>,
}
/// Sealed admitted lease. Only this actual trusted set's live admit path constructs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantLease {
    id: u64,
    admission: Admission,
    expires_ns: u64,
}
impl GrantLease {
    /// Exact configured revocation id.
    pub fn id(&self) -> u64 {
        self.id
    }
    /// Finite exclusive monotonic expiry.
    pub fn expires_ns(&self) -> u64 {
        self.expires_ns
    }
    /// Exact admitted request scope/role, not caller-substitutable metadata.
    pub fn admission(&self) -> &Admission {
        &self.admission
    }
}
/// Sole decoded grant authority; production must receive bytes only through a trusted
/// readonly boot/application capability. Public object controls cannot create this set.
pub struct GrantSet {
    grants: Vec<Grant>,
    revoked: BTreeSet<u64>,
}
impl GrantSet {
    /// Validate explicitly supplied trusted grants. No default/implicit grant is made.
    pub fn new(grants: Vec<Grant>) -> Result<Self> {
        if grants.is_empty() || grants.len() > MAX_GRANTS {
            return Err(Error::Limit);
        }
        let mut ids = BTreeSet::new();
        for (index, grant) in grants.iter().enumerate() {
            grant.admission.scope.validate()?;
            if grant.id == 0
                || !ids.insert(grant.id)
                || grant.expires_ns == u64::MAX
                || grant.valid_from_ns >= grant.expires_ns
                || grant.rules.is_empty()
                || grant.rules.len() > MAX_RULES
                || grants[..index]
                    .iter()
                    .any(|prior| prior.admission == grant.admission)
            {
                return Err(Error::Invalid);
            }
            for rule in &grant.rules {
                Actions::new(rule.actions.bits())?;
                match &rule.resource {
                    Resource::Exact { object, version } => {
                        object.validate()?;
                        if object.key.is_empty() && rule.actions.bits() != Actions::LIST {
                            return Err(Error::Invalid);
                        }
                        if let Some(version) = version {
                            version.validate()?;
                            if rule.actions.bits() & Actions::LIST != 0
                                || (rule.actions.bits() & Actions::WRITE != 0
                                    && grant.admission.role
                                        != crate::auth::SessionRole::CacheScratch)
                            {
                                return Err(Error::Invalid);
                            }
                        }
                    }
                    Resource::Prefix { prefix } => {
                        prefix.validate()?;
                        if prefix.key.is_empty() || !prefix.key.ends_with('/') {
                            return Err(Error::Invalid);
                        }
                    }
                }
            }
        }
        Ok(Self {
            grants,
            revoked: BTreeSet::new(),
        })
    }
    /// Encode trusted grants only, without serializing mutable live revocation state.
    pub fn encode_into(&self, destination: &mut [u8]) -> Result<usize> {
        let mut out = wire::Out::new(destination)?;
        out.u8(10)?;
        out.u32(self.grants.len() as u32)?;
        for grant in &self.grants {
            out.u64(grant.id)?;
            grant.admission.write(&mut out)?;
            out.u64(grant.valid_from_ns)?;
            out.u64(grant.expires_ns)?;
            out.u32(grant.rules.len() as u32)?;
            for rule in &grant.rules {
                out.u8(rule.actions.bits())?;
                match &rule.resource {
                    Resource::Exact { object, version } => {
                        out.u8(0)?;
                        out.object(object)?;
                        match version {
                            None => out.u8(0)?,
                            Some(version) => {
                                out.u8(1)?;
                                out.version(version)?;
                            }
                        }
                    }
                    Resource::Prefix { prefix } => {
                        out.u8(1)?;
                        out.object(prefix)?;
                    }
                }
            }
        }
        out.finish()
    }
    /// Decode one exact canonical grant control; all counts are bounded before allocation.
    pub fn decode(source: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(source)?;
        if input.u8()? != 10 {
            return Err(Error::Invalid);
        }
        let count = input.u32()? as usize;
        if count == 0 || count > MAX_GRANTS {
            return Err(Error::Limit);
        }
        let mut grants = Vec::new();
        for _ in 0..count {
            let id = input.u64()?;
            let admission = Admission::read(&mut input)?;
            let valid_from_ns = input.u64()?;
            let expires_ns = input.u64()?;
            let count = input.u32()? as usize;
            if count == 0 || count > MAX_RULES {
                return Err(Error::Limit);
            }
            let mut rules = Vec::new();
            for _ in 0..count {
                let actions = Actions::new(input.u8()?)?;
                let resource = match input.u8()? {
                    0 => {
                        let object = input.object()?;
                        let version = match input.u8()? {
                            0 => None,
                            1 => Some(input.version()?),
                            _ => return Err(Error::Invalid),
                        };
                        Resource::Exact { object, version }
                    }
                    1 => Resource::Prefix {
                        prefix: input.object()?,
                    },
                    _ => return Err(Error::Invalid),
                };
                rules.push(Rule { resource, actions });
            }
            grants.push(Grant {
                id,
                admission,
                valid_from_ns,
                expires_ns,
                rules,
            });
        }
        input.finish()?;
        Self::new(grants)
    }
    /// Decode exact canonical bytes followed only by zero padding in trusted boot Memory.
    pub fn decode_page(source: &[u8]) -> Result<Self> {
        let length = wire::header_bytes(source)?;
        if length > source.len() || source[length..].iter().any(|byte| *byte != 0) {
            return Err(Error::Incomplete);
        }
        Self::decode(&source[..length])
    }
    /// Attenuate an existing exact live grant; no request can add a scope, role or rule.
    /// `now_ns` is checked elapsed svc monotonic time from the authority's ONE successful
    /// load this boot. Provider retains that anchor, denies backward time/overflow and
    /// never renews it on reconnect/redecode. This pure codec creates no clock source.
    pub fn admit(
        &self,
        admission: &Admission,
        now_ns: u64,
    ) -> core::result::Result<GrantLease, Refusal> {
        let grant = self
            .grants
            .iter()
            .find(|grant| {
                &grant.admission == admission
                    && !self.revoked.contains(&grant.id)
                    && grant.valid_from_ns <= now_ns
                    && now_ns < grant.expires_ns
            })
            .ok_or(Refusal::Denied)?;
        Ok(GrantLease {
            id: grant.id,
            admission: grant.admission.clone(),
            expires_ns: grant.expires_ns,
        })
    }
    /// Fresh exact-action decision. Application proof is NEVER interpreted as native
    /// authority. Version-restricted rules deny missing/different version context.
    pub fn verify(
        &self,
        lease: &GrantLease,
        auth: &Authorization<'_>,
        now_ns: u64,
    ) -> core::result::Result<(), Refusal> {
        if !auth.proof().native_session()
            || auth.scope() != &lease.admission.scope
            || self.revoked.contains(&lease.id)
        {
            return Err(Refusal::Denied);
        }
        let grant = self
            .grants
            .iter()
            .find(|grant| {
                grant.id == lease.id
                    && grant.admission == lease.admission
                    && grant.expires_ns == lease.expires_ns
                    && grant.valid_from_ns <= now_ns
                    && now_ns < grant.expires_ns
            })
            .ok_or(Refusal::Denied)?;
        let allowed = grant.rules.iter().any(|rule| {
            rule.actions.allows(auth.action())
                && match &rule.resource {
                    Resource::Exact { object, version } => {
                        object == auth.object()
                            && version
                                .as_ref()
                                .is_none_or(|version| auth.version() == Some(version))
                    }
                    Resource::Prefix { prefix } => {
                        prefix.workspace == auth.object().workspace
                            && prefix.bucket == auth.object().bucket
                            && auth.object().key.starts_with(&prefix.key)
                    }
                }
        });
        if allowed {
            Ok(())
        } else {
            Err(Refusal::Denied)
        }
    }
    /// Revoke the actual configured id for all existing/future leases immediately.
    pub fn revoke(&mut self, id: u64) -> core::result::Result<(), Refusal> {
        if !self.grants.iter().any(|grant| grant.id == id) {
            return Err(Refusal::Invalid);
        }
        self.revoked.insert(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Scope, auth::SessionRole};
    fn admission() -> Admission {
        Admission {
            scope: Scope {
                workspace: "requester".into(),
                run: "run".into(),
            },
            role: SessionRole::BucketHome,
        }
    }
    fn object(key: &str) -> Object {
        Object {
            workspace: "foreign-owner".into(),
            bucket: "home".into(),
            key: key.into(),
        }
    }
    fn grant(resource: Resource) -> Grant {
        Grant {
            id: 7,
            admission: admission(),
            valid_from_ns: 10,
            expires_ns: 100,
            rules: alloc::vec![Rule {
                resource,
                actions: Actions::new(Actions::READ).unwrap()
            }],
        }
    }
    #[test]
    fn exact_context_actions_namespace_time_and_revocation_never_widen() {
        let mut set = GrantSet::new(alloc::vec![grant(Resource::Prefix {
            prefix: object("allowed/")
        })])
        .unwrap();
        let admitted = admission();
        let target = object("allowed/rows");
        let auth = Authorization::native(&admitted.scope, &target, Action::Read).unwrap();
        assert!(set.admit(&admitted, 9).is_err());
        let lease = set.admit(&admitted, 10).unwrap();
        assert_eq!(lease.id(), 7);
        assert_eq!(lease.expires_ns(), 100);
        assert!(set.verify(&lease, &auth, 99).is_ok()); // explicit foreign owner, no owner-equality rule
        assert!(set.verify(&lease, &auth, 100).is_err());
        assert!(
            set.admit(
                &Admission {
                    role: SessionRole::CheckpointMetadata,
                    ..admitted.clone()
                },
                11
            )
            .is_err()
        );
        let adjacent = object("allowed-other/rows");
        assert!(
            set.verify(
                &lease,
                &Authorization::native(&admitted.scope, &adjacent, Action::Read).unwrap(),
                11
            )
            .is_err()
        );
        assert!(
            set.verify(
                &lease,
                &Authorization::native(&admitted.scope, &target, Action::List).unwrap(),
                11
            )
            .is_err()
        );
        assert!(
            set.verify(
                &lease,
                &Authorization::new(
                    &admitted.scope,
                    &target,
                    Action::Read,
                    b"arbitrary-application-bytes"
                )
                .unwrap(),
                11
            )
            .is_err()
        );
        let other = Scope {
            run: "other".into(),
            ..admitted.scope.clone()
        };
        assert!(
            set.verify(
                &lease,
                &Authorization::native(&other, &target, Action::Read).unwrap(),
                11
            )
            .is_err()
        );
        set.revoke(7).unwrap();
        assert!(set.verify(&lease, &auth, 11).is_err());
        assert!(set.admit(&admitted, 11).is_err());
        assert!(set.revoke(123).is_err());
    }
    #[test]
    fn exact_version_and_canonical_codec_refuse_missing_or_mutated_proof() {
        let pinned = Version {
            etag: Some("actual-v1".into()),
            generation: None,
        };
        let target = object("exact");
        let set = GrantSet::new(alloc::vec![grant(Resource::Exact {
            object: target.clone(),
            version: Some(pinned.clone())
        })])
        .unwrap();
        let mut page = [0; 4096];
        let length = set.encode_into(&mut page).unwrap();
        let set = GrantSet::decode_page(&page).unwrap();
        let admitted = admission();
        let lease = set.admit(&admitted, 11).unwrap();
        assert!(
            set.verify(
                &lease,
                &Authorization::native(&admitted.scope, &target, Action::Read).unwrap(),
                11
            )
            .is_err()
        );
        let auth = Authorization::native(&admitted.scope, &target, Action::Read)
            .unwrap()
            .with_version(&pinned)
            .unwrap();
        assert!(set.verify(&lease, &auth, 11).is_ok());
        let mut authority = [0; 4096];
        let n = auth.encode_into(9, &mut authority).unwrap();
        let decoded = crate::auth::DecodedAuthorization::decode(&authority[..n]).unwrap();
        assert_eq!(decoded.authorization().version(), Some(&pinned));
        assert!(set.verify(&lease, &decoded.authorization(), 11).is_ok());
        let wrong = Version {
            etag: Some("actual-v2".into()),
            generation: None,
        };
        assert!(
            set.verify(
                &lease,
                &Authorization::native(&admitted.scope, &target, Action::Read)
                    .unwrap()
                    .with_version(&wrong)
                    .unwrap(),
                11
            )
            .is_err()
        );
        for cut in 0..length {
            assert!(GrantSet::decode(&page[..cut]).is_err());
        }
        page[length] = 1;
        assert!(GrantSet::decode_page(&page).is_err());
        let mut invalid = grant(Resource::Prefix {
            prefix: object("unbounded-no-slash"),
        });
        assert!(GrantSet::new(alloc::vec![invalid.clone()]).is_err());
        invalid.rules[0].resource = Resource::Prefix {
            prefix: object("safe/"),
        };
        invalid.expires_ns = u64::MAX;
        assert!(GrantSet::new(alloc::vec![invalid]).is_err());
        assert!(Actions::new(0).is_err());
        assert!(Actions::new(128).is_err());
    }
}
