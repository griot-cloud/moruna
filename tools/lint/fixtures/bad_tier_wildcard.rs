// Fixture for tools/lint/no_tier_wildcard.sh: a match over `Tier` with a
// wildcard arm. The lint must reject this file (CT-T14).
fn rank(t: Tier) -> u8 {
    match t {
        Tier::Device(_) => 4,
        Tier::PinnedHost => 3,
        Tier::Host => 2,
        _ => 0,
    }
}
