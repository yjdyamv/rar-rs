//! RAR 1.5–4.x read path: volume scanning, member decoding and the legacy
//! solid-chain driver.
//!
//! Everything a legacy archive needs to be listed or extracted lives here;
//! the family-neutral orchestration is in
//! [`crate::format::shared::extract`].

use std::fs::File;
use std::io::{Read, Write};

use super::{LegacyDecoder, MemberDecodeOptions, Rar4VolumeScan};
use crate::archive::RarArchive;
use crate::detect::RAR4_SIGNATURE;
use crate::error::{RarError, RarResult};
use crate::format::shared::extract::{MAX_CATALOG_ENTRIES, check_entry_cap};
use crate::format::shared::stream_mut;
use crate::model::FileHeader;

impl RarArchive {
    /// Scan a RAR 1.5–4.x volume set (legacy fixed-width block headers) into
    /// the entry catalog. Volume 0 is the already-open primary stream,
    /// positioned right after the signature (SFX-aware); later volumes open
    /// fresh and each starts with its own 7-byte signature.
    pub(crate) fn open_read_rar4(&mut self) -> RarResult<()> {
        self.entries.clear();
        let mut scan = Rar4VolumeScan::default();
        let mut out = Vec::new();

        scan.scan_volume(
            stream_mut(&mut self.stream)?,
            0,
            self.password.as_deref(),
            &mut out,
        )?;
        check_entry_cap(out.len(), MAX_CATALOG_ENTRIES)?;
        for (vol_idx, vol_path) in self.volume_paths.iter().enumerate().skip(1) {
            self.check_cancel()?;
            let mut stream = File::open(vol_path)?;
            let mut sig = [0u8; 7];
            stream.read_exact(&mut sig)?;
            if &sig != RAR4_SIGNATURE {
                return Err(RarError::Format(format!(
                    "volume {} has a bad RAR4 signature",
                    vol_path.display()
                )));
            }
            scan.scan_volume(&mut stream, vol_idx, self.password.as_deref(), &mut out)?;
            check_entry_cap(out.len(), MAX_CATALOG_ENTRIES)?;
        }
        let archive_solid = scan.archive_solid;
        let new_numbering = scan.new_numbering;
        scan.finish()?;
        self.rar4_solid_archive = archive_solid;
        self.rar4_new_numbering = new_numbering;
        self.entries = out;
        Ok(())
    }

    /// Whether `idx` sits in a legacy solid run. RAR3+ members (unp_ver >=
    /// 29) chain on the per-file FHD_SOLID bit (a head member is solid by
    /// being directly followed by a flagged member). Pre-RAR3 codecs never
    /// write that bit: when the main header carried MHD_SOLID, every
    /// compressed member of such a codec is part of one shared-window run.
    fn is_rar4_solid_member(&self, idx: usize) -> bool {
        let hdr = &self.entries[idx].header;
        if hdr.unp_ver < 29 {
            return self.rar4_solid_archive && !self.entries[idx].is_dir();
        }
        if hdr.comp_solid {
            return true;
        }
        idx + 1 < self.entries.len() && self.entries[idx + 1].header.comp_solid
    }

    /// Find the start index of the legacy solid chain containing `idx` (the
    /// first member at or before it that is not solid, followed by solid
    /// members; directory entries do not break the run).
    fn rar4_find_chain_start(&self, target_idx: usize) -> usize {
        // Pre-RAR3 codecs under MHD_SOLID do not write FHD_SOLID. STORE
        // members leave the shared window untouched, so the run reaches back
        // across them to the first compressed member.
        if self.entries[target_idx].header.unp_ver < 29 {
            let mut chain_start = target_idx;
            for i in (0..target_idx).rev() {
                if !self.entries[i].is_dir()
                    && !super::is_stored(self.entries[i].header.comp_method)
                {
                    chain_start = i;
                }
            }
            return chain_start;
        }

        // For RAR3+, FHD_SOLID belongs to the current member and means it
        // continues the previous non-directory member. Stop as soon as the
        // current chain head is unflagged; inspecting the previous member's
        // flag would incorrectly cross into an independent earlier run.
        let mut chain_start = target_idx;
        while self.entries[chain_start].header.comp_solid {
            let Some(previous) = (0..chain_start).rev().find(|&i| !self.entries[i].is_dir()) else {
                break;
            };
            if self.entries[previous].header.unp_ver < 29 {
                break;
            }
            chain_start = previous;
        }
        chain_start
    }

    /// Reset the legacy solid decoder to immediately before the current run.
    fn reset_rar4_solid_decoder(&mut self, chain_start: usize) {
        let ctx = self.read_ctx_mut();
        ctx.rar4_decoder = None;
        ctx.rar4_decoded_through = chain_start as isize - 1;
    }

    /// Decode the legacy solid chain up through `target_idx` with one shared
    /// decoder, returning the target member's bytes. Intermediate members are
    /// decoded only to advance the shared window. STORE members in a RAR2.x
    /// or RAR1.5 chain do not advance the window but do not break the chain
    /// either (the decoder is simply not called).
    pub(crate) fn rar4_decode_solid_through(&mut self, target_idx: usize) -> RarResult<Vec<u8>> {
        let chain_start = self.rar4_find_chain_start(target_idx);

        let start_from = {
            let ctx = self.read_ctx_mut();
            if ctx.rar4_decoder.is_some()
                && ctx.rar4_decoded_through >= chain_start as isize
                && ctx.rar4_decoded_through < target_idx as isize
            {
                // Continue from where we left off.
            } else {
                // Backwards request or a fresh chain: restart from this run's
                // head, not from unrelated members in an earlier solid run.
                ctx.rar4_decoder = None;
                ctx.rar4_decoded_through = chain_start as isize - 1;
            }
            if ctx.rar4_decoder.is_none() {
                // Bootstrap with a Rar29 decoder; it will be replaced on the
                // first compressed member that reveals the actual unp_ver.
                ctx.rar4_decoder = Some(LegacyDecoder::Rar29(
                    crate::codec::legacy::rar29::Rar29Decoder::new(),
                ));
            }
            (ctx.rar4_decoded_through + 1) as usize
        };

        let mut target = Vec::new();
        for i in start_from..=target_idx {
            self.validate_entry_limits(i)?;
            let entry = self.entries[i].clone();
            if entry.is_dir() {
                continue;
            }
            let hdr = entry.header;
            let chunks = entry.chunks;

            // Determine decoder type from the member's unp_ver. A STORE
            // member keeps the existing decoder unchanged (for RAR2.x the
            // window is not advanced; for RAR1.5 likewise).
            let is_compressed = !super::is_stored(hdr.comp_method);
            if is_compressed {
                // Ensure the decoder matches this member's codec version.
                let needs_rebuild = {
                    let dec = self.read_ctx_mut().rar4_decoder.as_ref();
                    match (hdr.unp_ver, dec) {
                        (v, Some(LegacyDecoder::Rar29(_))) if v >= 29 => false,
                        (20 | 26, Some(LegacyDecoder::Rar20(_))) => false,
                        (15, Some(LegacyDecoder::Rar15(_))) => false,
                        _ => true,
                    }
                };
                if needs_rebuild {
                    let new_decoder = if hdr.unp_ver >= 29 {
                        LegacyDecoder::Rar29(crate::codec::legacy::rar29::Rar29Decoder::new())
                    } else if hdr.unp_ver == 20 || hdr.unp_ver == 26 {
                        LegacyDecoder::Rar20(Box::default())
                    } else {
                        LegacyDecoder::Rar15(Box::default())
                    };
                    self.read_ctx_mut().rar4_decoder = Some(new_decoder);
                }
            }

            let mut decoder = self.read_ctx_mut().rar4_decoder.take();
            let max_packed_bytes = self.max_packed_bytes();
            let data = match super::decode_member_bytes(
                stream_mut(&mut self.stream)?,
                &self.volume_paths,
                &chunks,
                &hdr,
                MemberDecodeOptions {
                    password: self.password.as_deref(),
                    decoder: decoder.as_mut(),
                    max_alloc_packed_bytes: max_packed_bytes,
                    max_stream_packed_bytes: max_packed_bytes,
                },
            ) {
                Ok(data) => data,
                Err(err) => {
                    self.reset_rar4_solid_decoder(chain_start);
                    return Err(err);
                }
            };
            self.read_ctx_mut().rar4_decoder = decoder;
            self.read_ctx_mut().rar4_decoded_through = i as isize;
            if i == target_idx {
                target = data;
            }
        }
        Ok(target)
    }

    /// Decode a single RAR4 member in memory, verifying its CRC32. Solid
    /// chain members decode through their chain prefix (shared window).
    pub(crate) fn decode_rar4_at(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        self.validate_entry_limits(idx)?;
        let hdr = self.entries[idx].header.clone();
        if self.is_rar4_solid_member(idx) {
            let chain_start = self.rar4_find_chain_start(idx);
            let result = self
                .rar4_decode_solid_through(idx)
                .and_then(|out| self.rar4_verify_crc(&hdr, &out).map(|()| out));
            if result.is_err() {
                self.reset_rar4_solid_decoder(chain_start);
            }
            return result;
        }
        let out = self.rar4_decode_member(idx)?;
        self.rar4_verify_crc(&hdr, &out)?;
        Ok(out)
    }

    /// Maximum packed bytes accepted by a truly streaming STORE path. With no
    /// unpacked limit there is no allocation-driven packed-size ceiling.
    fn max_stream_packed_bytes(&self) -> u64 {
        self.read_ctx()
            .extract_options
            .max_unpacked_bytes
            .map(|u| u.saturating_add(1 << 20))
            .unwrap_or(u64::MAX)
    }

    /// Decode a single RAR4 member, streaming output to `writer`, verifying
    /// its CRC32 over the written bytes. Non-chain members stream through
    /// the bounded-memory path (STORE chunks copied straight out; compressed
    /// members decode incrementally); solid-chain members keep the shared
    /// window semantics and decode in one pass.
    pub(crate) fn decode_rar4_to(&mut self, idx: usize, writer: &mut dyn Write) -> RarResult<u64> {
        self.validate_entry_limits(idx)?;
        let hdr = self.entries[idx].header.clone();
        if self.is_rar4_solid_member(idx) {
            let chain_start = self.rar4_find_chain_start(idx);
            let result = self.rar4_decode_solid_through(idx).and_then(|out| {
                self.rar4_verify_crc(&hdr, &out)?;
                writer.write_all(&out).map_err(RarError::Io)?;
                Ok(out.len() as u64)
            });
            if result.is_err() {
                self.reset_rar4_solid_decoder(chain_start);
            }
            return result;
        }
        let entry = self.entries[idx].clone();
        let max_alloc_packed_bytes = self.max_packed_bytes();
        let max_stream_packed_bytes = self.max_stream_packed_bytes();
        let (written, crc, rar13_checksum) = super::decode_member_bytes_to(
            stream_mut(&mut self.stream)?,
            &self.volume_paths,
            &entry.chunks,
            &entry.header,
            MemberDecodeOptions {
                password: self.password.as_deref(),
                decoder: None,
                max_alloc_packed_bytes,
                max_stream_packed_bytes,
            },
            writer,
        )?;
        // The streamed checksum is authoritative; compare with the header.
        if let Some(expected) = hdr.crc32_val {
            let actual = if hdr.format_version == 3 {
                u32::from(rar13_checksum)
            } else {
                crc
            };
            if actual != expected {
                return Err(RarError::Crc {
                    expected,
                    actual,
                    context: format!("{}: checksum mismatch", hdr.name),
                });
            }
        }
        Ok(written)
    }

    /// Decode RAR4 member `idx`, routing solid-chain members through the
    /// persistent legacy decoder so their look-behind window covers the
    /// chain prefix.
    fn rar4_decode_member(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        if self.is_rar4_solid_member(idx) {
            return self.rar4_decode_solid_through(idx);
        }
        let entry = self.entries[idx].clone();
        let max_packed_bytes = self.max_packed_bytes();
        super::decode_member_bytes(
            stream_mut(&mut self.stream)?,
            &self.volume_paths,
            &entry.chunks,
            &entry.header,
            MemberDecodeOptions {
                password: self.password.as_deref(),
                decoder: None,
                max_alloc_packed_bytes: max_packed_bytes,
                max_stream_packed_bytes: max_packed_bytes,
            },
        )
    }

    fn rar4_verify_crc(&self, hdr: &FileHeader, data: &[u8]) -> RarResult<()> {
        if let Some(expected) = hdr.crc32_val {
            let actual = if hdr.format_version == 3 {
                u32::from(crate::format::rar13::file_checksum(data))
            } else {
                super::member_crc(data)
            };
            if actual != expected {
                return Err(RarError::Crc {
                    expected,
                    actual,
                    context: format!("{}: checksum mismatch", hdr.name),
                });
            }
        }
        Ok(())
    }
}
