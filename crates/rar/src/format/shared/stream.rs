//! Stream access shared by every format path.

/// Borrow the archive's underlying stream, or report the internal invariant
/// violation as `InvalidState` instead of panicking.
pub(crate) fn stream_mut(
    stream: &mut Option<Box<dyn crate::archive::ArchiveStream>>,
) -> crate::error::RarResult<&mut Box<dyn crate::archive::ArchiveStream>> {
    stream.as_mut().ok_or_else(|| {
        crate::error::RarError::InvalidState("archive has no underlying stream".into())
    })
}
