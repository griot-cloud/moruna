//! Tier to tier copies (06 f.5), whose legal transitions are contracts e.1.
//!
//! Every pair is decided before anything is submitted, so an illegal or unsupported copy is an
//! error the caller sees at once rather than a failed operation later. The mechanisms are the
//! device's copy engine for the two host to device rows, cuFile for the `Disk -> Device` row
//! when the GDS path is selected, and nothing else: disk and the host tier are joined by
//! `read_file` and `write_file`, not by `copy`, and every `Remote` endpoint is
//! `Unsupported("rdma")` in a v1 build (E11).
//!
//! `unsafe` is permitted here (section l) for the CUDA driver calls; every block cites RE-I1,
//! which is what makes the pointers valid for the life of the operation.

use moruna_kernel::{MorunaError, Buffer, CopyDst, CopySrc, DeviceId, Result, SegmentRef, Tier};

use crate::stats::CopyDirection;

/// Where one end of a copy is, once `Remote` has been refused.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum End {
    /// The run's one host tier (`PinnedHost` when the arena is pinned, `Host` otherwise, e.1).
    Host,
    /// Accelerator memory.
    Device(DeviceId),
    /// A staging segment range.
    Disk(SegmentRef),
}

/// Reduce a tier to the end it is, refusing the reserved multi-node variant explicitly
/// (CT-I11: every arm is named).
pub(crate) fn classify(tier: Tier) -> Result<End> {
    match tier {
        Tier::PinnedHost | Tier::Host => Ok(End::Host),
        Tier::Device(d) => Ok(End::Device(d)),
        Tier::Disk(seg) => Ok(End::Disk(seg)),
        Tier::Remote(_, _) => Err(MorunaError::Unsupported("rdma")),
    }
}

/// What the reactor will do for one `copy`, decided before submission (f.5).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum Plan {
    /// Host tier to `Device`, by the copy engine; `bounce` when the arena is not pinned.
    HostToDevice { device: DeviceId, bounce: bool },
    /// `Device` to the host tier, by the copy engine.
    DeviceToHost { device: DeviceId, bounce: bool },
    /// A registered segment straight into device memory, by cuFile.
    GdsRead {
        segment: SegmentRef,
        device: DeviceId,
    },
}

impl Plan {
    pub(crate) fn direction(self) -> CopyDirection {
        match self {
            Plan::HostToDevice { .. } => CopyDirection::HostToDevice,
            Plan::DeviceToHost { .. } => CopyDirection::DeviceToHost,
            Plan::GdsRead { .. } => CopyDirection::Gds,
        }
    }
}

fn illegal(msg: &str) -> MorunaError {
    MorunaError::Io {
        op: "copy",
        target: "copy".into(),
        msg: msg.to_string(),
    }
}

/// Bytes each end covers, for the "destination shorter than source" check of f.5.
fn lengths(src: &CopySrc, dst: &CopyDst) -> (u64, u64) {
    let s = match src {
        CopySrc::View(v) => v.len() as u64,
        CopySrc::Disk(seg) => seg.len,
    };
    let d = match dst {
        CopyDst::Buffer(b) => b.len() as u64,
        CopyDst::Disk(seg) => seg.len,
    };
    (s, d)
}

/// Decide what one `copy` is, or why it is not a copy at all (f.5, contracts e.1).
///
/// `arena_pinned` is `Allocator::is_pinned()`: an unpinned host tier still reaches a device,
/// through the bounce buffer, which is a counted fallback and not a refusal (e.2).
pub(crate) fn plan(src: &CopySrc, dst: &CopyDst, gds_on: bool, arena_pinned: bool) -> Result<Plan> {
    let from = match src {
        CopySrc::View(v) => classify(v.tier())?,
        CopySrc::Disk(seg) => End::Disk(*seg),
    };
    let to = match dst {
        CopyDst::Buffer(b) => classify(b.tier())?,
        CopyDst::Disk(seg) => End::Disk(*seg),
    };
    let (src_len, dst_len) = lengths(src, dst);
    if dst_len < src_len {
        return Err(illegal(&format!(
            "destination is {dst_len} bytes for a {src_len} byte source"
        )));
    }
    match (from, to) {
        (End::Host, End::Device(device)) => Ok(Plan::HostToDevice {
            device,
            bounce: !arena_pinned,
        }),
        (End::Device(device), End::Host) => Ok(Plan::DeviceToHost {
            device,
            bounce: !arena_pinned,
        }),
        (End::Disk(segment), End::Device(device)) => {
            if gds_on {
                Ok(Plan::GdsRead { segment, device })
            } else {
                Err(illegal("disk endpoint without gds"))
            }
        }
        (End::Device(_), End::Disk(_)) => Err(illegal("gds write not used in v1")),
        (End::Host, End::Disk(_)) | (End::Disk(_), End::Host) => {
            Err(illegal("disk endpoint without gds"))
        }
        (End::Device(a), End::Device(b)) => Err(illegal(&format!(
            "device {} to device {} is not a v1 transition",
            a.0, b.0
        ))),
        (End::Host, End::Host) => Err(illegal("host tier to host tier is not a copy")),
        (End::Disk(_), End::Disk(_)) => Err(illegal("disk to disk is not a copy")),
    }
}

/// Run a planned copy. Called on a reactor thread, never on the caller's (f.8).
///
/// Without the `cuda` feature there is no copy engine in this build, so every row of f.5 that
/// needs one is an error naming the build rather than a silent success. The GDS row is the
/// same, through `gds::read`.
pub(crate) fn execute(
    plan: Plan,
    src: CopySrc,
    dst: CopyDst,
    segment_handle: Option<&crate::gds::Handle>,
) -> Result<Option<Buffer>> {
    match plan {
        Plan::GdsRead { segment, device: _ } => {
            let Some(handle) = segment_handle else {
                return Err(crate::segments::not_registered(segment.segment));
            };
            let CopyDst::Buffer(buffer) = dst else {
                return Err(illegal("a gds read needs a buffer destination"));
            };
            let Some(device_ptr) = buffer.device_ptr() else {
                return Err(illegal("a gds read needs a device destination"));
            };
            let n = crate::gds::read(handle, device_ptr, segment.len as usize, segment.offset)?;
            if n != segment.len as usize {
                return Err(illegal(&format!(
                    "gds read returned {n} of {} bytes",
                    segment.len
                )));
            }
            Ok(Some(buffer))
        }
        Plan::HostToDevice { .. } | Plan::DeviceToHost { .. } => device_copy(plan, src, dst),
    }
}

#[cfg(not(feature = "cuda"))]
fn device_copy(_plan: Plan, _src: CopySrc, _dst: CopyDst) -> Result<Option<Buffer>> {
    Err(illegal("this build has no copy engine (feature cuda)"))
}

#[cfg(feature = "cuda")]
fn device_copy(plan: Plan, src: CopySrc, dst: CopyDst) -> Result<Option<Buffer>> {
    use cudarc::driver::sys as cu;

    let (host_ptr, device_ptr, len, to_device, device) = match plan {
        Plan::HostToDevice { device, .. } => {
            let CopySrc::View(view) = &src else {
                return Err(illegal("a host to device copy needs a view source"));
            };
            let CopyDst::Buffer(buffer) = &dst else {
                return Err(illegal("a host to device copy needs a buffer destination"));
            };
            let (Some(h), Some(d)) = (view.host_ptr(), buffer.device_ptr()) else {
                return Err(illegal("endpoints are not a host view and a device buffer"));
            };
            (h as *mut u8, d, view.len(), true, device)
        }
        Plan::DeviceToHost { device, .. } => {
            let CopySrc::View(view) = &src else {
                return Err(illegal("a device to host copy needs a view source"));
            };
            let CopyDst::Buffer(buffer) = &dst else {
                return Err(illegal("a device to host copy needs a buffer destination"));
            };
            let (Some(d), Some(h)) = (view.device_ptr(), buffer.host_ptr()) else {
                return Err(illegal("endpoints are not a device view and a host buffer"));
            };
            (h, d, view.len(), false, device)
        }
        Plan::GdsRead { .. } => return Err(illegal("a gds read is not a device copy")),
    };
    let _ = device;
    // SAFETY: both pointers are valid for `len` bytes until this operation resolves, because
    // the reactor holds the source `BufferView` and the destination `Buffer` for its whole
    // life (RE-I1); the driver is initialised by the arena before any device buffer exists.
    let status = unsafe {
        if to_device {
            cu::cuMemcpyHtoD_v2(device_ptr, host_ptr.cast(), len)
        } else {
            cu::cuMemcpyDtoH_v2(host_ptr.cast(), device_ptr, len)
        }
    };
    if status != cu::CUresult::CUDA_SUCCESS {
        return Err(illegal(&format!("cuMemcpy failed: {status:?}")));
    }
    match dst {
        CopyDst::Buffer(buffer) => Ok(Some(buffer)),
        CopyDst::Disk(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_testkit::FakeAllocator;
    use std::sync::Arc;

    fn view(alloc: &FakeAllocator, tier: Tier, len: usize) -> CopySrc {
        let buffer = Arc::new(alloc.buffer(len, tier));
        CopySrc::View(buffer.view())
    }

    fn buffer(alloc: &FakeAllocator, tier: Tier, len: usize) -> CopyDst {
        CopyDst::Buffer(alloc.buffer(len, tier))
    }

    const SEG: SegmentRef = SegmentRef {
        segment: 3,
        offset: 0,
        len: 64,
    };

    #[test]
    fn the_two_device_rows_are_the_only_copies() {
        let alloc = FakeAllocator::new().pinned(true);
        let host = alloc.host_tier();
        let device = Tier::Device(DeviceId(0));
        let p = plan(
            &view(&alloc, host, 64),
            &buffer(&alloc, device, 64),
            false,
            true,
        )
        .expect("host to device");
        assert_eq!(
            p,
            Plan::HostToDevice {
                device: DeviceId(0),
                bounce: false
            }
        );
        assert_eq!(p.direction(), CopyDirection::HostToDevice);
        let q = plan(
            &view(&alloc, device, 64),
            &buffer(&alloc, host, 64),
            false,
            true,
        )
        .expect("device to host");
        assert_eq!(
            q,
            Plan::DeviceToHost {
                device: DeviceId(0),
                bounce: false
            }
        );
        assert_eq!(q.direction(), CopyDirection::DeviceToHost);
    }

    #[test]
    fn an_unpinned_arena_reaches_a_device_through_the_bounce_buffer() {
        let alloc = FakeAllocator::new().pinned(false);
        let p = plan(
            &view(&alloc, alloc.host_tier(), 64),
            &buffer(&alloc, Tier::Device(DeviceId(1)), 64),
            false,
            false,
        )
        .expect("plan");
        assert_eq!(
            p,
            Plan::HostToDevice {
                device: DeviceId(1),
                bounce: true
            }
        );
    }

    #[test]
    fn disk_is_reached_by_copy_only_through_gds() {
        let alloc = FakeAllocator::new();
        let device = Tier::Device(DeviceId(0));
        let p = plan(&CopySrc::Disk(SEG), &buffer(&alloc, device, 64), true, true).expect("gds");
        assert_eq!(
            p,
            Plan::GdsRead {
                segment: SEG,
                device: DeviceId(0)
            }
        );
        assert_eq!(p.direction(), CopyDirection::Gds);
        let err = plan(
            &CopySrc::Disk(SEG),
            &buffer(&alloc, device, 64),
            false,
            true,
        )
        .expect_err("no gds");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
        let err = plan(
            &CopySrc::Disk(SEG),
            &buffer(&alloc, alloc.host_tier(), 64),
            true,
            true,
        )
        .expect_err("disk to host is read_file, not copy");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
        let err = plan(
            &view(&alloc, alloc.host_tier(), 64),
            &CopyDst::Disk(SEG),
            true,
            true,
        )
        .expect_err("host to disk is write_file, not copy");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
        let err = plan(&view(&alloc, device, 64), &CopyDst::Disk(SEG), true, true)
            .expect_err("gds write is not used in v1");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
        let err =
            plan(&CopySrc::Disk(SEG), &CopyDst::Disk(SEG), true, true).expect_err("disk to disk");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
    }

    #[test]
    fn every_pair_contracts_e1_calls_illegal_fails_before_submission() {
        let alloc = FakeAllocator::new();
        let host = alloc.host_tier();
        let err = plan(
            &view(&alloc, host, 64),
            &buffer(&alloc, host, 64),
            false,
            true,
        )
        .expect_err("same tier");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
        let err = plan(
            &view(&alloc, Tier::Device(DeviceId(0)), 64),
            &buffer(&alloc, Tier::Device(DeviceId(1)), 64),
            false,
            true,
        )
        .expect_err("device to device");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
        let err = plan(
            &view(&alloc, host, 128),
            &buffer(&alloc, Tier::Device(DeviceId(0)), 64),
            false,
            true,
        )
        .expect_err("destination shorter than source");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
    }

    /// A buffer in a tier no allocator hands out, so the reserved `Remote` rows can be reached
    /// at all. Test code may use `unsafe` to construct a state a test needs (preamble E9).
    fn remote_buffer(len: usize, tier: Tier) -> Buffer {
        struct NoArena;
        impl moruna_kernel::ArenaHandle for NoArena {
            fn release(&self, ptr: *mut u8, len: usize, _tier: Tier) {
                // SAFETY: the pointer came from the boxed slice leaked below, with this length.
                drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
            }
        }
        let leaked = Box::leak(vec![0u8; len].into_boxed_slice());
        let ptr = leaked.as_mut_ptr();
        // SAFETY: the region is `len` bytes, owned by nothing else, and released exactly once
        // by `NoArena::release` when the buffer drops.
        unsafe { Buffer::from_raw(ptr, len, tier, Arc::new(NoArena)) }
    }

    #[test]
    fn a_remote_endpoint_is_unsupported_in_every_v1_build() {
        let alloc = FakeAllocator::new();
        let remote = Tier::Remote(
            moruna_kernel::LOCAL_NODE,
            moruna_kernel::RemoteRef {
                addr: 0,
                rkey: 0,
                len: 64,
            },
        );
        assert!(matches!(
            classify(remote),
            Err(MorunaError::Unsupported("rdma"))
        ));
        let remote_view = CopySrc::View(Arc::new(remote_buffer(64, remote)).view());
        let err = plan(
            &remote_view,
            &buffer(&alloc, alloc.host_tier(), 64),
            false,
            true,
        )
        .expect_err("remote source");
        assert!(matches!(err, MorunaError::Unsupported("rdma")));
        let err = plan(
            &view(&alloc, alloc.host_tier(), 64),
            &CopyDst::Buffer(remote_buffer(64, remote)),
            false,
            true,
        )
        .expect_err("remote destination");
        assert!(matches!(err, MorunaError::Unsupported("rdma")));
    }

    #[test]
    fn without_a_copy_engine_a_device_copy_says_so() {
        let alloc = FakeAllocator::new().pinned(true);
        let src = view(&alloc, alloc.host_tier(), 64);
        let dst = buffer(&alloc, Tier::Device(DeviceId(0)), 64);
        let p = plan(&src, &dst, false, true).expect("plan");
        let out = execute(p, src, dst, None);
        assert_eq!(out.is_err(), !cfg!(feature = "cuda"));
        let seg = CopySrc::Disk(SEG);
        let dst = buffer(&alloc, Tier::Device(DeviceId(0)), 64);
        let err = execute(
            Plan::GdsRead {
                segment: SEG,
                device: DeviceId(0),
            },
            seg,
            dst,
            None,
        )
        .expect_err("an unregistered segment");
        assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
    }

    #[test]
    fn a_view_over_a_device_buffer_reports_the_device_pointer() {
        let alloc = FakeAllocator::new();
        let buffer = Arc::new(alloc.buffer(64, Tier::Device(DeviceId(2))));
        let v = buffer.view();
        assert!(v.device_ptr().is_some());
        assert!(v.host_ptr().is_none());
        assert_eq!(
            classify(v.tier()).expect("device"),
            End::Device(DeviceId(2))
        );
        assert_eq!(alloc.in_use(Tier::Device(DeviceId(2))), 64);
    }
}
