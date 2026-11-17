// SPDX-License-Identifier: MPL-2.0

//! CPU-local variables in user space.
//!
//! A user process does not know which CPU it runs on (every thread is on CPU 0 for
//! per-CPU state, `CpuId::current_racy`), so a CPU-local variable is one value per CPU
//! the core reported, reached by CPU id, and shared by the threads on it: its type must
//! be `Sync`.

use core::ops::Deref;

use super::CpuId;
use crate::{task::atomic_mode::InAtomicMode, util::id_set::Id};

/// The most CPUs a personality instance keeps state for.
pub const MAX_CPUS: usize = 64;

/// A CPU-local variable: one value per CPU.
pub struct CpuLocal<T: 'static> {
    values: [T; MAX_CPUS],
}

/// The type `cpu_local!` defines.
pub type StaticCpuLocal<T> = CpuLocal<T>;

impl<T: 'static> CpuLocal<T> {
    #[doc(hidden)]
    pub const fn __new(values: [T; MAX_CPUS]) -> Self {
        Self { values }
    }

    /// The value of the current CPU, under a guard that pins the thread.
    pub fn get_with<'a>(&'a self, _guard: &'a dyn InAtomicMode) -> CpuLocalDerefGuard<'a, T> {
        CpuLocalDerefGuard(&self.values[CpuId::current_racy().as_usize()])
    }
}

impl<T: 'static + Sync> CpuLocal<T> {
    /// The value of CPU `cpu`.
    pub fn get_on_cpu(&self, cpu: CpuId) -> &T {
        &self.values[cpu.as_usize()]
    }
}

/// A reference to the current CPU's value.
pub struct CpuLocalDerefGuard<'a, T: 'static>(&'a T);

impl<'a, T: 'static> CpuLocalDerefGuard<'a, T> {
    /// The value, for as long as the variable lives.
    pub fn deref_target(&self) -> &'a T {
        self.0
    }
}

impl<T: 'static> Deref for CpuLocalDerefGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0
    }
}

// SAFETY: a value is shared by the threads that reach it, which hold it only through
// shared references; `get_with` needs `T: Sync` to be sent across threads as well.
unsafe impl<T: Send + Sync + 'static> Sync for CpuLocal<T> {}

/// Defines CPU-local variables.
#[macro_export]
macro_rules! cpu_local {
    ($( $(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr; )*) => {
        $(
            $(#[$attr])* $vis static $name: $crate::cpu::local::StaticCpuLocal<$t> =
                $crate::cpu::local::CpuLocal::__new([const { $init }; $crate::cpu::local::MAX_CPUS]);
        )*
    };
}

/// A value per CPU, allocated at run time: OSDK's heap allocator's per-CPU box.
pub struct CpuLocalBox<T> {
    values: alloc::vec::Vec<T>,
}

impl<T> CpuLocalBox<T> {
    /// The value of CPU `cpu`.
    pub fn get_on_cpu(&self, cpu: CpuId) -> &T {
        &self.values[cpu.as_usize()]
    }
}

/// Allocates a value per CPU, each made by `init` from its CPU.
pub fn alloc_cpu_local<T>(mut init: impl FnMut(CpuId) -> T) -> crate::Result<CpuLocalBox<T>> {
    let mut values = alloc::vec::Vec::new();
    values
        .try_reserve_exact(super::num_cpus())
        .map_err(|_| crate::Error::NoMemory)?;
    values.extend(super::all_cpus().map(&mut init));
    Ok(CpuLocalBox { values })
}

/// A primitive a [`CpuLocalCell`] holds, kept as the low bits of a `u64`.
pub trait CellPrimitive: Copy {
    #[doc(hidden)]
    fn from_bits(bits: u64) -> Self;
    #[doc(hidden)]
    fn to_bits(self) -> u64;
}

macro_rules! cell_primitive {
    ($($t:ty),*) => {$(
        impl CellPrimitive for $t {
            fn from_bits(bits: u64) -> Self {
                bits as $t
            }
            fn to_bits(self) -> u64 {
                self as u64
            }
        }
    )*};
}

cell_primitive!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

impl CellPrimitive for bool {
    fn from_bits(bits: u64) -> Self {
        bits & 1 != 0
    }
    fn to_bits(self) -> u64 {
        self as u64
    }
}

/// A CPU-local cell: in user space, one atomic shared by the threads on CPU 0. Integers
/// keep their wrapping arithmetic in the low bits.
pub struct CpuLocalCell<T: CellPrimitive> {
    value: core::sync::atomic::AtomicU64,
    _t: core::marker::PhantomData<T>,
}

// SAFETY: the value is an atomic.
unsafe impl<T: CellPrimitive> Sync for CpuLocalCell<T> {}

impl<T: CellPrimitive> CpuLocalCell<T> {
    #[doc(hidden)]
    pub const fn __new(bits: u64) -> Self {
        Self {
            value: core::sync::atomic::AtomicU64::new(bits),
            _t: core::marker::PhantomData,
        }
    }

    /// The value.
    pub fn load(&'static self) -> T {
        T::from_bits(self.value.load(core::sync::atomic::Ordering::Relaxed))
    }

    /// Sets the value.
    pub fn store(&'static self, v: T) {
        self.value
            .store(v.to_bits(), core::sync::atomic::Ordering::Relaxed)
    }

    /// Adds `v`.
    pub fn add_assign(&'static self, v: T) {
        self.value
            .fetch_add(v.to_bits(), core::sync::atomic::Ordering::Relaxed);
    }

    /// Subtracts `v`.
    pub fn sub_assign(&'static self, v: T) {
        self.value
            .fetch_sub(v.to_bits(), core::sync::atomic::Ordering::Relaxed);
    }

    /// Ors in `v`.
    pub fn bitor_assign(&'static self, v: T) {
        self.value
            .fetch_or(v.to_bits(), core::sync::atomic::Ordering::Relaxed);
    }

    /// Ands with `v`.
    pub fn bitand_assign(&'static self, v: T) {
        self.value
            .fetch_and(v.to_bits(), core::sync::atomic::Ordering::Relaxed);
    }

    /// Xors with `v`.
    pub fn bitxor_assign(&'static self, v: T) {
        self.value
            .fetch_xor(v.to_bits(), core::sync::atomic::Ordering::Relaxed);
    }
}

/// Defines CPU-local cells.
#[macro_export]
macro_rules! cpu_local_cell {
    ($( $(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr; )*) => {
        $(
            $(#[$attr])* $vis static $name: $crate::cpu::local::CpuLocalCell<$t> =
                $crate::cpu::local::CpuLocalCell::__new(($init) as u64);
        )*
    };
}
