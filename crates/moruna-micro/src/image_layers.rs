//! The shipping personality layer-query path, independent of startup-page access.
//! Native builds use the real SDK channel; protocol tests use its existing host
//! model to exercise this exact handshake, request and seal validation function.
use linux_protocol::linux;

pub(super) fn query(
    listener: &moruna_svc::Channel,
    deadline: u64,
) -> Result<Vec<[u8; 32]>, String> {
    let channel = moruna_svc::connect(listener)
        .map_err(|e| format!("connecting to the personality for its layers: {e:?}"))?;
    let mut client = linux::Client::connect(channel, deadline)
        .map_err(|e| format!("hello to the personality for its layers: {e:?}"))?;
    // The canonical moruna.linux 3.0 handshake guarantees ordinal 4 in the
    // baseline. The obsolete 1.3 minor gate would reject a valid 3.0 peer here.
    let seals = client
        .layers()
        .map_err(|e| format!("the personality's layers: {e:?}"))?;
    seals
        .into_iter()
        .enumerate()
        .map(|(i, seal)| {
            <[u8; 32]>::try_from(seal.0.as_slice()).map_err(|_| {
                format!(
                    "the personality says layer {i}'s seal is {} bytes, not 32",
                    seal.0.len()
                )
            })
        })
        .collect()
}
