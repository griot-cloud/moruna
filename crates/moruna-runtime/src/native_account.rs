//! Native pressure is actual Account charges, never a fabricated `/proc` RSS.
use moruna_kernel::{MemoryBasis, MorunaError, ProcessPeak, Result, Sample, Sampler};
use std::sync::{Arc, Mutex};

// Private transport boundaries keep the canonical error boxed; Sampler still returns it unchanged.
type AccountingResult<T> = std::result::Result<T, Box<MorunaError>>;

pub(crate) struct Reading {
    pub(crate) used: u64,
    pub(crate) reserved: u64,
    pub(crate) limit: u64,
}
#[derive(Default)]
struct Observed {
    interval_peak: u64,
    process_peak: ProcessPeak,
}
struct NativeAccount {
    read: Box<dyn Fn() -> AccountingResult<Reading> + Send + Sync>,
    limits: Box<dyn Fn() -> (u64, f64) + Send + Sync>,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
    observed: Mutex<Observed>,
}
impl NativeAccount {
    fn new(
        read: impl Fn() -> AccountingResult<Reading> + Send + Sync + 'static,
        limits: impl Fn() -> (u64, f64) + Send + Sync + 'static,
        now: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> AccountingResult<Self> {
        let sampler = Self {
            read: Box::new(read),
            limits: Box::new(limits),
            now: Box::new(now),
            observed: Mutex::new(Observed::default()),
        };
        sampler.sample()?; // Fail closed before arena sizing, including the first read.
        Ok(sampler)
    }
    fn held(&self) -> AccountingResult<std::sync::MutexGuard<'_, Observed>> {
        self.observed.lock().map_err(|_| {
            Box::new(MorunaError::Config {
                name: "native.account",
                msg: "observed sampler state poisoned".into(),
            })
        })
    }
}
impl Sampler for NativeAccount {
    fn memory_basis(&self) -> MemoryBasis {
        MemoryBasis::NativeAccountCharge
    }
    fn sample(&self) -> Result<Sample> {
        let mut observed = self.held().map_err(|error| *error)?;
        let actual = (self.read)().map_err(|error| *error)?;
        let (ceiling, cpu) = (self.limits)();
        if actual.limit == 0
            || ceiling == 0
            || actual
                .used
                .checked_add(actual.reserved)
                .is_none_or(|n| n > actual.limit)
        {
            return Err(MorunaError::Config {
                name: "native.account",
                msg: "actual Account ledger or published run ceiling is invalid".into(),
            });
        }
        let now = (self.now)();
        observed.interval_peak = observed.interval_peak.max(actual.used);
        if actual.used > observed.process_peak.bytes {
            observed.process_peak = ProcessPeak {
                bytes: actual.used,
                at_ns: now,
                since_ns: now,
                exact: false,
            };
        }
        Ok(Sample {
            // Only actual own charges: child ceilings are separately retained by
            // TaskMemory/WithOutside. `reserved` is checked but never added here.
            anon_bytes: actual.used,
            peak_anon_bytes: observed.interval_peak,
            ceiling_bytes: ceiling.min(actual.limit),
            cpu_limit: cpu,
            at_ns: now,
            ..Sample::default() // File residency/CPU throttling not separately measured.
        })
    }
    fn reset_peak(&self) {
        // No syscall or fallible read can be hidden in this infallible method.
        // The next fresh sample establishes the new observed interval's peak.
        // Taking the inner counter does not clear the poison: sample() still
        // refuses it. This only resets the non-measurement interval boundary.
        let mut observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observed.interval_peak = 0;
    }
    fn process_peak(&self) -> ProcessPeak {
        // A poisoned reader cannot invent an exact mark. Its next sample fails;
        // retain only previously observed evidence for the terminal report.
        match self.observed.lock() {
            Ok(observed) => observed.process_peak,
            Err(poisoned) => poisoned.into_inner().process_peak,
        }
    }
    // Default process_usage is explicitly unmeasured: no CPU/billing estimate.
}

pub(crate) fn sampler(
    read: impl Fn() -> AccountingResult<Reading> + Send + Sync + 'static,
    limits: impl Fn() -> (u64, f64) + Send + Sync + 'static,
    now: impl Fn() -> u64 + Send + Sync + 'static,
) -> AccountingResult<Arc<dyn Sampler>> {
    Ok(Arc::new(NativeAccount::new(read, limits, now)?))
}

/// The instances each of a run's native stages asks for: each live instance of a stage is a
/// child of its own (one task process each, as many as `moruna_adapters::instances` gives the
/// stage), each funded with one share and carving its account within it. A stage that is not
/// stateful has no instance count and is refused.
pub(crate) fn native_instances(
    kinds: impl IntoIterator<Item = moruna_kernel::KernelKind>,
) -> AccountingResult<Vec<usize>> {
    kinds
        .into_iter()
        .map(|kind| match kind {
            moruna_kernel::KernelKind::Stateful { max_instances } => Ok(max_instances.get()),
            moruna_kernel::KernelKind::Stateless => Err(MorunaError::Config {
                name: "budget.native_children",
                msg: "a native child's stage must be stateful, its instances its children".into(),
            }
            .into()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    struct Fixture {
        used: AtomicU64,
        reserved: AtomicU64,
        limit: AtomicU64,
        cap: AtomicU64,
        failed: AtomicBool,
    }
    fn fixture() -> (Arc<Fixture>, NativeAccount) {
        let state = Arc::new(Fixture {
            used: AtomicU64::new(40),
            reserved: AtomicU64::new(200),
            limit: AtomicU64::new(1000),
            cap: AtomicU64::new(400),
            failed: AtomicBool::new(false),
        });
        let read = state.clone();
        let limits = state.clone();
        let sampler = NativeAccount::new(
            move || {
                if read.failed.load(Ordering::SeqCst) {
                    return Err(MorunaError::Io {
                        op: "account_info",
                        target: "TEST-ONLY Account".into(),
                        msg: "read refused".into(),
                    }
                    .into());
                }
                Ok(Reading {
                    used: read.used.load(Ordering::SeqCst),
                    reserved: read.reserved.load(Ordering::SeqCst),
                    limit: read.limit.load(Ordering::SeqCst),
                })
            },
            move || (limits.cap.load(Ordering::SeqCst), 2.0),
            || 99,
        )
        .unwrap();
        (state, sampler)
    }
    #[test]
    fn current_charges_follow_run_ceiling_and_actual_account_cap() {
        let (state, sampler) = fixture();
        let first = sampler.sample().unwrap();
        assert_eq!(first.anon_bytes, 40);
        assert_eq!(first.ceiling_bytes, 400);
        state.cap.store(1500, Ordering::SeqCst);
        assert_eq!(sampler.sample().unwrap().ceiling_bytes, 1000);
        state.cap.store(20, Ordering::SeqCst);
        let lowered = sampler.sample().unwrap();
        assert_eq!(lowered.ceiling_bytes, 20);
        assert_eq!(lowered.anon_bytes, 40);
        assert_eq!(sampler.memory_basis(), MemoryBasis::NativeAccountCharge);
        assert!(!sampler.process_usage().measured);
        assert!(!sampler.process_peak().exact);
    }
    /// An account whose ledger is not one (no limit, or more charged than its limit) or a run
    /// with no published ceiling is refused, never sampled.
    #[test]
    fn an_invalid_ledger_or_ceiling_is_refused() {
        let (state, sampler) = fixture();
        state.limit.store(0, Ordering::SeqCst);
        assert!(sampler.sample().unwrap_err().to_string().contains("ledger"));
        state.limit.store(100, Ordering::SeqCst);
        assert!(
            sampler.sample().is_err(),
            "used and reserved exceed the limit"
        );
        state.limit.store(1000, Ordering::SeqCst);
        state.cap.store(0, Ordering::SeqCst);
        assert!(sampler.sample().is_err(), "no published ceiling");
        state.cap.store(400, Ordering::SeqCst);
        sampler.sample().unwrap();
    }

    #[test]
    fn reset_changes_only_observed_interval_not_lifetime_evidence() {
        let (state, sampler) = fixture();
        state.used.store(90, Ordering::SeqCst);
        assert_eq!(sampler.sample().unwrap().peak_anon_bytes, 90);
        sampler.reset_peak();
        state.used.store(12, Ordering::SeqCst);
        assert_eq!(sampler.sample().unwrap().peak_anon_bytes, 12);
        assert_eq!(sampler.process_peak().bytes, 90);
    }
    #[test]
    fn failed_current_read_never_returns_stale_or_zero_and_initial_read_is_required() {
        let (state, sampler) = fixture();
        state.failed.store(true, Ordering::SeqCst);
        assert!(
            matches!(sampler.sample(),Err(MorunaError::Io { target, .. }) if target=="TEST-ONLY Account")
        );
        assert_eq!(sampler.process_peak().bytes, 40);
        assert!(
            NativeAccount::new(|| Err(MorunaError::Cancelled.into()), || (400, 1.0), || 99)
                .is_err()
        );
    }
    #[test]
    fn reserved_child_ceilings_are_not_added_to_native_base_pressure() {
        let (state, sampler) = fixture();
        state.reserved.store(800, Ordering::SeqCst);
        assert_eq!(sampler.sample().unwrap().anon_bytes, 40);
        assert_eq!(sampler.memory_basis(), MemoryBasis::NativeAccountCharge);
        assert!(!sampler.process_usage().measured);
    }
    #[test]
    fn public_factory_is_the_same_fallible_sampler_and_default_basis_remains_compatible() {
        let sampler = super::sampler(
            || {
                Ok(Reading {
                    used: 7,
                    reserved: 2,
                    limit: 30,
                })
            },
            || (20, 1.0),
            || 1,
        )
        .unwrap();
        assert_eq!(sampler.sample().unwrap().anon_bytes, 7);
        assert_eq!(sampler.memory_basis(), MemoryBasis::NativeAccountCharge);
        struct Existing;
        impl Sampler for Existing {
            fn sample(&self) -> Result<Sample> {
                Err(MorunaError::Cancelled)
            }
            fn reset_peak(&self) {}
            fn process_peak(&self) -> ProcessPeak {
                ProcessPeak::default()
            }
        }
        assert_eq!(Existing.memory_basis(), MemoryBasis::ProcessAnon);
    }
}

#[cfg(test)]
mod child_share_tests {
    fn stateful(n: usize) -> moruna_kernel::KernelKind {
        moruna_kernel::KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::new(n).unwrap(),
        }
    }

    /// Each instance of a native stage is a child: a stage asks for its instances, and a stage
    /// that is not stateful has none (the shares are `moruna_adapters::instances::fund`'s, 6.3e).
    #[test]
    fn every_instance_of_a_native_stage_is_a_child() {
        let wanted = super::native_instances([stateful(4), stateful(3)]).unwrap();
        assert_eq!(wanted, vec![4, 3]);
        assert_eq!(super::native_instances([stateful(1)]).unwrap(), vec![1]);
        assert!(super::native_instances([]).unwrap().is_empty());
        let refused = super::native_instances([stateful(2), moruna_kernel::KernelKind::Stateless])
            .unwrap_err()
            .to_string();
        assert!(refused.contains("must be stateful"), "{refused}");
    }
}
