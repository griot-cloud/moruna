//! Explicit offline disk-v3 provisioning, never a mount fallback or object backend.
//! The operator must own the target. Images are exclusively locked regular files;
//! Linux raw targets use the existing O_DIRECT|O_EXCL validated Device boundary.
use moruna_storage_format::{
    Role,
    banks::Root,
    config::{BootConfig, Provider, Region, validate_regions},
};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::Path,
};
/// Maximum supported provisioning alignment; fixed aligned stack scratch is bounded.
pub const MAX_ALIGNMENT: usize = 65536;
#[repr(align(65536))]
struct Scratch([u8; MAX_ALIGNMENT]);
/// Exact provisioning phase, retained on refusal/uncertainty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// Validate explicit input, physical geometry, role and exclusive ownership.
    Preflight,
    /// Prove BOTH existing root cells are zero before the first write.
    ReadRoots,
    /// Clear the initial bounded journal, not historical payload media.
    ClearJournal,
    /// Complete the journal barrier before root publication.
    SyncJournal,
    /// Write the only initial root authority.
    WriteRoot,
    /// Complete the initial-root durability barrier.
    SyncRoot,
    /// Read and verify the exact persisted initial root.
    Readback,
}
/// Typed provisioning refusal. Uncertain means target bytes may have changed; do not
/// interpret it as rollback or blindly retry against a now nonzero root.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Explicit input/geometry/ownership policy refused before mutation.
    #[error("provision {phase:?}: {reason}")]
    Invalid {
        /// Actual phase.
        phase: Phase,
        /// Nonsecret fixed diagnostic.
        reason: &'static str,
    },
    /// Actual filesystem/device operation failure.
    #[error("provision {phase:?} (target_may_have_changed={uncertain}): {source}")]
    Io {
        /// Actual phase.
        phase: Phase,
        /// Target mutation may already have occurred.
        uncertain: bool,
        /// Original OS diagnostic.
        #[source]
        source: std::io::Error,
    },
    /// The sole common root codec refused the artifact.
    #[error("provision {phase:?} (target_may_have_changed={uncertain}): {source}")]
    Format {
        /// Actual phase.
        phase: Phase,
        /// Target mutation may already have occurred.
        uncertain: bool,
        /// Original canonical format refusal.
        #[source]
        source: moruna_storage_format::Error,
    },
    /// The existing strict Linux raw boundary refused opening/ownership.
    #[error("provision raw preflight: {0}")]
    Raw(#[source] Box<crate::Error>),
}
impl Error {
    /// Exact last attempted phase; raw-open failures are always preflight.
    pub fn phase(&self) -> Phase {
        match self {
            Self::Invalid { phase, .. } | Self::Io { phase, .. } | Self::Format { phase, .. } => {
                *phase
            }
            Self::Raw(_) => Phase::Preflight,
        }
    }
    /// True when this failure cannot claim rollback or untouched target media.
    pub fn target_may_have_changed(&self) -> bool {
        match self {
            Self::Io { uncertain, .. } | Self::Format { uncertain, .. } => *uncertain,
            Self::Invalid {
                phase: Phase::Readback,
                ..
            } => true,
            _ => false,
        }
    }
}
/// One formatter result, not a plaintext store/facade or physical-device performance proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    /// Actual validated root now synced and read back.
    pub root: Root,
    /// True only for a validated Linux raw block device, false for an owned disk image.
    pub raw_device: bool,
}
fn invalid(reason: &'static str) -> Error {
    Error::Invalid {
        phase: Phase::Preflight,
        reason,
    }
}
fn io(phase: Phase, uncertain: bool, source: std::io::Error) -> Error {
    Error::Io {
        phase,
        uncertain,
        source,
    }
}
/// Validate the explicit initial artifact before any target mutation. No future/old
/// major, noninitial bank, unsupported role or oversized alignment is reinterpreted.
pub fn validate_root(root: &Root) -> Result<(), Error> {
    root.validate().map_err(|source| Error::Format {
        phase: Phase::Preflight,
        uncertain: false,
        source,
    })?;
    if root.generation() != 1
        || root.bank() != 0
        || !matches!(
            root.geometry().role,
            Role::SingleNodeHome | Role::CheckpointMetadata | Role::CacheScratch
        )
    {
        return Err(invalid(
            "explicit initial home/metadata/cache root required",
        ));
    }
    if root.geometry().alignment as usize > MAX_ALIGNMENT {
        return Err(invalid("provisioning alignment exceeds fixed scratch"));
    }
    moruna_storage_format::journal::Journal::new(root.geometry().clone()).map_err(|source| {
        Error::Format {
            phase: Phase::Preflight,
            uncertain: false,
            source,
        }
    })?;
    Ok(())
}
/// Provision an EXISTING exclusively locked owned regular disk image. Never creates,
/// truncates, resizes, formats on mount failure, returns a store, or claims raw IO.
pub fn image(path: &Path, root: &Root, layout: Option<&BootConfig>) -> Result<Report, Error> {
    validate_root(root)?;
    if !path.is_absolute() {
        return Err(invalid("absolute explicit image path required"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|source| io(Phase::Preflight, false, source))?;
    file.try_lock()
        .map_err(|source| io(Phase::Preflight, false, std::io::Error::other(source)))?;
    let metadata = file
        .metadata()
        .map_err(|source| io(Phase::Preflight, false, source))?;
    if !metadata.is_file() {
        return Err(invalid("image mode requires a regular file"));
    }
    initialize(
        &file,
        metadata.len(),
        root.geometry().alignment as usize,
        root.geometry().role,
        root,
        layout,
    )?;
    Ok(Report {
        root: root.clone(),
        raw_device: false,
    })
}
/// Provision a Linux raw target through the existing strict direct/exclusive boundary.
/// No raw cache object facade is returned; keys are required later by encrypted runtime.
#[cfg(target_os = "linux")]
pub fn raw(path: &Path, root: &Root, layout: Option<&BootConfig>) -> Result<Report, Error> {
    validate_root(root)?;
    let device = crate::raw::Device::open(path, root.geometry().role, crate::raw::PathMode::Direct)
        .map_err(|source| Error::Raw(Box::new(source)))?;
    device.provision(root, layout)?;
    Ok(Report {
        root: root.clone(),
        raw_device: true,
    })
}
fn position(base: u64, relative: u64) -> Result<u64, Error> {
    base.checked_add(relative)
        .ok_or_else(|| invalid("region offset overflow"))
}
fn region(
    layout: Option<&BootConfig>,
    root: &Root,
    capacity: u64,
    alignment: usize,
) -> Result<Region, Error> {
    let Some(layout) = layout else {
        return Ok(Region {
            offset: 0,
            bytes: capacity,
        });
    };
    let alignment = u32::try_from(alignment).map_err(|_| invalid("alignment width"))?;
    match &layout.provider {
        Provider::Local {
            alignment: configured,
            metadata,
            cache,
            home,
            ..
        } => {
            if *configured != alignment {
                return Err(invalid("layout differs from actual selected alignment"));
            }
            let mut regions = vec![*metadata, *cache];
            if let Some(home) = home {
                regions.push(*home);
            }
            validate_regions(&regions, alignment, capacity).map_err(|source| Error::Format {
                phase: Phase::Preflight,
                uncertain: false,
                source,
            })?;
            match root.geometry().role {
                Role::CheckpointMetadata => Ok(*metadata),
                Role::CacheScratch => Ok(*cache),
                Role::SingleNodeHome => {
                    home.ok_or_else(|| invalid("layout has no explicit local home region"))
                }
                Role::FormatProof => Err(invalid("format-proof cannot select a production region")),
            }
        }
        Provider::Raw {
            alignment: configured,
            role,
            ..
        } if *configured == alignment && *role == root.geometry().role => Ok(Region {
            offset: 0,
            bytes: capacity,
        }),
        _ => Err(invalid(
            "layout does not designate this local role/alignment",
        )),
    }
}
/// Sole host formatter IO sequence, shared by regular-image and already-validated
/// strict raw Device. Native adapters consume the same Root/Journal authority.
pub(crate) fn initialize(
    file: &File,
    capacity: u64,
    alignment: usize,
    role: Role,
    root: &Root,
    layout: Option<&BootConfig>,
) -> Result<(), Error> {
    validate_root(root)?;
    let region = region(layout, root, capacity, alignment)?;
    if root.physical_capacity() != region.bytes
        || root.geometry().alignment as usize != alignment
        || root.geometry().role != role
    {
        return Err(invalid(
            "root artifact differs from actual capacity/alignment/selected role",
        ));
    }
    let encoded = root.encode().map_err(|source| Error::Format {
        phase: Phase::Preflight,
        uncertain: false,
        source,
    })?;
    let mut scratch = Scratch([0; MAX_ALIGNMENT]);
    for at in [0, alignment as u64] {
        file.read_exact_at(&mut scratch.0[..alignment], position(region.offset, at)?)
            .map_err(|source| io(Phase::ReadRoots, false, source))?;
        if scratch.0[..alignment].iter().any(|byte| *byte != 0) {
            return Err(Error::Invalid {
                phase: Phase::ReadRoots,
                reason: "existing/nonzero root refuses provisioning",
            });
        }
    }
    scratch.0[..alignment].fill(0);
    for at in (root.geometry().log_offset..root.geometry().data_offset).step_by(alignment) {
        file.write_all_at(&scratch.0[..alignment], position(region.offset, at)?)
            .map_err(|source| io(Phase::ClearJournal, true, source))?;
    }
    file.sync_all()
        .map_err(|source| io(Phase::SyncJournal, true, source))?;
    scratch.0[..encoded.len()].copy_from_slice(&encoded);
    file.write_all_at(
        &scratch.0[..alignment],
        position(region.offset, root.slot())?,
    )
    .map_err(|source| io(Phase::WriteRoot, true, source))?;
    file.sync_all()
        .map_err(|source| io(Phase::SyncRoot, true, source))?;
    file.read_exact_at(
        &mut scratch.0[..alignment],
        position(region.offset, root.slot())?,
    )
    .map_err(|source| io(Phase::Readback, true, source))?;
    let observed = Root::decode(&scratch.0[..alignment]).map_err(|source| Error::Format {
        phase: Phase::Readback,
        uncertain: true,
        source,
    })?;
    if &observed != root {
        return Err(Error::Invalid {
            phase: Phase::Readback,
            reason: "synced root readback differs; target may have changed",
        });
    }
    Ok(())
}
