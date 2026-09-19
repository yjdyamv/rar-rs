//! Rewrite planning: walk the archive and emit the op list.

use super::*;

use std::fs::File;

use super::super::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{
    BlockCursor, parse_service_block_name, parse_service_recovery_percent, split_main_extra,
};
use crate::format::rar5::{
    BLOCK_FLAG_DEPENDS_PREV, BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_FILE_HEADER,
    BLOCK_TYPE_SERVICE_HEADER, COMP_METHOD_STORE,
};

impl RarArchive {
    /// Walk the archive and build the rewrite plan: verbatim copies for
    /// every kept block, recompression ops for the affected solid chain,
    /// and the dropped QO/RR service records (the RR percentage is parsed
    /// so the record can be rebuilt).
    pub(super) fn plan_rewrite(
        &mut self,
        reader: &mut File,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        force_rr: Option<u8>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
        comment: Option<&[u8]>,
    ) -> RarResult<RewritePlan> {
        let file_len = reader.metadata().map_err(RarError::Io)?.len();

        // Leading blocks: optional archive encryption header (plaintext),
        // then the main archive header (rebuilt so the locator stays
        // consistent with the rewritten archive). The opener re-emits the
        // encryption header verbatim for the rewrite.
        let main = crate::format::rar5::extract::open::read_main_header(self, reader)?;
        let encrypt_header = main.encrypt_header;
        let main_meta = main.meta;
        let ah = main.parsed;

        // Decide quick-open capture and recovery rebuild from the main
        // header (write_main_header re-derives them from the same data).
        let (had_qo, _had_rr, _) = split_main_extra(&ah.extra_data)?;
        let capture_qo = had_qo && !self.header_encryption;

        let mut ops = Vec::new();
        let mut entry_idx = 0usize;
        let mut chain_active = false;
        let mut chain_end = usize::MAX;
        let mut rr_percent = force_rr;
        // Service blocks flagged as dependent on the previous block (e.g.
        // NTFS streams/ACLs) belong to their file: drop them when that file
        // was deleted.
        let mut prev_file_deleted = false;

        let mut blocks = BlockCursor::new(
            file_len,
            crate::format::rar5::extract::verify::archive_block_key(self)?,
        );
        while let Some(meta) = blocks.next(reader)? {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_FILE_HEADER => {
                    let idx = entry_idx;
                    entry_idx += 1;
                    if !chain_active
                        && let Some((s, e)) = chain
                        && s == idx
                    {
                        chain_active = true;
                        chain_end = e;
                    }
                    if chain_active && idx <= chain_end {
                        let entry = &self.entries[idx];
                        if entry.is_dir() || entry.header.comp_method == COMP_METHOD_STORE {
                            // Directories and STORE members never
                            // participate in the LZ window, but their copied
                            // headers still belong in the rebuilt quick-open
                            // record.
                            if !deleted[idx] {
                                ops.push(RewriteOp::CopyBlock {
                                    qo_header: capture_qo.then(|| meta.header_bytes.clone()),
                                    header_bytes: meta.header_bytes,
                                    src_data: meta.data_offset,
                                    len: meta.raw.data_size,
                                });
                            }
                        } else {
                            ops.push(RewriteOp::Recompress {
                                idx,
                                is_deleted: deleted[idx],
                            });
                        }
                        if idx == chain_end {
                            chain_active = false;
                        }
                        prev_file_deleted = deleted[idx];
                    } else if deleted[idx] {
                        prev_file_deleted = true;
                    } else {
                        let header_bytes = match rename_map.and_then(|m| m.get(&idx)) {
                            Some(new_name) => {
                                let mut fh = self.entries[idx].header.clone();
                                fh.name = new_name.clone();
                                fh.to_bytes()
                            }
                            None => meta.header_bytes,
                        };
                        ops.push(RewriteOp::CopyBlock {
                            qo_header: if capture_qo {
                                Some(header_bytes.clone())
                            } else {
                                None
                            },
                            header_bytes,
                            src_data: meta.data_offset,
                            len: meta.raw.data_size,
                        });
                        prev_file_deleted = false;
                    }
                }
                BLOCK_TYPE_SERVICE_HEADER => {
                    let name = parse_service_block_name(&meta.raw.header_data)?;
                    if name.as_deref() == Some("RR") && rr_percent.is_none() {
                        rr_percent = parse_service_recovery_percent(&meta.raw.header_data);
                    }
                    let drops = name.as_deref() == Some("QO")
                        || name.as_deref() == Some("RR")
                        || (comment.is_some() && name.as_deref() == Some("CMT"))
                        || (meta.flags & BLOCK_FLAG_DEPENDS_PREV != 0 && prev_file_deleted);
                    if !drops {
                        ops.push(RewriteOp::CopyBlock {
                            qo_header: None,
                            header_bytes: meta.header_bytes,
                            src_data: meta.data_offset,
                            len: meta.raw.data_size,
                        });
                    }
                }
                _ => ops.push(RewriteOp::CopyBlock {
                    qo_header: None,
                    header_bytes: meta.header_bytes,
                    src_data: meta.data_offset,
                    len: meta.raw.data_size,
                }),
            }
        }

        Ok(RewritePlan {
            ops,
            encrypt_header,
            main_meta,
            rr_percent: rr_percent.filter(|p| *p <= 100),
            comment: comment.map(|c| c.to_vec()),
        })
    }
}
