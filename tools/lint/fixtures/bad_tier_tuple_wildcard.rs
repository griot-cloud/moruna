// Fixture for tools/lint/no_tier_wildcard.sh: a move table keyed by a pair of
// tiers, where the wildcard would silently cover every Remote row. The lint
// must reject this file (CT-T14). The block body of the first arm also
// contains a `_ =>` inside a nested, unrelated match, which alone would be fine.
fn legal(from: Tier, to: Tier) -> Result<(), AmoruError> {
    match (from, to) {
        (Tier::Host, Tier::Disk(_)) => {
            match 1u8 {
                1 => Ok(()),
                _ => Ok(()),
            }
        }
        _ => Err(AmoruError::Staging("illegal".into())),
    }
}
