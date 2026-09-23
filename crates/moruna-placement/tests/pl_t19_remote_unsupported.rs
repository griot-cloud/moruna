//! PL-T19 remote_unsupported (CT-I11): every reserved multi-node path returns
//! `Unsupported("rdma")` in a build without the feature, an unknown codec byte is
//! `Unsupported("codec")`, and no `match` on `Tier` or `StagingCodec` in this crate has a
//! wildcard arm.

mod common;

use moruna_kernel::{
    DeviceId, LOCAL_NODE, Locality, Morsel, MorunaError, NodeId, Payload, Placement, RemoteRef,
    SegmentRef, StagingCodec, Tier, TierKind, TierPref,
};
use moruna_placement::plan::one_tier_down;
use moruna_placement::staging::segment::RecordHeader;
use moruna_placement::state::{State, satisfies};
use moruna_testkit::{FakeAllocator, FakeReactor};

const REMOTE: RemoteRef = RemoteRef {
    addr: 1,
    rkey: 2,
    len: 3,
};

#[test]
fn pl_t19_a_remote_payload_cannot_be_pushed() {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    let engine = common::engine(cfg, &alloc, &reactor);
    // E9 exempts test code: the tier tag is a lie the engine must refuse, not believe.
    let batch = common::int_batch(&alloc, 8, alloc.host_tier());
    let payload = unsafe { Payload::table_in(batch, Tier::Remote(NodeId(1), REMOTE)) };
    let morsel = Morsel::new(0, 0, payload, common::origin(0));
    match engine.push(0, morsel) {
        Err(MorunaError::Unsupported("rdma")) => {}
        other => panic!("expected Unsupported(rdma), got {other:?}"),
    }
}

#[test]
fn pl_t19_a_remote_head_cannot_be_popped() {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    let engine = common::engine(cfg, &alloc, &reactor);
    engine
        .push(0, common::table_morsel(&alloc, 0, 0, 8))
        .expect("push");
    // The reserved state is never constructed by a v1 path; a test builds one to prove the
    // arm that meets it refuses rather than guesses.
    let state = State::OnRemote(NodeId(1), REMOTE);
    assert_eq!(state.index(), 7, "the histogram slot j reserves for it");
    assert!(state.resident_tier().is_none());
    assert!(!state.in_flight());
    assert!(!satisfies(
        Tier::Remote(NodeId(1), REMOTE),
        &moruna_kernel::PayloadSpec {
            kind: moruna_kernel::PayloadKind::Either,
            tier: TierPref::Any,
        }
    ));
    assert_eq!(
        one_tier_down(Tier::Remote(NodeId(1), REMOTE), Tier::Host),
        None
    );
    assert_eq!(
        one_tier_down(
            Tier::Disk(SegmentRef {
                segment: 0,
                offset: 0,
                len: 0
            }),
            Tier::Host
        ),
        None
    );
    assert_eq!(
        one_tier_down(Tier::Device(DeviceId(0)), Tier::PinnedHost),
        Some(TierKind::PinnedHost)
    );
    assert_eq!(one_tier_down(Tier::Host, Tier::Host), Some(TierKind::Disk));
    // And the queue itself still behaves: a v1 build never meets the variant.
    assert!(
        engine
            .pop(0, common::want_host(), Locality::Any)
            .expect("pop")
            .is_some()
    );
    assert_eq!(LOCAL_NODE, NodeId(0));
}

#[test]
fn pl_t19_an_unknown_codec_byte_is_refused() {
    let mut page = vec![0u8; common::PAGE];
    RecordHeader {
        seq: 1,
        stage: 0,
        kind: moruna_kernel::PayloadKind::Table,
        codec: StagingCodec::Raw,
        payload_len: 16,
        body_offset: 4096,
    }
    .write(&mut page)
    .expect("write");
    assert!(RecordHeader::read(&page).is_ok(), "a Raw record reads");
    page[19] = 1;
    match RecordHeader::read(&page) {
        Err(MorunaError::Unsupported("codec")) => {}
        other => panic!("expected Unsupported(codec), got {other:?}"),
    }
    page[19] = 0;
    page[18] = 9;
    assert!(
        RecordHeader::read(&page).is_err(),
        "an unknown kind is refused"
    );
    page[0] = 0;
    assert!(RecordHeader::read(&page).is_err(), "bad magic is refused");
    assert!(
        RecordHeader::read(&page[..8]).is_err(),
        "a short page is refused"
    );
}

#[test]
fn pl_t19_the_tier_lint_passes_on_this_crate() {
    // CT-T14's lint is the repository's; this asserts it over this crate's sources, so a
    // wildcard arm cannot be introduced here without the test failing (CT-I11).
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root
        .join("../../tools/lint/no_tier_wildcard.sh")
        .canonicalize()
        .expect("the lint script");
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![root.join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    assert!(!files.is_empty(), "the crate has sources to scan");
    let out = std::process::Command::new(&script)
        .args(&files)
        .output()
        .expect("run the lint");
    assert!(
        out.status.success(),
        "no_tier_wildcard: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
