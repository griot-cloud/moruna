//! Path selection (06 e.2) and the two kinds of fallback (06 f.9).
//!
//! Every path is chosen once, in `Reactor::new`, from the host profile and the allocator, and
//! no operation changes it afterwards (RE-I3). What a failure on a chosen path means is
//! decided here too: a `Present` field is a guarantee, so a failure on it is an error (G-I7),
//! while a `Probed(true)` field is only an availability, so a failure on it falls back once.

use moruna_kernel::{Guarantee, HostProfile, IoPaths};

/// The io path names used as the second half of the once-per-run warn key (e.3, f.9) and in
/// the `Config` messages of RE-I3.
pub(crate) const P_URING: &str = "io_uring";
pub(crate) const P_DIRECT: &str = "direct_io";
pub(crate) const P_PINNED: &str = "pinned";
pub(crate) const P_OBJECT: &str = "object_store";

/// True when this build can drive io_uring at all: the crate feature and Linux.
pub(crate) const URING_BUILT: bool = cfg!(all(feature = "uring", target_os = "linux"));
/// True when this build can drive the CUDA copy engine.
pub(crate) const CUDA_BUILT: bool = cfg!(feature = "cuda");
/// True when this build can drive cuFile.
pub(crate) const GDS_BUILT: bool = cfg!(feature = "gds");
/// Always false in a v1 build: `Remote` endpoints are `Unsupported("rdma")`, never emulated
/// over TCP (e.2, E11).
pub(crate) const RDMA_BUILT: bool = cfg!(feature = "rdma");

/// One selected path and whether a failure on it is an error or a fallback.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selected {
    /// The path is in use.
    pub(crate) on: bool,
    /// The host declared it `Present`, so a failure is a platform bug (G-I7, RE-I3).
    pub(crate) guaranteed: bool,
}

impl Selected {
    fn from(g: Guarantee, built: bool) -> Selected {
        Selected {
            on: g.is_available() && built,
            guaranteed: g.is_guaranteed(),
        }
    }

    /// True when an operation on this path may retry once through its fallback (f.9).
    pub(crate) fn may_fall_back(&self) -> bool {
        self.on && !self.guaranteed
    }
}

/// The five rows of e.2, decided once at `new`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Paths {
    pub(crate) uring: Selected,
    pub(crate) direct: Selected,
    pub(crate) pinned: Selected,
    pub(crate) gds: Selected,
    pub(crate) rdma: Selected,
    /// `IoPaths::pinned` is `alloc.is_pinned()` whatever the build (e.2), which is not the
    /// same question as whether the pinned copy path is usable, so it is kept apart.
    pub(crate) arena_pinned: bool,
}

impl Paths {
    /// Select every path from the profile and the allocator (e.2). `memlock` is the profile's
    /// word for the pinned row, and `alloc.is_pinned()` is the fact; a run whose arena is not
    /// page-locked stages device copies through the bounce buffer whatever the profile said.
    pub(crate) fn select(profile: &HostProfile, arena_pinned: bool) -> Paths {
        Paths {
            uring: Selected::from(profile.io_uring, URING_BUILT),
            direct: Selected::from(profile.direct_io_staging, true),
            pinned: Selected {
                on: arena_pinned && CUDA_BUILT,
                guaranteed: profile.memlock.is_guaranteed(),
            },
            gds: Selected::from(profile.gds, GDS_BUILT),
            rdma: Selected {
                on: profile.rdma.is_available() && RDMA_BUILT,
                guaranteed: profile.rdma.is_guaranteed(),
            },
            arena_pinned,
        }
    }

    /// What the run report shows (`IoPaths`, contracts d.9).
    pub(crate) fn to_io_paths(self) -> IoPaths {
        IoPaths {
            direct_io: self.direct.on,
            io_uring: self.uring.on,
            gds: self.gds.on,
            pinned: self.arena_pinned,
            rdma: self.rdma.on,
        }
    }

    /// Turn a path off after its driver refused to start (RE-T15): legal only when the path
    /// was not guaranteed, in which case the caller returns `Config` instead.
    pub(crate) fn disable_uring(&mut self) {
        self.uring.on = false;
    }

    /// As `disable_uring`, for the cuFile driver.
    pub(crate) fn disable_gds(&mut self) {
        self.gds.on = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(g: Guarantee) -> HostProfile {
        HostProfile {
            io_uring: g,
            direct_io_staging: g,
            gds: g,
            memlock: g,
            ..HostProfile::default()
        }
    }

    #[test]
    fn availability_decides_selection_and_guarantee_decides_fallback() {
        let p = Paths::select(&profile(Guarantee::Probed(true)), true);
        assert!(p.direct.on);
        assert!(p.direct.may_fall_back());
        let q = Paths::select(&profile(Guarantee::Present), true);
        assert!(q.direct.on);
        assert!(!q.direct.may_fall_back());
        let r = Paths::select(&profile(Guarantee::Probed(false)), true);
        assert!(!r.direct.on);
        assert!(!r.direct.may_fall_back());
        let s = Paths::select(&profile(Guarantee::Absent), true);
        assert!(!s.direct.on);
    }

    #[test]
    fn rdma_is_off_in_every_v1_build_and_pinned_follows_the_arena() {
        let p = Paths::select(&profile(Guarantee::Present), true);
        assert!(!p.rdma.on, "rdma is a post-v1 feature (e.2, E11)");
        assert!(
            p.to_io_paths().pinned,
            "IoPaths::pinned is alloc.is_pinned()"
        );
        let q = Paths::select(&profile(Guarantee::Present), false);
        assert!(!q.to_io_paths().pinned);
        assert!(!q.pinned.on);
    }

    #[test]
    fn a_path_this_build_cannot_drive_is_not_selected() {
        let p = Paths::select(&profile(Guarantee::Present), true);
        assert_eq!(p.uring.on, URING_BUILT);
        assert_eq!(p.gds.on, GDS_BUILT);
        assert_eq!(p.pinned.on, CUDA_BUILT);
    }

    #[test]
    fn a_driver_that_refuses_to_start_turns_its_path_off() {
        let mut p = Paths::select(&profile(Guarantee::Probed(true)), true);
        p.disable_uring();
        p.disable_gds();
        assert!(!p.uring.on);
        assert!(!p.gds.on);
        assert!(!p.to_io_paths().io_uring);
        assert!(!p.to_io_paths().gds);
    }
}
