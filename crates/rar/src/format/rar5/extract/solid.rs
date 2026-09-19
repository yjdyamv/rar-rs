//! Solid-chain decode drivers.
//!
//! A chain member's bytes depend on the shared window, so decoding starts at
//! the chain head and runs forward to the target; `decode_solid_through_to`
//! additionally streams the intermediate output away.

use std::io::{self, Write};

use crate::codec::DecoderState;
use crate::engine::Engine;
use crate::error::RarResult;
use crate::format::rar5::COMP_METHOD_STORE;

/// Check if entry at `idx` is in a solid chain (is solid itself, or
/// the next entry after it is solid).
pub(crate) fn is_solid_chain_member(cx: &dyn Engine, idx: usize) -> bool {
    let hdr = &cx.entries()[idx].header;
    if hdr.comp_solid {
        return true;
    }
    // First file in a solid group isn't flagged solid but the next one is
    if idx + 1 < cx.entries().len() && cx.entries()[idx + 1].header.comp_solid {
        return true;
    }
    false
}

/// Reset RAR5 solid state to immediately before the current chain. Keeping
/// the state and marker in lockstep is essential after a decoder or writer
/// error because the local decoder may already have been partially mutated.
fn reset_solid_decoder(cx: &mut dyn Engine, chain_start: usize) {
    let ctx = cx.read_ctx_mut();
    ctx.solid_state = None;
    ctx.solid_decoded_through = chain_start as isize - 1;
}

/// Decode all files in the solid chain up through `target_idx`,
/// returning the data for `target_idx`.
pub(crate) fn decode_solid_through(cx: &mut dyn Engine, target_idx: usize) -> RarResult<Vec<u8>> {
    let mut target_data = Vec::new();
    decode_solid_through_to(cx, target_idx, &mut target_data)?;
    Ok(target_data)
}

/// Find the start index of the solid chain containing `target_idx`:
/// walk back across members flagged `comp_solid` (each continues its
/// predecessor) to the first member that is not solid. A member that is
/// not solid itself starts a new group even when its predecessor is
/// solid (`-se`/`-sv` resets), so the walk must not cross it.
fn find_solid_chain_start(cx: &dyn Engine, target_idx: usize) -> usize {
    let mut chain_start = target_idx;
    while cx.entries()[chain_start].header.comp_solid {
        let Some(previous) = (0..chain_start).rev().find(|&i| !cx.entries()[i].is_dir()) else {
            break;
        };
        chain_start = previous;
    }
    chain_start
}

/// Streaming variant of [`decode_solid_through`]: decodes the
/// chain up to `target_idx`, writing only the target member to
/// `writer` (intermediate members are decoded to a discard sink so the
/// shared window advances).
pub(crate) fn decode_solid_through_to(
    cx: &mut dyn Engine,
    target_idx: usize,
    writer: &mut dyn Write,
) -> RarResult<u64> {
    let chain_start = find_solid_chain_start(cx, target_idx);

    let can_continue = {
        let ctx = cx.read_ctx();
        ctx.solid_state.is_some()
            && ctx.solid_decoded_through >= chain_start as isize
            && ctx.solid_decoded_through < target_idx as isize
    };
    if !can_continue {
        reset_solid_decoder(cx, chain_start);
    }

    let chain_window = crate::format::rar5::extract::decode::member_dict_window(cx, chain_start)?;
    if cx.read_ctx().solid_state.is_none() {
        cx.read_ctx_mut().solid_state = Some(DecoderState::new(chain_window));
    }

    let start_from = (cx.read_ctx_mut().solid_decoded_through + 1) as usize;
    let mut target_written = 0u64;
    let mut discard = io::sink();

    for i in start_from..=target_idx {
        let entry = cx.entries()[i].clone();
        if entry.is_dir() {
            continue;
        }
        // A continuation may declare a dictionary larger than the chain
        // head's (official archives do this); grow the shared window so
        // its distances stay addressable, carrying the lookbehind tail
        // and codec state forward instead of rejecting the archive.
        // `-mdx` still bounds the declared size via `member_dict_window`.
        if entry.header.comp_method != COMP_METHOD_STORE {
            let member_window = crate::format::rar5::extract::decode::member_dict_window(cx, i)?;
            let state = cx.read_ctx_mut().solid_state.as_mut().unwrap();
            if member_window > state.window_capacity() {
                state.grow_window(member_window);
            }
        }
        let sink: &mut dyn Write = if i == target_idx {
            writer
        } else {
            &mut discard
        };
        let mut state = cx.read_ctx_mut().solid_state.take().unwrap();
        let written = match crate::format::rar5::extract::decode::decode_file_to(
            cx,
            i,
            sink,
            Some(&mut state),
        ) {
            Ok(written) => written,
            Err(err) => {
                reset_solid_decoder(cx, chain_start);
                return Err(err);
            }
        };
        cx.read_ctx_mut().solid_state = Some(state);
        cx.read_ctx_mut().solid_decoded_through = i as isize;
        if i == target_idx {
            target_written = written;
        }
    }

    Ok(target_written)
}
