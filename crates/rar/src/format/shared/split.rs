//! Cross-volume split-member merge, shared by the RAR 1.5–4.x and
//! RAR 1.3/1.4 scans.
//!
//! A member whose data spans volumes reappears as continuation file headers
//! (`SPLIT_BEFORE`) in later volumes; the fragments merge into one entry with
//! one chunk per volume segment, completed by the continuation fragment that
//! drops `SPLIT_AFTER`. The ordering guards and the completion fields live
//! here once so the two families cannot drift; each family maps
//! [`SplitMergeError`] to its own message.

use crate::archive::ArchiveEntry;

/// Why a fragment could not be merged.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SplitMergeError {
    /// A continuation fragment arrived with no member pending.
    ContinuationWithoutStart { fragment: String },
    /// A new split member started while one was still pending.
    Overlapping { pending: String },
    /// A regular (unsplit) member appeared while one was pending: completing
    /// the pending member later would append it to the catalog out of archive
    /// order (a later member would precede it).
    Interrupted { pending: String },
    /// The scan ended mid-member (its final volume is missing).
    MissingFinal { pending: String },
    /// The summed fragment sizes overflow `u64`.
    PackedSizeOverflow { pending: String },
}

/// Merge state for one volume-set scan.
#[derive(Default)]
pub(crate) struct SplitMerge {
    pending: Option<ArchiveEntry>,
}

impl SplitMerge {
    /// Feed one parsed fragment in scan order.
    ///
    /// `Ok(Some(entry))` is a completed member (a regular member, or the
    /// final fragment of a split member); a middle fragment returns
    /// `Ok(None)` and stays pending. The first fragment stays canonical
    /// (name, method, attributes); the final fragment supplies the
    /// whole-member CRC and unpacked size, and the packed size is the
    /// checked sum of the fragments. Rejections leave the pending member
    /// untouched.
    pub(crate) fn push(
        &mut self,
        entry: ArchiveEntry,
        split_before: bool,
        split_after: bool,
    ) -> Result<Option<ArchiveEntry>, SplitMergeError> {
        if split_before {
            let Some(pending) = self.pending.as_mut() else {
                return Err(SplitMergeError::ContinuationWithoutStart {
                    fragment: entry.header.name,
                });
            };
            let total = pending
                .header
                .packed_size
                .checked_add(entry.header.packed_size)
                .ok_or_else(|| SplitMergeError::PackedSizeOverflow {
                    pending: pending.header.name.clone(),
                })?;
            pending.header.packed_size = total;
            pending.chunks.extend(entry.chunks);
            if split_after {
                return Ok(None);
            }
            let mut finished = self.pending.take().expect("pending split member");
            finished.header.crc32_val = entry.header.crc32_val;
            finished.header.unpacked_size = entry.header.unpacked_size;
            return Ok(Some(finished));
        }

        if let Some(pending) = self.pending.as_ref() {
            return Err(if split_after {
                SplitMergeError::Overlapping {
                    pending: pending.header.name.clone(),
                }
            } else {
                SplitMergeError::Interrupted {
                    pending: pending.header.name.clone(),
                }
            });
        }
        if split_after {
            self.pending = Some(entry);
            Ok(None)
        } else {
            Ok(Some(entry))
        }
    }

    /// The member whose data continues in the next volume, if any: a
    /// per-member comment block that follows a volume's last fragment
    /// belongs to it.
    pub(crate) fn pending_mut(&mut self) -> Option<&mut ArchiveEntry> {
        self.pending.as_mut()
    }

    /// End of scan: a still-pending member is truncated.
    pub(crate) fn finish(self) -> Result<(), SplitMergeError> {
        match self.pending {
            Some(entry) => Err(SplitMergeError::MissingFinal {
                pending: entry.header.name,
            }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataChunk, FileHeader};

    fn fragment(
        name: &str,
        volume_index: usize,
        packed: u64,
        crc: u32,
        unpacked: u64,
    ) -> ArchiveEntry {
        ArchiveEntry {
            header: FileHeader {
                name: name.to_string(),
                packed_size: packed,
                unpacked_size: unpacked,
                crc32_val: Some(crc),
                ..Default::default()
            },
            chunks: vec![DataChunk {
                volume_index,
                data_offset: 0,
                packed_size: packed,
                crc32_val: None,
                is_final: true,
                extra_data: Vec::new(),
            }],
        }
    }

    #[test]
    fn fragments_merge_into_one_member() {
        let mut merge = SplitMerge::default();
        assert!(
            merge
                .push(fragment("m", 0, 100, 1, 111), false, true)
                .unwrap()
                .is_none()
        );
        assert!(
            merge
                .push(fragment("m", 1, 50, 2, 222), true, true)
                .unwrap()
                .is_none()
        );
        assert_eq!(merge.pending_mut().unwrap().header.packed_size, 150);

        let done = merge
            .push(fragment("m", 2, 25, 0xABCD, 333), true, false)
            .unwrap()
            .expect("final fragment completes the member");
        assert_eq!(
            done.header.packed_size, 175,
            "packed size is the fragment sum"
        );
        assert_eq!(done.header.crc32_val, Some(0xABCD), "final CRC wins");
        assert_eq!(done.header.unpacked_size, 333, "final unpacked size wins");
        let volumes: Vec<usize> = done.chunks.iter().map(|chunk| chunk.volume_index).collect();
        assert_eq!(volumes, [0, 1, 2], "chunks keep volume order");
        assert!(merge.pending_mut().is_none());
        merge.finish().unwrap();
    }

    /// A last fragment may carry no data (the member's bytes all fit in the
    /// earlier fragments); it still completes the member.
    #[test]
    fn zero_length_final_fragment_completes() {
        let mut merge = SplitMerge::default();
        merge.push(fragment("m", 0, 10, 1, 5), false, true).unwrap();
        let done = merge
            .push(fragment("m", 1, 0, 7, 5), true, false)
            .unwrap()
            .expect("empty final fragment completes the member");
        assert_eq!(done.header.packed_size, 10);
        assert_eq!(done.header.crc32_val, Some(7));
        assert_eq!(done.chunks.len(), 2);
    }

    #[test]
    fn regular_members_pass_through() {
        let mut merge = SplitMerge::default();
        let done = merge.push(fragment("a", 0, 1, 2, 3), false, false).unwrap();
        assert_eq!(done.unwrap().header.name, "a");
        merge.finish().unwrap();
    }

    #[test]
    fn malformed_fragments_are_rejected() {
        // Continuation with nothing pending.
        let mut merge = SplitMerge::default();
        assert_eq!(
            merge
                .push(fragment("orphan", 0, 1, 0, 0), true, false)
                .unwrap_err(),
            SplitMergeError::ContinuationWithoutStart {
                fragment: "orphan".into()
            }
        );

        // A second split member starts while one is pending.
        let mut merge = SplitMerge::default();
        merge
            .push(fragment("first", 0, 1, 0, 0), false, true)
            .unwrap();
        assert_eq!(
            merge
                .push(fragment("second", 0, 1, 0, 0), false, true)
                .unwrap_err(),
            SplitMergeError::Overlapping {
                pending: "first".into()
            }
        );

        // A regular member interrupts a pending one.
        let mut merge = SplitMerge::default();
        merge
            .push(fragment("first", 0, 1, 0, 0), false, true)
            .unwrap();
        assert_eq!(
            merge
                .push(fragment("interloper", 0, 1, 0, 0), false, false)
                .unwrap_err(),
            SplitMergeError::Interrupted {
                pending: "first".into()
            }
        );

        // The scan ends mid-member.
        let mut merge = SplitMerge::default();
        merge
            .push(fragment("truncated", 0, 1, 0, 0), false, true)
            .unwrap();
        assert_eq!(
            merge.finish().unwrap_err(),
            SplitMergeError::MissingFinal {
                pending: "truncated".into()
            }
        );
    }

    #[test]
    fn packed_size_overflow_is_checked() {
        let mut merge = SplitMerge::default();
        merge
            .push(fragment("huge", 0, u64::MAX, 0, 0), false, true)
            .unwrap();
        assert_eq!(
            merge
                .push(fragment("huge", 1, 1, 0, 0), true, false)
                .unwrap_err(),
            SplitMergeError::PackedSizeOverflow {
                pending: "huge".into()
            }
        );
        assert_eq!(
            merge.pending_mut().unwrap().header.packed_size,
            u64::MAX,
            "a rejected fragment leaves the pending member untouched"
        );
    }
}
