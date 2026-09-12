//! Solid-chain decode drivers (RAR5 and legacy RAR4).
//!
//! A chain member's bytes depend on the shared window, so decoding starts at
//! the chain head and runs forward to the target; `decode_solid_through_to`
//! additionally streams the intermediate output away.

use std::io::{self, Write};

use crate::archive::RarArchive;
use crate::codec::DecoderState;
use crate::error::RarResult;
use crate::format::shared::stream_mut;

impl RarArchive {
    /// Check if entry at `idx` is in a solid chain (is solid itself, or
    /// the next entry after it is solid).
    pub(super) fn is_solid_chain_member(&self, idx: usize) -> bool {
        let hdr = &self.entries[idx].header;
        if hdr.comp_solid {
            return true;
        }
        // First file in a solid group isn't flagged solid but the next one is
        if idx + 1 < self.entries.len() && self.entries[idx + 1].header.comp_solid {
            return true;
        }
        false
    }

    /// Reset RAR5 solid state to immediately before the current chain. Keeping
    /// the state and marker in lockstep is essential after a decoder or writer
    /// error because the local decoder may already have been partially mutated.
    fn reset_solid_decoder(&mut self, chain_start: usize) {
        let ctx = self.read_ctx_mut();
        ctx.solid_state = None;
        ctx.solid_decoded_through = chain_start as isize - 1;
    }

    /// Decode all files in the solid chain up through `target_idx`,
    /// returning the data for `target_idx`.
    pub(super) fn decode_solid_through(&mut self, target_idx: usize) -> RarResult<Vec<u8>> {
        let mut target_data = Vec::new();
        self.decode_solid_through_to(target_idx, &mut target_data)?;
        Ok(target_data)
    }

    /// Find the start index of the solid chain containing `target_idx`
    /// (the first non-directory file at or before it that is not solid,
    /// followed by solid files).
    fn find_solid_chain_start(&self, target_idx: usize) -> usize {
        let mut chain_start = target_idx;
        for i in (0..target_idx).rev() {
            if self.entries[i].is_dir() {
                continue;
            }
            if self.entries[i].header.comp_solid || self.is_solid_chain_member(i) {
                chain_start = i;
            } else {
                break;
            }
        }
        chain_start
    }

    /// Streaming variant of [`Self::decode_solid_through`]: decodes the
    /// chain up to `target_idx`, writing only the target member to
    /// `writer` (intermediate members are decoded to a discard sink so the
    /// shared window advances).
    pub(super) fn decode_solid_through_to(
        &mut self,
        target_idx: usize,
        writer: &mut dyn Write,
    ) -> RarResult<u64> {
        let chain_start = self.find_solid_chain_start(target_idx);

        let can_continue = {
            let ctx = self.read_ctx();
            ctx.solid_state.is_some()
                && ctx.solid_decoded_through >= chain_start as isize
                && ctx.solid_decoded_through < target_idx as isize
        };
        if !can_continue {
            self.reset_solid_decoder(chain_start);
        }

        if self.read_ctx().solid_state.is_none() {
            let dict_size = self.member_dict_window(chain_start)?;
            self.read_ctx_mut().solid_state = Some(DecoderState::new(dict_size));
        }

        let start_from = (self.read_ctx_mut().solid_decoded_through + 1) as usize;
        let mut target_written = 0u64;
        let mut discard = io::sink();

        for i in start_from..=target_idx {
            let entry = self.entries[i].clone();
            if entry.is_dir() {
                continue;
            }
            let sink: &mut dyn Write = if i == target_idx {
                writer
            } else {
                &mut discard
            };
            let mut state = self.read_ctx_mut().solid_state.take().unwrap();
            let written = match self.decode_file_to(i, sink, Some(&mut state)) {
                Ok(written) => written,
                Err(err) => {
                    self.reset_solid_decoder(chain_start);
                    return Err(err);
                }
            };
            self.read_ctx_mut().solid_state = Some(state);
            self.read_ctx_mut().solid_decoded_through = i as isize;
            if i == target_idx {
                target_written = written;
            }
        }

        Ok(target_written)
    }
    /// Whether `idx` sits in a legacy solid run. RAR3+ members (unp_ver >=
    /// 29) chain on the per-file FHD_SOLID bit (a head member is solid by
    /// being directly followed by a flagged member). Pre-RAR3 codecs never
    /// write that bit: when the main header carried MHD_SOLID, every
    /// compressed member of such a codec is part of one shared-window run.
    pub(super) fn is_rar4_solid_member(&self, idx: usize) -> bool {
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
    pub(super) fn rar4_find_chain_start(&self, target_idx: usize) -> usize {
        // Pre-RAR3 codecs under MHD_SOLID do not write FHD_SOLID. STORE
        // members leave the shared window untouched, so the run reaches back
        // across them to the first compressed member.
        if self.entries[target_idx].header.unp_ver < 29 {
            let mut chain_start = target_idx;
            for i in (0..target_idx).rev() {
                if !self.entries[i].is_dir()
                    && !crate::format::rar4::is_stored(self.entries[i].header.comp_method)
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
    pub(super) fn reset_rar4_solid_decoder(&mut self, chain_start: usize) {
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
                ctx.rar4_decoder = Some(crate::format::rar4::LegacyDecoder::Rar29(
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
            let is_compressed = !crate::format::rar4::is_stored(hdr.comp_method);
            if is_compressed {
                // Ensure the decoder matches this member's codec version.
                let needs_rebuild = {
                    let dec = self.read_ctx_mut().rar4_decoder.as_ref();
                    match (hdr.unp_ver, dec) {
                        (v, Some(crate::format::rar4::LegacyDecoder::Rar29(_))) if v >= 29 => false,
                        (20 | 26, Some(crate::format::rar4::LegacyDecoder::Rar20(_))) => false,
                        (15, Some(crate::format::rar4::LegacyDecoder::Rar15(_))) => false,
                        _ => true,
                    }
                };
                if needs_rebuild {
                    let new_decoder = if hdr.unp_ver >= 29 {
                        crate::format::rar4::LegacyDecoder::Rar29(
                            crate::codec::legacy::rar29::Rar29Decoder::new(),
                        )
                    } else if hdr.unp_ver == 20 || hdr.unp_ver == 26 {
                        crate::format::rar4::LegacyDecoder::Rar20(Box::default())
                    } else {
                        crate::format::rar4::LegacyDecoder::Rar15(Box::default())
                    };
                    self.read_ctx_mut().rar4_decoder = Some(new_decoder);
                }
            }

            let mut decoder = self.read_ctx_mut().rar4_decoder.take();
            let max_packed_bytes = self.max_packed_bytes();
            let data = match crate::format::rar4::decode_member_bytes(
                stream_mut(&mut self.stream)?,
                &self.volume_paths,
                &chunks,
                &hdr,
                crate::format::rar4::MemberDecodeOptions {
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
}
