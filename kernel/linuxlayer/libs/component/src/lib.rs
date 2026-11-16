// SPDX-License-Identifier: MPL-2.0

//! Asterinas's components, in user space.
//!
//! `#[init_component]` registers a component's init function, as in Asterinas, and
//! `init_all` runs a stage's functions. Asterinas orders them by the dependencies in its
//! `Components.toml`; here they run by crate name in the order `ORDER` gives, then the
//! rest.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

pub use component_macro::init_component;
#[doc(hidden)]
pub use inventory::submit;

/// Why a component failed to start.
#[derive(Debug)]
pub enum ComponentInitError {
    /// It was not found.
    UninitializedComponent(&'static str),
    /// It failed.
    Unknown,
}

/// The stage a component starts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitStage {
    /// Before the first process.
    Bootstrap,
    /// With the first process.
    Process,
    /// With kernel threads.
    Kthread,
}

/// A component's init function.
pub struct ComponentInit {
    stage: InitStage,
    path: &'static str,
    init: fn() -> Result<(), ComponentInitError>,
}

impl ComponentInit {
    #[doc(hidden)]
    pub const fn new(
        stage: InitStage,
        path: &'static str,
        init: fn() -> Result<(), ComponentInitError>,
    ) -> Self {
        Self { stage, path, init }
    }

    /// The crate that registered it.
    pub fn crate_name(&self) -> &'static str {
        self.path.split("::").next().unwrap_or(self.path)
    }
}

inventory::collect!(ComponentInit);

/// The crates whose components start first, in this order: each one's dependencies
/// come before it.
pub const ORDER: &[&str] = &[
    "aster_cmdline",
    "aster_logger",
    "aster_time",
    "aster_systree",
    "aster_device",
    "aster_softirq",
    "aster_console",
    "aster_block",
];

/// Asterinas's component metadata; the order is `ORDER` here.
#[macro_export]
macro_rules! parse_metadata {
    () => {
        ()
    };
}

/// Runs every init function registered for `stage`, in `ORDER`, then the rest.
pub fn init_all(stage: InitStage, _metadata: ()) -> Result<(), ComponentInitError> {
    let mut inits: Vec<&ComponentInit> = inventory::iter::<ComponentInit>
        .into_iter()
        .filter(|c| c.stage == stage)
        .collect();
    let rank = |c: &ComponentInit| {
        ORDER
            .iter()
            .position(|n| *n == c.crate_name())
            .unwrap_or(ORDER.len())
    };
    inits.sort_by_key(|c| (rank(c), c.path));
    for c in inits {
        (c.init)()?;
    }
    Ok(())
}

/// The components registered for `stage`, by module path.
pub fn registered(stage: InitStage) -> impl Iterator<Item = &'static str> {
    inventory::iter::<ComponentInit>
        .into_iter()
        .filter(move |c| c.stage == stage)
        .map(|c| c.path)
}
