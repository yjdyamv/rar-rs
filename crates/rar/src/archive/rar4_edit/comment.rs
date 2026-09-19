//! Archive comment (`CMT` NEWSUB block) lookup inside an open archive.
//!
//! The block's wire format lives in [`crate::format::rar4::comment`]; what
//! stays here is the part that needs the engine: which volume holds the
//! block, how a `-hp` header is decrypted, and the metadata limit the
//! payload size is checked against.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::archive::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar4::comment::{
    comment_block_name_is_cmt, decode_comment_payload, decode_comment_stream,
};
use crate::format::rar4::{
    ENDARC_HEAD, EnvelopePolicy, MAIN_HEAD, MHD_PASSWORD, NEWSUB_HEAD, RAR4_METHOD_STORE,
    read_block,
};

use super::layout::{first_volume, first_volume_signature_offset, header_password, main_flags};

/// Read the archive comment (`rar cw`): locate the NEWSUB `CMT` block in
/// the set's first volume and decode its payload. Returns `None` when the
/// archive has no comment. On a `-hp` archive the block's header is
/// decrypted with the archive password first (only the header is encrypted;
/// the payload is not).
pub(crate) fn read_comment(archive: &RarArchive) -> RarResult<Option<Vec<u8>>> {
    // The comment sits in the first volume (WinRAR's placement), which is
    // not necessarily the part that was opened.
    let path = first_volume(archive);
    let sfx_offset = first_volume_signature_offset(archive)?;
    let mut file = File::open(path).map_err(RarError::Io)?;
    let file_len = file.metadata().map_err(RarError::Io)?.len();
    let mut pos = sfx_offset as u64 + 7;
    let mut saw_main = false;
    let mut hp: Option<&[u8]> = None;
    while pos < file_len {
        let Some(view) = read_block(&mut file, hp.is_some(), hp, EnvelopePolicy::PLAN)? else {
            break;
        };
        if view.head_type == MAIN_HEAD && !saw_main {
            saw_main = true;
            if main_flags(&view.header)? & MHD_PASSWORD != 0 {
                let password = header_password(archive).ok_or_else(|| {
                    RarError::Encrypted(
                        "reading the comment of a header-encrypted (-hp) RAR4 archive requires its password".into(),
                    )
                })?;
                hp = Some(password.as_bytes());
            }
        } else if view.head_type == NEWSUB_HEAD
            && view.header.len() >= 32
            && comment_block_name_is_cmt(&view.header)
        {
            let method = view.header[25];
            let unp = u32::from_le_bytes(view.header[11..15].try_into().unwrap());
            let unicode = u32::from_le_bytes(view.header[28..32].try_into().unwrap()) & 1 != 0;
            let payload = if method == RAR4_METHOD_STORE {
                // The declared size comes from the header alone (a hand-made
                // archive can pass the header CRC with any size), so cap it
                // before it can drive an allocation, like the RAR5 comment
                // reader does.
                let limit = archive.read_ctx().extract_options.metadata_limit();
                if view.add_size > limit {
                    return Err(RarError::LimitExceeded {
                        limit,
                        context: format!("RAR4 archive comment declares {} bytes", view.add_size),
                    });
                }
                let data_len =
                    usize::try_from(view.add_size).map_err(|_| RarError::LimitExceeded {
                        limit,
                        context: "RAR4: comment size does not fit in usize".into(),
                    })?;
                let mut data = vec![0u8; data_len];
                file.seek(SeekFrom::Start(view.data_offset()))
                    .map_err(RarError::Io)?;
                file.read_exact(&mut data).map_err(RarError::Io)?;
                data
            } else {
                decode_comment_stream(
                    &mut file,
                    view.data_offset(),
                    view.add_size,
                    method,
                    unp as usize,
                )?
            };
            return Ok(Some(decode_comment_payload(&payload, unicode)));
        }
        if view.head_type == ENDARC_HEAD {
            break;
        }
        pos = view.end();
    }
    Ok(None)
}
