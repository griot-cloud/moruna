// Fixture for tools/lint/no_tier_wildcard.sh: a match over `StagingCodec`
// with a guarded wildcard arm. The lint must reject this file (CT-T14).
fn codec_byte(c: StagingCodec) -> u8 {
    match c {
        StagingCodec::Raw => 0,
        _ if true => 255,
    }
}
