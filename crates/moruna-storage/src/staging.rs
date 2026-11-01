//! Accounting for SDK ingress staging, separate from the caller's charged destination.
//! Reservations cover requested bounded response bytes, not a claimed direct-network path.
#![forbid(unsafe_code)]
use crate::{Error, Result, Scope};
use moruna_kernel::{Reservation, Reserve, Residency, Tier};
use std::sync::Arc;

/// A charge retained through a complete owned response body operation.
pub trait Lease: Send {}
impl Lease for Reservation {}
/// No production uncharged/permissive default exists. The orchestrator injects the run's
/// actual budget authority, which remains alive while owned network work is outstanding.
pub trait Staging: Send + Sync {
    /// Reserve worst-case SDK payload staging before starting the bounded GET.
    fn reserve(&self, scope: &Scope, bytes: u64) -> Result<Box<dyn Lease>>;
}
/// Production adapter to the existing residency/admission catalog.
pub struct Catalog {
    residency: Arc<dyn Residency>,
    tier: Tier,
    scope: Scope,
}
impl Catalog {
    /// Bind the run's real host-tier budget; a device/disk budget cannot cover HTTP bytes.
    pub fn new(residency: Arc<dyn Residency>, tier: Tier, scope: Scope) -> Result<Self> {
        if !tier.is_host() {
            return Err(Error::Invalid("SDK staging requires host budget"));
        }
        scope.validate()?;
        Ok(Self {
            residency,
            tier,
            scope,
        })
    }
}
impl Staging for Catalog {
    fn reserve(&self, scope: &Scope, bytes: u64) -> Result<Box<dyn Lease>> {
        scope.validate()?;
        if *scope != self.scope {
            return Err(Error::Denied);
        }
        match self
            .residency
            .reserve(self.tier, bytes)
            .map_err(|e| Error::Arena(Box::new(e)))?
        {
            Reserve::Granted(reservation) => Ok(Box::new(reservation)),
            Reserve::Pending { need } => Err(Error::StagingPending { need }),
            Reserve::Refused { need, budget } => Err(Error::StagingRefused { need, budget }),
        }
    }
}
