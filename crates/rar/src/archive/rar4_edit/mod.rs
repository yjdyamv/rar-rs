//! Header-level edits on existing RAR 1.5–4.x archives (ADR 0005, stage A).
//!
//! Three operations ship here: member rename (`rar rn`, and `rar ch` case
//! conversion which routes through the same rename path), inline
//! recovery-record add/replace (`rar rr`) and lock (`rar k`). All are pure
//! header/block surgery — no member data is decoded or recompressed, so
//! solid and non-solid archives are handled identically. Multi-volume sets
//! are edited per volume for renames and lock (official `rar` rewrites each
//! volume without rebalancing, so a volume may grow past `-v`); delete and
//! append on a set are refused exactly like the official "Cannot modify
//! volume", archive comments are supported (the `CMT` block lands right after
//! the first volume's main header); recovery-record and per-member-comment
//! edits on a set are refused (a volume set uses `.rev` recovery volumes,
//! and RAR4 member comments are not interoperable — see `PLAN.md`).
//!
//! Rename rebuilds each FILE_HEAD's encoded name field in place (keeping
//! every other field byte-identical, including salt / nested comment /
//! extended time) and recomputes the header CRC16 over the reader's
//! coverage. Directory renames expand to descendants, mirroring the RAR5
//! engine. Recovery (`rr`) and any edit on an archive that carries a
//! NEWSUB record rebuild that record over the new prefix — the tags are
//! offset-based, so a renamed header would otherwise leave stale tags.
//!
//! Header-encrypted (`-hp`) archives are editable: the main header is the
//! plaintext marker carrying MHD_PASSWORD, so the layout scan decrypts every
//! later block header with the archive password and the rewrite re-encrypts
//! each block it rebuilds or inserts with a fresh salt (untouched blocks are
//! copied as ciphertext). Multi-volume sets follow the same per-volume rule.
//!
//! Lock (`k`) patches the 13-byte main header in place (mirroring the RAR5
//! lock). Recovery reads the whole archive into memory — the NEWSUB record
//! is XOR parity over the protected prefix, so the builder needs the
//! prefix anyway (the same bound the RAR4 creation path accepts) — and
//! stages a rewritten copy next to the archive before replacing it
//! atomically.
//!
//! Role split:
//! - [`layout`] — main-header access and the block/layout scan,
//! - [`headers`] — FILE_HEAD rename and nested-comment rewriting,
//! - [`comment`] — the `CMT` archive-comment block,
//! - [`engine`] — append prelude and the combined edit transaction,
//! - [`repack`] — solid-archive decode/re-encode repack (stage C).

mod comment;
mod engine;
mod headers;
mod layout;
mod repack;
#[cfg(test)]
mod tests;

pub(crate) use comment::{build_comment_block, encode_comment_text, read_comment};
pub(crate) use engine::{SolidAppendEntry, append_prelude, edit_rar4};
pub(crate) use layout::lock_archive;
pub(crate) use repack::repack_solid_archive;

/// Header byte count of the NEWSUB `CMT` archive-comment block (32 fixed
/// bytes + the 3-byte name `CMT`); the payload follows as data.
pub(crate) const CMT_HEAD_SIZE: usize = 35;
/// Header byte count of the NEWSUB `RR` recovery record built by
/// `build_legacy_recovery_block` (32 fixed + 2-byte name + 20-byte tail);
/// the tag table and parity sectors follow as data.
pub(crate) const RECOVERY_HEAD_SIZE: usize = 54;

/// RAR4 header CRC16: standard CRC-32 truncated to 16 bits over
/// `bytes[2..]` (the body after the CRC field).
fn header_crc16(body: &[u8]) -> u16 {
    (crate::crc32::crc32(body) & 0xffff) as u16
}
