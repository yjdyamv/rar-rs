//! Rewrite planning: walk the archive and emit the op list.

use super::*;

use std::fs::File;

use std::io::{Read, Seek, SeekFrom};

use super::super::RarArchive;
use crate::crypto::parse_archive_encrypt_header;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{ArchiveHeader, split_main_extra};
use crate::format::rar5::{
    BLOCK_FLAG_DEPENDS_PREV, BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_ENCRYPT_HEADER,
    BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_FILE_HEADER, BLOCK_TYPE_SERVICE_HEADER, COMP_METHOD_STORE,
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
        // Signature (after any embedded SFX stub).
        let mut sig = [0u8; 8];
        reader.seek(SeekFrom::Start(self.sfx_offset))?;
        reader.read_exact(&mut sig)?;

        // Leading blocks: optional archive encryption header (plaintext),
        // then the main archive header (rebuilt so the locator stays
        // consistent with the rewritten archive).
        let mut encrypt_header = None;
        let first =
            crate::format::rar5::headers::read_block(reader, self.archive_block_key()?.as_ref())?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                self.handle_archive_encrypt_header(params)?;
                encrypt_header = Some(first.header_bytes);
                let main = crate::format::rar5::headers::read_block(
                    reader,
                    self.archive_block_key()?.as_ref(),
                )?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
                if main.block_type != BLOCK_TYPE_ARCHIVE_HEADER {
                    return Err(RarError::Format(
                        "archive is missing the main header".into(),
                    ));
                }
                main
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };

        // Decide quick-open capture and recovery rebuild from the main
        // header (write_main_header re-derives them from the same data).
        let ah = ArchiveHeader::from_raw(&main_meta.raw)?;
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

        while let Some(meta) =
            crate::format::rar5::headers::read_block(reader, self.archive_block_key()?.as_ref())?
        {
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
                            // participate in the LZ window.
                            if !deleted[idx] {
                                ops.push(RewriteOp::CopyBlock {
                                    qo_header: None,
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
                    let name = self.service_block_name(&meta)?;
                    if name.as_deref() == Some("RR") && rr_percent.is_none() {
                        rr_percent = self.rr_percent_from_block(&meta);
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
            // Advance past the data area (headers are read separately).
            reader.seek(SeekFrom::Start(meta.data_end))?;
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
