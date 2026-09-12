//! Rewrite execution: write the planned ops, QO/RR records and end block.

use super::*;

use std::fs::File;
use std::path::Path;

use std::io::{self, Read, Seek, SeekFrom, Write};

use super::super::{DecryptedPayload, RarArchive};
use crate::codec::{DecoderState, lzss_huff as compression};
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::build_comment_block;
use crate::format::rar5::{COMP_METHOD_STORE, RAR5_SIGNATURE};

/// Read-ahead copy job: `len` bytes from `src` in the original archive.
#[cfg(feature = "parallel")]
#[derive(Clone, Copy)]
struct CopyJob {
    src: u64,
    len: u64,
}

/// Bounded producer thread that prefetches verbatim block data ahead of
/// the writer, overlapping source reads with destination writes (and, for
/// solid chains, with the CPU-bound recompression).
#[cfg(feature = "parallel")]
struct CopyPipeline {
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, RarError>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "parallel")]
impl CopyPipeline {
    const CHUNK: usize = 4 * 1024 * 1024;
    const QUEUE: usize = 4;

    fn start(src_path: &Path, jobs: &[CopyJob]) -> Self {
        let src_path = src_path.to_path_buf();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, RarError>>(Self::QUEUE);
        let jobs = jobs.to_vec();
        let handle = std::thread::spawn(move || {
            let mut f = match File::open(src_path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Err(e.into()));
                    return;
                }
            };
            for job in jobs {
                if let Err(e) = f.seek(SeekFrom::Start(job.src)) {
                    let _ = tx.send(Err(e.into()));
                    return;
                }
                let mut left = job.len;
                while left > 0 {
                    let want = left.min(Self::CHUNK as u64) as usize;
                    let mut buf = vec![0u8; want];
                    if let Err(e) = f.read_exact(&mut buf) {
                        let _ = tx.send(Err(e.into()));
                        return;
                    }
                    if tx.send(Ok(buf)).is_err() {
                        return; // consumer aborted
                    }
                    left -= want as u64;
                }
            }
        });
        CopyPipeline {
            rx,
            handle: Some(handle),
        }
    }

    /// Next prefetched buffer, in job order.
    fn take(&self) -> RarResult<Option<Vec<u8>>> {
        match self.rx.recv() {
            Ok(Ok(buf)) => Ok(Some(buf)),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }

    fn finish(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl RarArchive {
    /// Write the plan to `self.stream`: signature, archive encryption
    /// header, rebuilt main header, then every op in order, and finally
    /// the quick-open record, the main header locator patch, the recovery
    /// record and the end block.
    pub(super) fn execute_rewrite(
        &mut self,
        plan: &RewritePlan,
        src_path: &Path,
        tmp_path: &Path,
    ) -> RarResult<()> {
        let out = self.stream.as_mut().unwrap();
        if self.sfx_offset > 0 {
            // Preserve the embedded SFX stub of the original archive.
            let mut stub = File::open(src_path)?;
            stub.seek(SeekFrom::Start(0))?;
            let mut limited = stub.take(self.sfx_offset + RAR5_SIGNATURE.len() as u64);
            io::copy(&mut limited, out)?;
        } else {
            out.write_all(RAR5_SIGNATURE)?;
        }
        if let Some(ref enc) = plan.encrypt_header {
            out.write_all(enc)?;
        }
        let (main_start, qo_field_pos, rr_field_pos, main_hdr) =
            self.write_main_header(&plan.main_meta, plan.rr_percent)?;
        if let Some(ref comment) = plan.comment
            && !comment.is_empty()
        {
            let block = build_comment_block(comment);
            self.write_block_header(&block)?;
        }

        // Prefetch every verbatim block with a background reader when the
        // total volume justifies the thread (parallel feature).
        #[cfg(feature = "parallel")]
        let mut pipeline: Option<CopyPipeline> = None;
        #[cfg(not(feature = "parallel"))]
        let pipeline: Option<()> = None;
        #[cfg(feature = "parallel")]
        {
            const PARALLEL_MIN_COPY: u64 = 32 * 1024 * 1024;
            let total_copy: u64 = plan
                .ops
                .iter()
                .map(|op| match op {
                    RewriteOp::CopyBlock { len, .. } => *len,
                    RewriteOp::Recompress { .. } => 0,
                })
                .sum();
            if total_copy >= PARALLEL_MIN_COPY && plan.ops.len() >= 4 {
                let jobs: Vec<CopyJob> = plan
                    .ops
                    .iter()
                    .filter_map(|op| match op {
                        RewriteOp::CopyBlock { src_data, len, .. } => Some(CopyJob {
                            src: *src_data,
                            len: *len,
                        }),
                        RewriteOp::Recompress { .. } => None,
                    })
                    .collect();
                pipeline = Some(CopyPipeline::start(src_path, &jobs));
            }
        }
        let mut reader = File::open(src_path)?;
        let mut dec = None;
        let mut enc = None;
        let mut enc_active = false;
        // Rewrite progress: `(processed_input_bytes, total_bytes)` where
        // total is the rewrite work (kept payload bytes + recompressed
        // input) and `processed` counts input bytes consumed, so the
        // fraction reaches 100% even when deletion shrinks the output.
        let total_bytes: u64 = plan
            .ops
            .iter()
            .map(|op| match op {
                RewriteOp::CopyBlock { len, .. } => *len,
                RewriteOp::Recompress { idx, .. } => self.entries[*idx].header.unpacked_size,
            })
            .sum();
        let mut processed = 0u64;

        for op in &plan.ops {
            self.check_cancel()?;
            if self.progress.is_some() {
                self.report_progress(processed, total_bytes);
            }
            match op {
                RewriteOp::CopyBlock {
                    header_bytes,
                    src_data,
                    len,
                    qo_header,
                } => {
                    let out_pos = self.stream.as_mut().unwrap().stream_position()?;
                    if let Some(qh) = qo_header {
                        self.write_ctx_mut()
                            .quick_open_entries
                            .push((out_pos, qh.clone()));
                    }
                    self.stream.as_mut().unwrap().write_all(header_bytes)?;
                    processed += len;
                    #[cfg_attr(not(feature = "parallel"), allow(unused_mut))]
                    let mut left = *len;
                    #[cfg(feature = "parallel")]
                    if let Some(pipe) = &pipeline {
                        while left > 0 {
                            let buf = pipe.take()?.ok_or_else(|| {
                                RarError::Io(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "copy pipeline ended early",
                                ))
                            })?;
                            self.stream.as_mut().unwrap().write_all(&buf)?;
                            left -= buf.len() as u64;
                        }
                    }
                    #[cfg(not(feature = "parallel"))]
                    let _ = &pipeline;
                    if left > 0 {
                        reader.seek(SeekFrom::Start(*src_data))?;
                        let mut limited = (&mut reader).take(left);
                        io::copy(&mut limited, self.stream.as_mut().unwrap())?;
                    }
                }
                RewriteOp::Recompress { idx, is_deleted } => {
                    if dec.is_none() {
                        let dict_log = self.entries[*idx].header.comp_dict_size;
                        let dict_size =
                            (128usize * 1024)
                                .checked_shl(dict_log as u32)
                                .ok_or_else(|| {
                                    RarError::Format(
                                        "dictionary size overflows host address space".into(),
                                    )
                                })?;
                        dec = Some(DecoderState::new(dict_size));
                        enc = Some(crate::codec::EncoderState::default());
                        enc_active = false;
                    }
                    self.recompress_chain_member(
                        &mut reader,
                        *idx,
                        *is_deleted,
                        dec.as_mut().unwrap(),
                        enc.as_mut().unwrap(),
                        &mut enc_active,
                    )?;
                    processed += self.entries[*idx].header.unpacked_size;
                }
            }
        }
        #[cfg(feature = "parallel")]
        if let Some(mut pipe) = pipeline {
            pipe.finish();
        }
        #[cfg(not(feature = "parallel"))]
        let _ = pipeline;

        if self.progress.is_some() {
            self.report_progress(processed, total_bytes);
        }

        // Quick-open record (rebuilt from the kept headers), locator patch
        // (with the recovery offset), recovery record and end block.
        let qo_pos = if self.write_ctx().quick_open {
            Some(self.write_quick_open_record()?)
        } else {
            None
        };
        let rr_pos = if self.recovery_percent.is_some() {
            Some(self.stream.as_mut().unwrap().stream_position()?)
        } else {
            None
        };
        if qo_pos.is_some() || rr_pos.is_some() {
            self.patch_main_header(
                qo_pos,
                rr_pos,
                main_start,
                qo_field_pos,
                rr_field_pos,
                &main_hdr,
            )?;
        }
        if rr_pos.is_some() {
            self.write_recovery_record_from(tmp_path)?;
        }
        self.write_end_block()?;
        Ok(())
    }

    /// Decode and recompress one member of the affected solid chain.
    ///
    /// Kept members are decoded with the shared decoder window and
    /// recompressed with a shared encoder window; deleted members are only
    /// decoded (to advance the window) and their blocks are not written.
    fn recompress_chain_member(
        &mut self,
        reader: &mut File,
        idx: usize,
        is_deleted: bool,
        dec: &mut DecoderState,
        enc: &mut crate::codec::EncoderState,
        enc_active: &mut bool,
    ) -> RarResult<()> {
        if is_deleted {
            let _ = self.decode_chain_member(reader, idx, dec)?;
            return Ok(());
        }
        let entry = self.entries[idx].clone();
        let hdr = &entry.header;
        let data = self.decode_chain_member(reader, idx, dec)?;

        let plain_crc = crc32fast::hash(&data);
        let plain_blake = hdr
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(&data));
        let variant = crate::version::ArchiveVersion::from_v70(hdr.dict_size_bytes.is_some());
        let packed = compression::encode_chunked(
            &data,
            compression::EncodeOptions {
                chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                state: Some(enc),
                is_final: true,
                variant,
                ..compression::EncodeOptions::new(hdr.comp_method, hdr.comp_dict_size)
            },
        )?;

        let (method, dsl, dict_bytes, payload) = if packed.len() >= data.len() {
            // Compression is a net loss: STORE resets the chain, matching
            // the sequential add_file path.
            enc.reset();
            *enc_active = false;
            (COMP_METHOD_STORE, 0u8, None, data.clone())
        } else {
            *enc_active = true;
            (
                hdr.comp_method,
                hdr.comp_dict_size,
                hdr.dict_size_bytes,
                packed,
            )
        };
        let (header_crc, extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        let payload = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &payload,
        )?;
        self.write_file_entry(
            &hdr.name,
            data.len() as u64,
            &payload,
            header_crc,
            method,
            dsl,
            dict_bytes,
            &extra_data,
            hdr.attributes,
            hdr.mtime,
            *enc_active,
            stored_hash,
        )?;
        Ok(())
    }

    /// Decode member `idx` with a shared decoder state, verifying its
    /// integrity. Reads directly from `reader` (the original archive) since
    /// `self.stream` is the replacement file during deletion.
    fn decode_chain_member(
        &mut self,
        reader: &mut File,
        idx: usize,
        state: &mut DecoderState,
    ) -> RarResult<Vec<u8>> {
        let hdr = &self.entries[idx].header;
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }
        let payload = self.read_packed_single(reader, idx)?;
        let mut raw_data = Vec::new();
        crate::format::rar5::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            Some(state),
            &mut raw_data,
        )?;
        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(&raw_data));
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(raw_data)
    }

    /// Read the packed (and decrypted, when applicable) payload of a
    /// single-volume member directly from `reader`.
    fn read_packed_single(&mut self, reader: &mut File, idx: usize) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let mut rr = crate::format::rar5::payload::SingleFileReader { reader };
        crate::format::rar5::payload::read_packed(
            &mut rr,
            hdr,
            &entry.chunks,
            &hdr.name,
            self.password.as_deref(),
            self.max_packed_bytes(),
            || Ok(()),
        )
    }
}
