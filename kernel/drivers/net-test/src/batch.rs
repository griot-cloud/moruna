//! The fixture's frame count is bounded by both the protocol and its actual wire size.

/// A vector count and each frame length are encoded as u32; the service frame adds
/// its own header. None means even one frame cannot fit without overflow.
pub(super) fn frames_per_send(frame_bytes: usize, frame_limit: usize) -> Option<usize> {
    let payload = moruna_svc::limits::CHANNEL_MAX_BYTES
        .checked_sub(moruna_svc::frame::HEADER + size_of::<u32>())?;
    let one = frame_bytes.checked_add(size_of::<u32>())?;
    let count = (payload / one).min(frame_limit);
    (count > 0).then_some(count)
}
