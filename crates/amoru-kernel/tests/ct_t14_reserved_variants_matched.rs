//! CT-T14 reserved_variants_matched: a `match` on `Tier` with five named arms, and a `match` on
//! `StagingCodec` with one named arm, compile with no wildcard; `Tier::Remote(..).is_resident()`
//! is false; `rank` and `index` return the f.6 values; `LOCAL_NODE == NodeId::default()`.
//! Proves CT-I11. The repository lint `tools/lint/no_tier_wildcard.sh` is the other half and
//! runs in CI and in the quality gate.

use amoru_kernel::{
    AmoruError, DeviceId, LOCAL_NODE, NodeId, RemoteRef, SegmentRef, StagingCodec, TIER_COUNT,
    Tier, TierKind,
};

/// The exhaustiveness assertion for `Tier`: five named arms, no wildcard, with `Remote`
/// answering `Unsupported("rdma")` as every v1 component must (CT-I11).
// `AmoruError` carries a morsel's features (CT-I10), so it is large beside this helper's
// `&'static str`; the shape of the error type is the contract's (d.14), not this test's.
#[allow(clippy::result_large_err)]
fn describe(tier: Tier) -> Result<&'static str, AmoruError> {
    match tier {
        Tier::Device(_) => Ok("device"),
        Tier::PinnedHost => Ok("pinned host"),
        Tier::Host => Ok("host"),
        Tier::Disk(_) => Ok("disk"),
        Tier::Remote(_, _) => Err(AmoruError::Unsupported("rdma")),
    }
}

/// The exhaustiveness assertion for `StagingCodec`: one named arm, no wildcard.
fn codec_byte(codec: StagingCodec) -> u8 {
    match codec {
        StagingCodec::Raw => 0,
    }
}

/// The same for `TierKind`, which keys every per-tier array.
fn kind_name(kind: TierKind) -> &'static str {
    match kind {
        TierKind::Device => "device",
        TierKind::PinnedHost => "pinned host",
        TierKind::Host => "host",
        TierKind::Disk => "disk",
        TierKind::Remote => "remote",
    }
}

#[test]
fn ct_t14_reserved_variants_matched() {
    let segment = SegmentRef {
        segment: 3,
        offset: 8192,
        len: 64,
    };
    let remote = RemoteRef {
        addr: 0xdead_beef,
        rkey: 7,
        len: 128,
    };
    let tiers = [
        Tier::Device(DeviceId(0)),
        Tier::PinnedHost,
        Tier::Host,
        Tier::Disk(segment),
        Tier::Remote(NodeId(1), remote),
    ];

    // Residency (d.2).
    assert!(describe(tiers[0]).is_ok() && describe(tiers[4]).is_err());
    assert!(matches!(
        describe(tiers[4]),
        Err(AmoruError::Unsupported("rdma"))
    ));
    assert!(tiers[0].is_resident() && tiers[1].is_resident() && tiers[2].is_resident());
    assert!(!tiers[3].is_resident());
    assert!(
        !tiers[4].is_resident(),
        "Tier::Remote is never resident in v1"
    );

    // Rank is the promotion order: Device 4, PinnedHost 3, Host 2, Remote 1, Disk 0 (f.6).
    assert_eq!(tiers.map(|t| t.rank()), [4, 3, 2, 0, 1]);
    // Index is the schema order and may not change: Device 0, PinnedHost 1, Host 2, Disk 3,
    // Remote 4 (f.6, CT-I8).
    assert_eq!(tiers.map(|t| t.index()), [0, 1, 2, 3, 4]);
    // `TierKind::index` equals `Tier::index` for the corresponding tier (d.2).
    for tier in tiers {
        assert_eq!(tier.kind().index(), tier.index());
        assert!(!kind_name(tier.kind()).is_empty());
    }
    assert_eq!(TIER_COUNT, tiers.len());

    // The reserved staging codec.
    assert_eq!(codec_byte(StagingCodec::Raw), 0);
    assert_eq!(StagingCodec::default(), StagingCodec::Raw);
    assert_eq!(StagingCodec::Raw.code(), codec_byte(StagingCodec::Raw));
    assert_eq!(
        StagingCodec::from_code(1),
        None,
        "an unknown codec is not silently accepted"
    );

    // The reserved node identity.
    assert_eq!(LOCAL_NODE, NodeId::default());
    assert_eq!(LOCAL_NODE, NodeId(0));
    assert_ne!(LOCAL_NODE, NodeId(1));
}
