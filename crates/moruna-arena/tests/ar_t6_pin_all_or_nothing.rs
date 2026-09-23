//! AR-T6 pin_all_or_nothing (02 k): with memlock forbidden (an rlimit set by the test) and
//! `memlock = Probed(true)`, `PinnedHost` requests fail with `Alloc { tier: PinnedHost }`,
//! `Host` requests succeed and `is_pinned()` is false; with it allowed and
//! `host_tier = PinnedHost`, `PinnedHost` requests succeed, `Host` requests fail with
//! `Alloc { tier: Host }` and `is_pinned()` is true; with memlock forbidden and
//! `memlock = Present`, `new` returns `Config`. AR-I6, G-I7.
//!
//! A test binary of its own, and one `#[test]` function, because the rlimit it sets is
//! process wide.

use moruna_arena::{Arena, ArenaConfig};
use moruna_kernel::{Allocator, MorunaError, Guarantee, TierKind};

fn cfg(host_tier: TierKind, memlock: Guarantee) -> ArenaConfig {
    ArenaConfig {
        host_bytes: 1 << 20,
        host_tier,
        device_bytes: Vec::new(),
        page_bytes: 4096,
        huge_pages: Guarantee::Probed(false),
        memlock,
        register_rdma: false,
    }
}

/// Set `RLIMIT_MEMLOCK` to `bytes` and return what it was.
fn set_memlock_limit(bytes: u64) -> libc::rlimit {
    // SAFETY: test-only; `rlimit` is a plain struct the call fills in.
    let mut previous: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: test-only; `previous` is a live local.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut previous) },
        0
    );
    let wanted = libc::rlimit {
        rlim_cur: bytes as libc::rlim_t,
        rlim_max: previous.rlim_max,
    };
    // SAFETY: test-only; lowering a soft limit the process owns.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &wanted) }, 0);
    previous
}

/// Raise the soft `RLIMIT_MEMLOCK` to the hard limit and return the original.
fn raise_memlock_limit() -> libc::rlimit {
    // SAFETY: test-only; `rlimit` is a plain struct the call fills in.
    let mut previous: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: test-only; `previous` is a live local.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut previous) },
        0
    );
    let wanted = libc::rlimit {
        rlim_cur: previous.rlim_max,
        rlim_max: previous.rlim_max,
    };
    // SAFETY: test-only; raising a soft limit to the hard limit the process already has.
    unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &wanted) };
    previous
}

fn restore(previous: libc::rlimit) {
    // SAFETY: test-only; restoring the limit this process started with.
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &previous) },
        0
    );
}

#[test]
fn ar_t6_pin_all_or_nothing() {
    // The region is 1 MiB and the soft limit is raised to the hard one first, so "memlock
    // allowed" really is allowed on a host whose default soft limit is small.
    let original = raise_memlock_limit();

    // 1. memlock allowed, host_tier PinnedHost: the whole region is locked, so PinnedHost
    //    is the run's one host tier and Host is refused.
    let arena = Arena::new(cfg(TierKind::PinnedHost, Guarantee::Probed(true)))
        .expect("a pinned arena where memlock is permitted");
    assert!(arena.is_pinned());
    let pinned = arena
        .alloc(4096, moruna_kernel::Tier::PinnedHost)
        .expect("the run's host tier");
    assert_eq!(pinned.tier(), moruna_kernel::Tier::PinnedHost);
    assert_eq!(arena.stats().pinned_in_use, 64 * 1024);
    assert_eq!(arena.stats().host_in_use, 0);
    let e = arena
        .alloc(4096, moruna_kernel::Tier::Host)
        .expect_err("the other host tier does not exist in this run");
    assert!(matches!(e, MorunaError::Alloc { tier, .. } if tier == moruna_kernel::Tier::Host));
    assert!(arena.arena_stats().pinned);
    drop(pinned);
    drop(arena);

    let previous = set_memlock_limit(0);

    // 2. memlock forbidden and probed: `new` falls back to Host before any buffer exists.
    let arena = Arena::new(cfg(TierKind::PinnedHost, Guarantee::Probed(true)))
        .expect("a probed guarantee falls back rather than failing");
    assert!(!arena.is_pinned());
    let host = arena
        .alloc(4096, moruna_kernel::Tier::Host)
        .expect("the run's host tier after the fallback");
    assert_eq!(host.tier(), moruna_kernel::Tier::Host);
    let e = arena
        .alloc(4096, moruna_kernel::Tier::PinnedHost)
        .expect_err("nothing is pinned in this run");
    assert!(matches!(e, MorunaError::Alloc { tier, .. } if tier == moruna_kernel::Tier::PinnedHost));
    assert!(!arena.arena_stats().pinned);
    drop(host);
    drop(arena);

    // 3. memlock forbidden but declared Present: a platform bug, not a fallback (G-I7).
    let e = Arena::new(cfg(TierKind::PinnedHost, Guarantee::Present))
        .expect_err("a declared guarantee that does not hold");
    assert!(matches!(e, MorunaError::Config { name, .. } if name == "host_profile"));

    restore(previous);
    restore(original);
}
