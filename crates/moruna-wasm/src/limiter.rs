//! The resource limiter: every growth of an instance's memory, decided by Moruna first (17 e.2,
//! SB-I2).

use std::sync::Arc;

use moruna_adapters::guard::{MemoryGate, Refusal};
use moruna_kernel::AllocCounts;
use wasmtime::ResourceLimiter;

use crate::{INSTANCES_MAX, MEMORIES_MAX, TABLE_ELEMENTS_MAX, WASM32_MAX_BYTES};

/// The store's data: the stage's limits and the call's linear counts.
pub struct Limits {
    gate: Option<Arc<MemoryGate>>,
    guarded: bool,
    /// Bytes of linear memory the instance holds now.
    size: u64,
    /// Admitted by the gate and not yet answered: settled when the next growth or the call's end
    /// comes without a failure, credited when wasmtime reports one.
    pending: u64,
    counts: AllocCounts,
    refusal: Option<Refusal>,
}

impl Limits {
    /// Limits with the run's gate, refusing through it when `guarded`.
    pub fn new(gate: Option<Arc<MemoryGate>>, guarded: bool) -> Limits {
        Limits {
            gate,
            guarded,
            size: 0,
            pending: 0,
            counts: AllocCounts::default(),
            refusal: None,
        }
    }

    /// No gate: the limit is wasm32's own.
    pub fn unbound() -> Limits {
        Limits::new(None, false)
    }

    /// The stage's ceiling for linear memory (b): the ceiling in force when a gate is bound,
    /// wasm32's four GiB otherwise.
    fn ceiling(&self) -> u64 {
        self.gate
            .as_ref()
            .map(|g| g.ceiling())
            .filter(|c| *c > 0)
            .unwrap_or(WASM32_MAX_BYTES)
            .min(WASM32_MAX_BYTES)
    }

    /// Whether the gate can refuse this instance's growth.
    pub fn refusal_on(&self) -> bool {
        self.guarded && self.gate.is_some()
    }

    /// The linear memory the instance holds.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The call's first refusal.
    pub fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }

    fn settle(&mut self) {
        if self.pending > 0 {
            if let Some(gate) = &self.gate {
                gate.settle(self.pending);
            }
            self.pending = 0;
        }
    }

    /// A call begins: fresh counts.
    pub fn begin_call(&mut self) {
        self.settle();
        self.counts = AllocCounts::default();
        self.refusal = None;
    }

    /// A call ends: its counts, with `peak` the memory's size now (a linear memory never
    /// shrinks).
    pub fn end_call(&mut self) -> AllocCounts {
        self.settle();
        let mut counts = self.counts;
        counts.peak = self.size;
        counts
    }

    fn refuse(&mut self, refusal: Refusal) -> bool {
        self.counts.refused += 1;
        if self.refusal.is_none() {
            self.refusal = Some(refusal);
        }
        false
    }
}

impl ResourceLimiter for Limits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.settle();
        let growth = desired.saturating_sub(current) as u64;
        self.counts.requests += 1;
        self.counts.bytes = self.counts.bytes.saturating_add(growth);
        self.counts.largest = self.counts.largest.max(growth);
        let ceiling = self.ceiling();
        if desired as u64 > ceiling {
            let refusal = Refusal {
                requested: growth,
                in_use: self.size,
                ceiling,
            };
            return Ok(self.refuse(refusal));
        }
        if self.guarded
            && let Some(gate) = &self.gate
        {
            match gate.admit(growth) {
                Ok(()) => self.pending = growth,
                Err(refusal) => return Ok(self.refuse(refusal)),
            }
        }
        self.size = self.size.saturating_add(growth);
        Ok(true)
    }

    fn memory_grow_failed(&mut self, _error: wasmtime::Error) -> wasmtime::Result<()> {
        if self.pending > 0 {
            if let Some(gate) = &self.gate {
                gate.credit(self.pending);
            }
            self.size = self.size.saturating_sub(self.pending);
            self.pending = 0;
        }
        Ok(())
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= TABLE_ELEMENTS_MAX)
    }

    fn instances(&self) -> usize {
        INSTANCES_MAX
    }

    fn tables(&self) -> usize {
        INSTANCES_MAX
    }

    fn memories(&self) -> usize {
        MEMORIES_MAX
    }
}
