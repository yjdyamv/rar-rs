//! Whole-archive repack of a solid RAR4 archive (ADR 0005 stage C).
//!
//! Every member is decoded in chain order through the shared window and
//! re-encoded into a fresh solid archive — same order, minus the deleted
//! members, with renames and each member's original compression level and
//! timestamps — then the comment and recovery record are applied
//! structurally and the result replaces the original atomically. Mirrors
//! WinRAR 7.21+'s full-archive repacking for solid RAR4 edits (the surgical
//! partial reprocess of 7.20 is not reproduced).

use std::collections::HashMap;
use std::fs;

use super::comment::read_comment;
use super::engine::edit_rar4;
use super::layout::{archive_is_header_encrypted, header_password};
use crate::archive::RarArchive;
use crate::archive::transaction::EditSummary;
use crate::engine::SolidAppendEntry;
use crate::error::{RarError, RarResult};
use crate::fs::atomic::{install_durable, temp_sibling_path};
use crate::recovery::legacy_rr::scan_protect_file;

/// Member generation the repacked archive is written with, derived from the
/// source members: RAR 1.5, RAR 2.x (the `26` alias folds onto the same
/// codec) or RAR 3.x/4.x (likewise `36`). A solid chain shares one codec, so
/// a mixed archive is refused rather than silently re-coded.
fn solid_repack_version(archive: &RarArchive) -> RarResult<crate::version::ArchiveVersion> {
    use crate::version::LegacyCodec;

    let mut target: Option<LegacyCodec> = None;
    for entry in archive.entries.iter().filter(|entry| !entry.is_dir()) {
        let codec = LegacyCodec::from_unp_ver(entry.header.unp_ver).ok_or_else(|| {
            RarError::Unsupported(format!(
                "repacking solid archives with unp_ver {} members is not supported",
                entry.header.unp_ver
            ))
        })?;
        match target {
            None => target = Some(codec),
            Some(existing) if existing == codec => {}
            Some(_) => {
                return Err(RarError::Unsupported(
                    "repacking solid archives with mixed member generations is not supported"
                        .into(),
                ));
            }
        }
    }
    Ok(target.unwrap_or(LegacyCodec::Rar29).writable_version())
}
/// Whole-archive repack of a solid RAR4 archive (ADR 0005 stage C): every
/// member is decoded in chain order through the shared window and
/// re-encoded into a fresh solid archive — same order, minus the deleted
/// members, with renames and each member's original compression level and
/// timestamp — then the comment and recovery record are applied
/// structurally and the result replaces the original atomically. Mirrors
/// WinRAR 7.21+'s full-archive repacking for solid RAR4 edits (the surgical
/// partial reprocess of 7.20 is not reproduced).
/// One surviving member of a solid repack: the metadata needed to re-emit it
/// (name, level, timestamps, comment), captured before the decode loop borrows
/// the archive mutably.
struct KeptMember {
    /// Index into the original catalog.
    index: usize,
    name: String,
    level: u8,
    mtime: u32,
    mtime_ns: u32,
    comment: Option<Vec<u8>>,
    /// On-disk DOS attribute byte to re-emit with the member.
    attr: u32,
}

/// Repack a solid RAR4 archive (ADR 0005 stage C): every member is decoded
/// in chain order and re-encoded into a fresh solid archive, then the
/// comment and recovery record are applied structurally and the result
/// replaces the original atomically. Mirrors WinRAR 7.21+'s full-archive
/// repacking. `additions` (used by the deferred solid-append path) are
/// emitted after the surviving members; `deleted`/`rename_map`/`comment`/
/// `force_rr` carry the editor transaction.
// One parameter per edit dimension the repack has to honour (deletes,
// renames, archive comment, recovery, additions, member comments).
#[allow(clippy::too_many_arguments)]
// The repack drives the legacy create facade on purpose: it needs the RAR4
// member writer (`add_rar4_data`) and the RAR4 comment queue
// (`set_rar4_writer_comment`), neither of which the typed `ArchiveWriter`
// exposes. Routing RAR4 writing through its own module is the breaking-release
// boundary work (audit P1), not a deprecation fix.
pub(crate) fn repack_solid_archive(
    archive: &mut RarArchive,
    deleted: &[bool],
    rename_map: &HashMap<usize, String>,
    comment: Option<&[u8]>,
    force_rr: Option<u8>,
    renamed: usize,
    additions: &[SolidAppendEntry],
    member_comments: &[(usize, Option<Vec<u8>>)],
) -> RarResult<EditSummary> {
    for idx in rename_map.keys() {
        if deleted[*idx] {
            return Err(RarError::InvalidOption(
                "cannot rename a member that the same edit deletes".into(),
            ));
        }
    }
    let deleted_count = deleted.iter().filter(|d| **d).count();
    // The fresh archive is written with the source's member generation so
    // the matching legacy encoder reproduces the chain (RAR 1.5 adaptive
    // Huffman, RAR 2.x LZSS+Huffman, RAR 3.x+). A solid chain cannot mix
    // codecs, so a mixed archive keeps the clear refusal.
    let target_version = solid_repack_version(archive)?;

    // The final comment text: the plan's value (empty removes), or the
    // archive's original comment preserved by the repack.
    let final_comment: Option<Vec<u8>> = match comment {
        Some([]) => None,
        Some(bytes) => Some(bytes.to_vec()),
        None => read_comment(archive)?,
    };
    // `-hp`: the fresh archive carries the same protection — the members are
    // re-encoded from their decrypted bytes, so both the data and the
    // headers are re-encrypted with the archive password. `-p` (without
    // `-hp`) must survive the repack too: the members are re-encoded from
    // decrypted bytes into an archive written with the same member password,
    // and a repack without the password would strip the encryption.
    let hp = archive_is_header_encrypted(archive)? || archive.header_encryption;
    let member_encrypted = archive
        .entries
        .iter()
        .any(|entry| entry.header.flags & crate::format::rar4::FHD_PASSWORD as u64 != 0);
    let password = if hp || member_encrypted {
        Some(
            header_password(archive)
                .ok_or_else(|| {
                    RarError::Encrypted(if hp {
                        "repacking a header-encrypted (-hp) RAR4 archive requires its password"
                            .into()
                    } else {
                        "repacking a member-encrypted (-p) RAR4 archive requires its password"
                            .into()
                    })
                })?
                .to_string(),
        )
    } else {
        None
    };
    // Recovery record strength: the explicit percent, or an approximation
    // of the original record's strength (the archive is a fresh whole, so
    // the record is rebuilt over it).
    let rr_percent: Option<u8> = if force_rr.is_some() {
        force_rr
    } else {
        let hp_bytes = if hp {
            header_password(archive).map(str::as_bytes)
        } else {
            None
        };
        match scan_protect_file(&archive.path, hp_bytes)?.protect {
            Some(protect) if &protect.mark == b"Protect+" => {
                let prefix_len = protect.block_offset.max(1) as u64;
                let percent =
                    ((u64::from(protect.rec_sectors) * 51_200) / prefix_len).clamp(1, 100);
                Some(percent as u8)
            }
            Some(_) => {
                return Err(RarError::Unsupported(
                    "RAR4: archives with a PROTECT_HEAD recovery record cannot be repacked in place; recreate the archive".into(),
                ));
            }
            None => None,
        }
    };

    // Keep-list with the emit metadata captured up front (name, level,
    // mtime, mtime_ns, comment) so the decode loop below can borrow the
    // archive mutably without aliasing its catalog.
    let mut kept: Vec<KeptMember> = Vec::new();
    for (i, entry) in archive.entries.iter().enumerate() {
        if !deleted[i] {
            let name = rename_map
                .get(&i)
                .cloned()
                .unwrap_or_else(|| entry.header.name.clone());
            // A queued SetMemberComment overrides the stored comment for the
            // solid repack (the non-solid path applies the same override in its
            // own rebuild loop below).
            let comment = member_comments
                .iter()
                .find(|(idx, _)| *idx == i)
                .map(|(_, c)| c.clone())
                .unwrap_or_else(|| entry.header.comment.clone());
            // The catalog stores legacy DOS times as local-civil seconds;
            // the writer re-packs from a Unix instant.
            let mtime = if entry.header.format_version == 4 {
                crate::format::shared::legacy_time::local_civil_to_epoch(entry.header.mtime)
            } else {
                entry.header.mtime
            };
            kept.push(KeptMember {
                index: i,
                name,
                level: entry.header.comp_method,
                mtime,
                mtime_ns: entry.header.mtime_ns.unwrap_or(0),
                comment,
                attr: (entry.header.attributes & 0xFF) as u32,
            });
        }
    }

    let tmp_path = temp_sibling_path(&archive.path);
    let repack = (|| -> RarResult<EditSummary> {
        // Decode every member in chain order (deleted ones included — their
        // compressed data references the shared window) and re-encode the
        // kept members into a fresh solid archive.
        {
            let mut writer = crate::archive::RarArchive::create_with_options(
                &tmp_path,
                crate::options::CreateOptions {
                    compression: target_version,
                    solid: true,
                    password: password.clone(),
                    encrypt_headers: hp,
                    ..Default::default()
                },
            )
            .map_err(|e| RarError::Format(format!("repack: create staged archive: {e:?}")))?;
            // The comment is emitted by the writer (it must precede every
            // member and has to be header-encrypted on a `-hp` archive).
            writer.set_rar4_writer_comment(final_comment.clone());
            for kept_member in &kept {
                archive.check_cancel()?;
                // Directory members are zero-byte placeholders: they contribute
                // nothing to the solid window (the decoder skips them), so they
                // are re-emitted with empty data rather than the decoded run.
                let data = if archive.entries[kept_member.index].is_dir() {
                    Vec::new()
                } else {
                    archive.rar4_decode_solid_through(kept_member.index)?
                };
                writer.add_rar4_data(
                    kept_member.name.clone(),
                    data,
                    kept_member.level,
                    kept_member.mtime,
                    kept_member.mtime_ns,
                    kept_member.comment.clone(),
                    Some(kept_member.attr),
                )?;
            }
            // Deferred solid-append additions continue the same fresh chain.
            for entry in additions {
                archive.check_cancel()?;
                writer.add_rar4_data(
                    entry.name.clone(),
                    entry.data.clone(),
                    entry.level,
                    entry.mtime,
                    entry.mtime_ns,
                    None,
                    Some(entry.attr),
                )?;
            }
            writer.close()?;
        }
        // The recovery record lands on the staged archive through the same
        // structural engine (it is header-level; the solid members are
        // untouched by it). The comment already came from the writer, so the
        // staged rewrite is only needed when a record has to be built.
        let summary = match rr_percent {
            Some(percent) => {
                let mut staged = match password.as_deref() {
                    Some(pw) => crate::archive::RarArchive::open_with_password(&tmp_path, pw),
                    None => crate::archive::RarArchive::open(&tmp_path),
                }
                .map_err(|e| RarError::Format(format!("repack: reopen staged archive: {e:?}")))?;
                edit_rar4(&mut staged, &[], &[], None, Some(percent), &[])?
            }
            None => EditSummary {
                deleted: 0,
                renamed: 0,
            },
        };
        Ok(summary)
    })();

    match repack {
        Ok(mut summary) => {
            install_durable(&tmp_path, &archive.path)?;
            summary.deleted = deleted_count;
            summary.renamed = renamed;
            archive.open_read()?;
            Ok(summary)
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp_path);
            Err(error)
        }
    }
}
