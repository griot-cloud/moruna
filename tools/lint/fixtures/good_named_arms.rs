// Fixture for tools/lint/no_tier_wildcard.sh: every match over `Tier` or
// `StagingCodec` names every variant, `Remote` returns `Unsupported("rdma")`,
// and the wildcards that do appear are on unrelated matches or inside strings
// and comments. The lint must accept this file (CT-T14).
fn is_resident(t: &Tier) -> bool {
    match t {
        Tier::Device(_) | Tier::PinnedHost | Tier::Host => true,
        Tier::Disk(_) => false,
        Tier::Remote(_, _) => false,
    }
}

fn read(t: Tier) -> Result<u8, MorunaError> {
    match t {
        Tier::Device(d) => Ok(d.0),
        Tier::PinnedHost => Ok(1),
        Tier::Host => Ok(2),
        Tier::Disk(seg) => {
            // a wildcard in a nested match on a plain integer is not a Tier arm
            match seg.segment {
                0 => Ok(3),
                _ => Ok(4),
            }
        }
        Tier::Remote(node, _) if node == LOCAL_NODE => Err(MorunaError::Unsupported("rdma")),
        Tier::Remote(_, _) => Err(MorunaError::Unsupported("rdma")),
    }
}

fn codec_byte(c: StagingCodec) -> u8 {
    match c {
        StagingCodec::Raw => 0,
    }
}

fn unrelated(n: u32) -> &'static str {
    // "Tier::Remote(_, _) => _" in a comment, and a string below: neither counts
    let label = "match tier { Tier::Host => 1, _ => 0 }";
    let ch = '{';
    let _ = (label, ch);
    match n {
        0 => "zero",
        _ => "many",
    }
}
