//! Archive and per-member comments.

use crate::args::{ArchiveArgs, CommentArgs, FileCommentArgs};
use crate::edit::open_editor;
use crate::error::CliResult;
use crate::info;
/// Set the archive comment (like `rar c`), from stdin or `-z<file>`;
/// empty input removes the comment.
pub(crate) fn cmd_comment_set(args: &CommentArgs) -> CliResult<()> {
    use std::io::Read;
    let mut comment = Vec::new();
    if let Some(file) = &args.comment_file {
        std::fs::File::open(file)
            .and_then(|mut f| f.read_to_end(&mut comment))
            .map_err(|e| format!("read comment file {file}: {e}"))?;
    } else {
        std::io::stdin()
            .read_to_end(&mut comment)
            .map_err(|e| format!("stdin: {e}"))?;
    }
    let mut editor = open_editor(&args.archive, args.password.password.as_deref())?;
    let remove = comment.is_empty();
    editor
        .apply(rar_rs::EditPlan::new().set_comment(comment))
        .map_err(|e| format!("comment: {e}"))?;
    if remove {
        info!("Comment removed from {archive}", archive = args.archive);
    } else {
        info!("Comment added to {archive}", archive = args.archive);
    }
    Ok(())
}

/// Set a member's file comment (like `rar cf`), from stdin or `-z<file>`;
/// empty input removes the member's comment. RAR 1.5–4.x only (RAR5 has no
/// per-member comment block).
pub(crate) fn cmd_file_comment_set(args: &FileCommentArgs) -> CliResult<()> {
    use std::io::Read;
    let mut comment = Vec::new();
    if let Some(file) = &args.comment_file {
        std::fs::File::open(file)
            .and_then(|mut f| f.read_to_end(&mut comment))
            .map_err(|e| format!("read comment file {file}: {e}"))?;
    } else {
        std::io::stdin()
            .read_to_end(&mut comment)
            .map_err(|e| format!("stdin: {e}"))?;
    }
    let mut editor = open_editor(&args.archive, args.password.password.as_deref())?;
    let id = editor
        .unique_entry(&args.member)
        .map_err(|e| format!("cf: {}: {e}", args.member))?;
    let remove = comment.is_empty();
    editor
        .apply(rar_rs::EditPlan::new().set_member_comment(id, comment))
        .map_err(|e| format!("cf: {e}"))?;
    if remove {
        info!(
            "Comment removed from {member} in {archive}",
            member = args.member,
            archive = args.archive
        );
    } else {
        info!(
            "Comment added to {member} in {archive}",
            member = args.member,
            archive = args.archive
        );
    }
    Ok(())
}

/// Write the archive comment to stdout (like `rar cw`).
pub(crate) fn cmd_comment_write(args: &ArchiveArgs) -> CliResult<()> {
    let mut options = rar_rs::OpenOptions::new();
    if let Some(pw) = &args.password.password {
        options = options.password(pw);
    }
    let mut rar = rar_rs::ArchiveReader::open_with(&args.archive, options)
        .map_err(|e| format!("open: {e}"))?;
    if let Some(comment) = rar.comment().map_err(|e| format!("cw: {e}"))? {
        use std::io::Write;
        std::io::stdout()
            .write_all(&comment)
            .map_err(|e| format!("stdout: {e}"))?;
    }
    Ok(())
}
