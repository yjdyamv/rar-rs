//! Stream access shared by every format path.

use std::io::{Read, Seek, SeekFrom};

/// Borrow the archive's underlying stream, or report the internal invariant
/// violation as `InvalidState` instead of panicking.
pub(crate) fn stream_mut(
    stream: &mut Option<Box<dyn crate::archive::ArchiveStream>>,
) -> crate::error::RarResult<&mut Box<dyn crate::archive::ArchiveStream>> {
    stream.as_mut().ok_or_else(|| {
        crate::error::RarError::InvalidState("archive has no underlying stream".into())
    })
}

/// Length of a seekable reader, restoring its position. Works for concrete
/// files and for the boxed archive stream alike.
pub(crate) fn stream_len(reader: &mut (impl Read + Seek)) -> crate::error::RarResult<u64> {
    let position = reader.stream_position()?;
    let length = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(position))?;
    Ok(length)
}

/// Seek `reader` past a block's declared data area, but only when that area
/// lies inside the file. A malformed vint size can point beyond the
/// filesystem's maximum offset, where Linux `lseek` returns `EINVAL`
/// instead of letting the following read hit EOF (Windows tolerates the
/// seek); `Ok(false)` means the area is out of bounds and the caller stops
/// scanning, which is exactly the state a seek-past-EOF plus EOF read
/// produces on the other platforms.
pub(crate) fn seek_past_data_area(
    reader: &mut (impl Read + Seek),
    data_end: u64,
    file_len: u64,
) -> crate::error::RarResult<bool> {
    if data_end > file_len {
        return Ok(false);
    }
    reader.seek(SeekFrom::Start(data_end))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_len_restores_the_position() {
        let mut cursor = std::io::Cursor::new(vec![0u8; 32]);
        cursor.set_position(7);
        assert_eq!(stream_len(&mut cursor).unwrap(), 32);
        assert_eq!(cursor.position(), 7);
    }

    #[test]
    fn seek_past_data_area_bounds_the_offset() {
        let mut cursor = std::io::Cursor::new(vec![0u8; 32]);
        assert!(seek_past_data_area(&mut cursor, 32, 32).unwrap());
        assert_eq!(cursor.position(), 32);

        cursor.set_position(3);
        assert!(!seek_past_data_area(&mut cursor, 33, 32).unwrap());
        assert_eq!(cursor.position(), 3, "an out-of-bounds area must not seek");
        assert!(!seek_past_data_area(&mut cursor, u64::MAX, 32).unwrap());
    }
}
