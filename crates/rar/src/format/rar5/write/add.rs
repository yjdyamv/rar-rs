//! RAR5 member addition: file/bytes entry points, directory headers, the
//! `-ts`/`-ow` extra-record builders and the redirect writer.
//!
//! The format-neutral dispatchers live in
//! `crate::format::shared::write_ops`; emission lives in [`super::emit`].

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::layout::{
    SAMPLE_PROBE_HEAD, dict_params_for, hash_file, sample_is_incompressible,
    sample_is_incompressible_stream,
};
use crate::codec::lzss_huff;
use crate::engine::Engine;
use crate::engine::MemberPlan;
use crate::engine::{ArchiveEntry, Mode, STREAM_COMPRESS_THRESHOLD};
use crate::error::{RarError, RarResult};
#[cfg(unix)]
use crate::format::rar5::headers::build_owner_extra_record;
use crate::format::rar5::headers::{
    file_time_extra_record, file_time_extra_record_windows, redirect_extra_bytes,
};
use crate::format::rar5::{
    COMP_METHOD_STORE, FILE_FLAG_CRC32, FILE_FLAG_DIRECTORY, FILE_FLAG_TIME_UNIX, level_to_method,
};
use crate::format::shared::write_ops::archive_name_from_path;
use crate::model::FileHeader;

#[cfg(windows)]
use super::windows;

/// Build the FILE_TIME extra record per explicit `-ts` settings (the
/// off-thread parallel batch path has no engine); `None` when no
/// time needs the extra record. On Windows the access/creation times are
/// read through `GetFileTime` (std exposes no access-time API).
#[allow(clippy::too_many_arguments)]
pub(super) fn time_extra_cfg(
    save_ctime: bool,
    save_atime: bool,
    save_mtime: bool,
    precision_seconds: bool,
    meta: &fs::Metadata,
    _path: &Path,
    mtime: u32,
    mtime_ns: u32,
) -> Option<Vec<u8>> {
    // `-ts1` drops the fractional second everywhere it is read — the header's
    // mtime below as well as the unix/windows ctime/atime reads — so this
    // normalizer must exist on every target, wasm included.
    let ns = |v: u32| if precision_seconds { 0 } else { v };
    let ctime = if save_ctime {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some((meta.ctime() as u64, ns(meta.ctime_nsec() as u32)))
        }
        #[cfg(windows)]
        {
            let _ = meta;
            windows::windows_file_time(_path, false).map(|(s, n)| (s, ns(n)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = meta;
            let _ = _path;
            None
        }
    } else {
        None
    };
    let atime = if save_atime {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some((meta.atime() as u64, ns(meta.atime_nsec() as u32)))
        }
        #[cfg(windows)]
        {
            let _ = meta;
            windows::windows_file_time(_path, true).map(|(s, n)| (s, ns(n)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    } else {
        None
    };
    let mtime = save_mtime.then_some((mtime as u64, ns(mtime_ns)));
    let present = mtime.is_some() || ctime.is_some() || atime.is_some();
    // Windows keeps its times in the FILETIME form unless the caller asked for
    // whole seconds (`-ts1`), which WinRAR stores in the Unix form.
    let windows_form = crate::platform::file_time_is_windows() && !precision_seconds;
    // On Windows the record is always the time carrier. On Unix the header's
    // 4-byte mtime already holds a whole-second modification time, so WinRAR
    // writes no record for it and only adds one to carry what the header
    // cannot: a fractional second, or the creation/access times (measured
    // against WinRAR 7.23).
    let needs_record = crate::platform::file_time_is_windows()
        || ctime.is_some()
        || atime.is_some()
        || mtime.is_some_and(|(_, sub)| sub != 0);
    (present && needs_record).then(|| {
        if windows_form {
            file_time_extra_record_windows(mtime, ctime, atime)
        } else {
            file_time_extra_record(mtime, ctime, atime)
        }
    })
}

/// Build the OWNER extra record (numeric uid/gid) per `-ow`; `None`
/// off-Unix or when disabled. Off-thread variant of `owner_extra_for`.
pub(super) fn owner_extra_cfg(save_owner: bool, meta: &fs::Metadata) -> Option<Vec<u8>> {
    if !save_owner {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(build_owner_extra_record(
            &meta.uid().to_string(),
            &meta.gid().to_string(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Build the FILE_TIME extra record for `meta`, per the current
/// `-ts` settings; `None` when no time needs the extra record.
fn time_extra_for(
    cx: &dyn Engine,
    meta: &fs::Metadata,
    path: &Path,
    mtime: u32,
    mtime_ns: u32,
) -> Option<Vec<u8>> {
    time_extra_cfg(
        cx.write_ctx().meta.ctime,
        cx.write_ctx().meta.atime,
        cx.write_ctx().meta.mtime,
        cx.write_ctx().meta.time_precision_seconds,
        meta,
        path,
        mtime,
        mtime_ns,
    )
}

/// The FILE_TIME record for a member whose only time is `mtime` seconds (a
/// raw-bytes / `-si` member, which has no filesystem metadata), in the
/// running platform's form. `None` when the platform stores the time in the
/// header itself (Unix) or when times are switched off — on Windows the
/// header carries no mtime, so the record is the only carrier.
pub(super) fn mtime_record(cx: &dyn Engine, mtime: u32) -> Option<Vec<u8>> {
    if !crate::platform::file_time_is_windows() || !cx.write_ctx().meta.mtime || mtime == 0 {
        return None;
    }
    let time = Some((u64::from(mtime), 0u32));
    Some(if cx.write_ctx().meta.time_precision_seconds {
        file_time_extra_record(time, None, None)
    } else {
        file_time_extra_record_windows(time, None, None)
    })
}

/// Build the OWNER extra record (numeric uid/gid) when `-ow` is on;
/// `None` off-Unix or when disabled.
fn owner_extra_for(cx: &dyn Engine, meta: &fs::Metadata) -> Option<Vec<u8>> {
    owner_extra_cfg(cx.write_ctx().meta.owner, meta)
}

/// Clamp a member's dictionary to the current RAR5 solid chain's window.
///
/// The reader builds one shared `DecoderState` from the chain head's
/// declared dictionary, so the window never grows across a solid run
/// (WinRAR's archiver guarantees the same). A later member whose own
/// size selects a larger dictionary would let the encoder emit
/// distances that window cannot resolve, so it inherits the chain-start
/// parameters instead. A chain head (or a non-solid member) records its
/// own selection as the window for the run.
fn solid_dict_params(cx: &mut dyn Engine, dsl: u8, dict_bytes: Option<u64>) -> (u8, Option<u64>) {
    if !cx.write_ctx().solid.mode {
        return (dsl, dict_bytes);
    }
    if cx.write_ctx().solid.encoder_state.is_none() {
        cx.write_ctx_mut().solid.chain_dict = Some((dsl, dict_bytes));
        return (dsl, dict_bytes);
    }
    let Some((head_dsl, head_bytes)) = cx.write_ctx().solid.chain_dict else {
        // Chain state seeded by a path that did not record a head
        // (surgical rewrite): leave the member's own selection.
        return (dsl, dict_bytes);
    };
    let window = |log: u8, bytes: Option<u64>| bytes.unwrap_or((128u64 * 1024) << log);
    if window(dsl, dict_bytes) > window(head_dsl, head_bytes) {
        (head_dsl, head_bytes)
    } else {
        (dsl, dict_bytes)
    }
}

/// Try the compression filters under the `-mc` policy, returning the
/// packed bytes of the chosen filter — or `None` when plain LZSS should
/// handle the member.
fn try_auto_filters(
    cx: &dyn Engine,
    data: &[u8],
    method: u8,
    dsl: u8,
    dict_bytes: Option<u64>,
) -> RarResult<Option<Vec<u8>>> {
    let variant = crate::version::ArchiveVersion::from_v70(dict_bytes.is_some());
    let threads = cx.effective_threads();
    let cancel = cx.cancel_token();
    super::filter_policy::encode_with_filter_policy(
        data,
        method,
        dsl,
        variant,
        cx.write_ctx().compression.filters,
        threads,
        cancel.as_deref(),
    )
}

/// RAR5 filesystem-file member writer (the neutral dispatcher lives in
/// `format::shared::write_ops`).
pub(crate) fn add_file_rar5(
    cx: &mut dyn Engine,
    path: &Path,
    arcname: Option<&str>,
    level: u8,
) -> RarResult<()> {
    cx.check_cancel()?;
    let meta = fs::metadata(path)?;
    if !meta.is_file() {
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a file: {}", path.display()),
        )));
    }
    let file_size = meta.len();
    let mtime = meta
        .modified()
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;
    let mtime_ns = meta
        .modified()
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let time_extra = time_extra_for(cx, &meta, path, mtime, mtime_ns);
    let owner_extra = owner_extra_for(cx, &meta);

    let attrs = crate::platform::file_attributes(&meta);

    let name = match arcname {
        Some(s) => s.to_string(),
        None => archive_name_from_path(path)?,
    };
    let name = name.replace('\\', "/");

    if cx.progress_slot().is_some() {
        cx.report_progress(0, file_size);
    }

    let method = level_to_method(level);
    let probe_incompressible = method != COMP_METHOD_STORE
        && file_size >= (SAMPLE_PROBE_HEAD as u64) * 4
        && sample_is_incompressible_stream(&mut fs::File::open(path)?, file_size, method)?;
    let (dsl, dict_bytes) = dict_params_for(
        file_size as usize,
        cx.write_ctx().compression.dict_size_log,
        cx.write_ctx().compression.dict_size_bytes,
        method,
        cx.write_ctx().compression.force_v70,
    );

    if method == COMP_METHOD_STORE || probe_incompressible {
        // STORE is written by streaming the file directly: bounded
        // memory regardless of file size. Encrypted STORE is encrypted
        // on the fly with a chained CBC state (also bounded memory).
        crate::format::shared::write_ops::reset_solid_chain(cx);
        let (plain_crc, plain_blake) = hash_file(
            path,
            file_size,
            cx.write_ctx().meta.blake2,
            cx.cancel_flag(),
        )?;
        let (header_crc, extra_data, stored_hash, encr) =
            super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
        let mut plan = MemberPlan {
            name: name.clone(),
            unpacked_size: file_size,
            file_crc: header_crc,
            method: COMP_METHOD_STORE,
            dict_size_log: 0,
            dict_size_bytes: dict_bytes,
            extra_data,
            attrs,
            mtime,
            solid: false,
            stored_hash,
        };
        plan.push_extra(time_extra.as_deref(), owner_extra.as_deref());
        super::stream::write_store_member(cx, path, plan, encr.as_ref())?;
        super::stream::write_member_streams(cx, path)?;
        cx.report_progress(file_size, file_size);
        return Ok(());
    }

    // Compressed path: files at or above the streaming threshold are
    // compressed in bounded chunks to a temporary spill file and then
    // streamed into the archive (bounded memory for any file size);
    // smaller files are compressed in memory.
    if file_size >= STREAM_COMPRESS_THRESHOLD {
        // The streaming path also applies the `-se` extension reset;
        // run it before clamping so a fresh chain head keeps its own
        // dictionary (the later call inside is then a no-op).
        crate::format::shared::write_ops::maybe_reset_solid_for_extension(cx, &name);
        let (dsl, dict_bytes) = solid_dict_params(cx, dsl, dict_bytes);
        return super::stream::add_file_streaming(
            cx,
            path,
            &name,
            file_size,
            attrs,
            mtime,
            time_extra,
            owner_extra,
            method,
            dsl,
            dict_bytes,
        );
    }

    // Compressed path: read the member whole (bounded by the streaming
    // threshold), hash it, and try the automatic x86 (E8/E8E9) filter
    // first — WinRAR applies it to x86 code and it is worth several
    // percent on real binaries. A filtered member is written standalone
    // (non-solid): the decoder's window holds transformed bytes and the
    // filter positions are member-relative, so it cannot share the LZ
    // window with its neighbours.
    let mut whole = Vec::with_capacity(file_size as usize);
    {
        let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
        file.read_to_end(&mut whole)?;
    }
    if whole.len() as u64 != file_size {
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "file changed size while being archived: expected {file_size} bytes, read {}",
                whole.len()
            ),
        )));
    }
    let mut crc_hasher = crc32fast::Hasher::new();
    let mut blake_hasher = if cx.write_ctx().meta.blake2 {
        Some(crate::format::rar5::blake2sp::Hasher::new())
    } else {
        None
    };
    crc_hasher.update(&whole);
    if let Some(h) = blake_hasher.as_mut() {
        h.update(&whole);
    }
    let plain_crc = crc_hasher.finalize();
    let plain_blake = blake_hasher.map(|h| h.finalize());

    // WinRAR `-se`: reset the solid statistics when the extension changes.
    crate::format::shared::write_ops::maybe_reset_solid_for_extension(cx, &name);
    let (dsl, dict_bytes) = solid_dict_params(cx, dsl, dict_bytes);
    let chain_solid = cx.begin_solid_member();

    // Try the automatic delta (multimedia) filter first, then the x86
    // (E8/E8E9) filter. Ordering matters: real x86 code is not
    // multi-channel-correlated, so the cheap delta scan returns `None`
    // immediately and we fall through to x86; for correlated audio/raw
    // data the delta filter wins outright, so we never pay for a useless
    // x86 scan. Each filter is only kept when it strictly beats plain
    // LZSS (the encoder compares against an unfiltered pack), so neither
    // can steal a member from the better transform or from plain LZSS.
    // The caller's `< file_size` guard only accepts a filter when it also
    // beats STORE.
    //
    // A filtered member cannot share the LZ window (see above), so a
    // solid archive never tries one: the first member would leave the
    // chain immediately and every later member would follow it, leaving
    // `-s` to buy nothing.
    let filtered = if cx.write_ctx().solid.mode {
        None
    } else {
        try_auto_filters(cx, &whole, method, dsl, dict_bytes)?
    };
    if let Some(filtered) = filtered
        && (filtered.len() as u64) < file_size
    {
        crate::format::shared::write_ops::reset_solid_chain(cx);
        let (header_crc, extra_data, stored_hash, encr) =
            super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
        let mut plan = MemberPlan {
            name: name.clone(),
            unpacked_size: file_size,
            file_crc: header_crc,
            method,
            dict_size_log: dsl,
            dict_size_bytes: dict_bytes,
            extra_data,
            attrs,
            mtime,
            solid: false,
            stored_hash,
        };
        plan.push_extra(time_extra.as_deref(), owner_extra.as_deref());
        let packed_data = super::emit::encrypt_payload_with(encr.as_ref(), &filtered);
        super::emit::write_file_entry(cx, &plan, &packed_data)?;
        super::stream::write_member_streams(cx, path)?;
        cx.report_progress(file_size, file_size);
        return Ok(());
    }

    // Unfiltered path: compress in bounded chunks with a persistent
    // encoder state (solid archives share the LZ window; non-solid
    // members keep one window within the member, reset between
    // members). The persistent state also carries the long-range match
    // history across chunk boundaries (WinRAR's `-mcl` long range
    // search).
    // Mid-size members (>= MT_MIN) get the same windowed MT encode as
    // the streaming path, matching WinRAR's per-file parallelization;
    // the measured ratio divergence from the sequential chunk loop on
    // the corpus is within ±0.3% (the repeat-distance cache resets per
    // slice). Solid chains take it too: the window still carries over
    // through the shared tail and long-range table, so only the parse
    // tier differs from the sequential chain — the same documented MT
    // divergence, now visible inside a chain as well. Filter members
    // stay sequential (the transform runs over the whole buffer).
    //
    // One chunk (4 MiB), not three: a member *without* a filter took the
    // sequential path below this, so `-mt` silently did nothing for text
    // and other unfiltered members in the 4-12 MiB band — very common
    // sizes (measured: a 4.88 MiB source tree encoded identically at
    // `--threads 1` and `8`). Filtered members already parallelize at any
    // size through `encode_with_filters_mt`, which is why the band only
    // looked covered. The batch and streaming gates keep their own
    // thresholds: those run inside a pool wave (nested MT) or only ever
    // see >= 64 MiB members.
    #[cfg(feature = "parallel")]
    const MT_MIN: usize = crate::codec::DEFAULT_CHUNK_SIZE;
    #[cfg(feature = "parallel")]
    let threads = cx.effective_threads();
    #[cfg(not(feature = "parallel"))]
    let threads = 1usize;
    let mut packed = Vec::new();
    let use_mt = {
        #[cfg(feature = "parallel")]
        {
            threads > 1 && whole.len() >= MT_MIN
        }
        #[cfg(not(feature = "parallel"))]
        {
            let _ = threads;
            false
        }
    };
    if use_mt {
        let state = cx
            .write_ctx_mut()
            .solid
            .encoder_state
            .as_mut()
            .expect("encoder state seeded");
        packed = crate::codec::lzss_huff::encode_chunked_mt(
            &whole,
            method,
            dsl,
            crate::codec::DEFAULT_CHUNK_SIZE,
            state,
            threads,
            true,
            crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
        );
        cx.report_progress(file_size, file_size);
    } else {
        let mut bytes_read = 0u64;
        for chunk in whole.chunks(crate::codec::DEFAULT_CHUNK_SIZE) {
            cx.check_cancel()?;
            bytes_read += chunk.len() as u64;
            let state = cx.write_ctx_mut().solid.encoder_state.as_mut();
            let compressed = lzss_huff::encode_chunked(
                chunk,
                lzss_huff::EncodeOptions {
                    chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                    state,
                    is_final: bytes_read >= whole.len() as u64,
                    variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                    // `add_path` already probed the file before reading
                    // it; re-probing here would sample-encode every
                    // member twice.
                    skip_incompressible_probe: true,
                    ..lzss_huff::EncodeOptions::new(method, dsl)
                },
            )?;
            packed.extend(compressed);
            cx.report_progress(bytes_read, file_size);
            if packed.len() as u64 >= file_size {
                break;
            }
        }
    }

    if packed.len() as u64 >= file_size {
        // Compression is a net loss: fall back to streaming STORE.
        crate::format::shared::write_ops::reset_solid_chain(cx);
        let (header_crc, extra_data, stored_hash, encr) =
            super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
        let mut plan = MemberPlan {
            name: name.clone(),
            unpacked_size: file_size,
            file_crc: header_crc,
            method: COMP_METHOD_STORE,
            dict_size_log: 0,
            dict_size_bytes: dict_bytes,
            extra_data,
            attrs,
            mtime,
            solid: false,
            stored_hash,
        };
        plan.push_extra(time_extra.as_deref(), owner_extra.as_deref());
        super::stream::write_store_member(cx, path, plan, encr.as_ref())?;
        super::stream::write_member_streams(cx, path)?;
        cx.report_progress(file_size, file_size);
        return Ok(());
    }

    let (header_crc, extra_data, stored_hash, encr) =
        super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
    let mut plan = MemberPlan {
        name: name.clone(),
        unpacked_size: file_size,
        file_crc: header_crc,
        method,
        dict_size_log: dsl,
        dict_size_bytes: dict_bytes,
        extra_data,
        attrs,
        mtime,
        solid: chain_solid,
        stored_hash,
    };
    plan.push_extra(time_extra.as_deref(), owner_extra.as_deref());
    let packed_data = super::emit::encrypt_payload_with(encr.as_ref(), &packed);
    super::emit::write_file_entry(cx, &plan, &packed_data)?;
    super::stream::write_member_streams(cx, path)?;
    // Non-solid members use an independent LZ window: drop the
    // encoder state so the next member starts fresh.
    if !cx.write_ctx().solid.mode {
        crate::format::shared::write_ops::reset_solid_chain(cx);
    }

    cx.report_progress(file_size, file_size);

    Ok(())
}

/// Roll to a fresh volume until `added` further bytes fit; `added` is what the
/// operation will append to the volume (its header, plus any payload). The
/// end-of-archive block and this volume's inline recovery record are reserved
/// on top, the latter sized from the prefix it protects. No-op for
/// single-volume archives, and a volume too small for one header errors
/// instead of rolling forever (matching the file-member splitter).
pub(super) fn ensure_rar5_volume_space(cx: &mut dyn Engine, added: u64) -> RarResult<()> {
    let Some(volume_size) = cx.write_ctx().output.volume_size else {
        return Ok(());
    };
    let mut rolled = false;
    loop {
        let used = cx.bytes_written();
        let needed = super::emit::volume_tail_reserve(cx, used + added) + added;
        if volume_size.saturating_sub(used) >= needed {
            return Ok(());
        }
        if rolled {
            return Err(RarError::invalid_option(format!(
                "volume size {volume_size} is too small for a RAR5 member header"
            )));
        }
        cx.start_next_volume()?;
        rolled = true;
    }
}

/// Effective header time and file flags for a RAR5 member: `-tsm-`
/// (`save_mtime == false`) omits the time field entirely, like WinRAR, and so
/// does an all-zero time — the "unknown" sentinel the reader renders as
/// `????-??-??` (e.g. a redirect created without one).
///
/// The 4-byte Unix mtime field and the FILE_TIME record are never both
/// present. Windows never uses the header field — WinRAR clears
/// `FILE_FLAG_TIME_UNIX` and keeps the times in the FILE_TIME record (as
/// FILETIME, unless `-ts1` asks for whole seconds) — and on Unix the header
/// field is used only when there is no record, i.e. a whole-second
/// modification time with no creation/access time (`has_time_record`; see
/// `time_extra_cfg`). This mirrors WinRAR 7.23 byte-for-byte.
pub(crate) fn rar5_time_fields(
    cx: &dyn Engine,
    mtime: u32,
    flags: u64,
    has_time_record: bool,
) -> (u32, u64) {
    if cx.write_ctx().meta.mtime
        && mtime != 0
        && !crate::platform::file_time_is_windows()
        && !has_time_record
    {
        (mtime, flags)
    } else {
        (0, flags & !FILE_FLAG_TIME_UNIX)
    }
}

/// The entry carries no data; `redir_type` is 1 (Unix symlink),
/// 2 (Windows symlink), 3 (Windows junction), 4 (hardlink) or
/// 5 (file copy) and `target` is the referenced member name.
pub(crate) fn add_redirect(
    cx: &mut dyn Engine,
    name: &str,
    redir_type: u64,
    target: &str,
) -> RarResult<()> {
    add_redirect_with_time(cx, name, redir_type, target, 0, None)
}

/// Like [`add_redirect`], carrying the link's modification time
/// (WinRAR stores it like a regular member's, including the FILE_TIME
/// extra record).
pub(crate) fn add_redirect_with_time(
    cx: &mut dyn Engine,
    name: &str,
    redir_type: u64,
    target: &str,
    mtime: u32,
    mtime_ns: Option<u32>,
) -> RarResult<()> {
    if cx.is_rar4() {
        return Err(RarError::unsupported(
            "redirect members are not supported for RAR4 archives",
        ));
    }
    if cx.is_rar13() {
        return Err(RarError::unsupported(
            "redirect members are not supported for RAR 1.3/1.4 archives",
        ));
    }
    if cx.mode() != Mode::Write && cx.mode() != Mode::Append {
        return Err(RarError::format(
            "add_redirect requires an archive being written",
        ));
    }
    crate::format::shared::write_ops::reset_solid_chain(cx);
    // Redirect type 3: a Windows junction (always a directory).
    const REDIR_WINDOWS_JUNCTION: u64 = 0x03;
    // A junction is a directory redirect; WinRAR flags it as one and drops the
    // (meaningless) CRC32, while a file symlink / hardlink keeps the CRC32.
    let is_directory = redir_type == REDIR_WINDOWS_JUNCTION;
    // `-tsm-` omits the link's time like a regular member. The record carries
    // the time only when the header cannot: always on Windows, and on Unix
    // only for a fractional second (`rar5_time_fields`).
    let precision_seconds = cx.write_ctx().meta.time_precision_seconds;
    let mtime_sub = mtime_ns.unwrap_or(0);
    let has_time_record = cx.write_ctx().meta.mtime
        && mtime != 0
        && (crate::platform::file_time_is_windows() || (!precision_seconds && mtime_sub != 0));
    let mut extra_data = if has_time_record {
        let time = Some((u64::from(mtime), mtime_sub));
        if crate::platform::file_time_is_windows() && !precision_seconds {
            file_time_extra_record_windows(time, None, None)
        } else {
            file_time_extra_record(time, None, None)
        }
    } else {
        Vec::new()
    };
    extra_data.extend_from_slice(&redirect_extra_bytes(redir_type, target));
    let (mtime, file_flags) = rar5_time_fields(
        cx,
        mtime,
        FILE_FLAG_TIME_UNIX
            | if is_directory {
                FILE_FLAG_DIRECTORY
            } else {
                FILE_FLAG_CRC32
            },
        has_time_record,
    );
    let fh = FileHeader {
        name: name.replace('\\', "/"),
        unpacked_size: 0,
        packed_size: 0,
        attributes: crate::platform::redirect_attributes(redir_type),
        crc32_val: (!is_directory).then_some(0),
        mtime,
        is_directory,
        host_os: crate::platform::host_os(),
        file_flags,
        extra_data,
        ..Default::default()
    };
    let hdr_bytes = fh.to_bytes();
    let hdr_on_disk = cx.on_disk_header_len(hdr_bytes.len() as u64);
    ensure_rar5_volume_space(cx, hdr_on_disk)?;
    cx.record_quick_open_entry(&hdr_bytes)?;
    cx.write_block_header(&hdr_bytes)?;
    cx.add_bytes_written(hdr_on_disk);
    cx.push_entry(ArchiveEntry {
        header: fh,
        chunks: Vec::new(),
    });
    Ok(())
}

/// Write one RAR5 directory header (FILE_HEAD with the directory flag).
/// Shared by the neutral `add_directory_only` / `add_directory`
/// dispatchers in `format::shared::write_ops`.
pub(crate) fn write_rar5_dir_entry(
    cx: &mut dyn Engine,
    name: &str,
    meta: &fs::Metadata,
    mtime: u32,
) -> RarResult<()> {
    let attrs = crate::platform::directory_attributes(meta);

    let (mtime, file_flags) =
        rar5_time_fields(cx, mtime, FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY, false);
    let fh = FileHeader {
        name: format!("{name}/"),
        attributes: attrs,
        mtime,
        host_os: crate::platform::host_os(),
        file_flags,
        is_directory: true,
        ..Default::default()
    };

    let hdr_bytes = fh.to_bytes();
    let hdr_on_disk = cx.on_disk_header_len(hdr_bytes.len() as u64);
    ensure_rar5_volume_space(cx, hdr_on_disk)?;
    cx.record_quick_open_entry(&hdr_bytes)?;
    cx.write_block_header(&hdr_bytes)?;
    cx.add_bytes_written(hdr_on_disk);
    cx.push_entry(ArchiveEntry {
        header: fh,
        chunks: Vec::new(),
    });
    Ok(())
}

/// RAR5 raw-bytes member writer (the neutral `add_bytes` dispatcher
/// lives in `format::shared::write_ops`).
pub(crate) fn add_bytes_rar5(
    cx: &mut dyn Engine,
    arcname: &str,
    data: &[u8],
    compression_level: u8,
) -> RarResult<()> {
    let name = arcname.replace('\\', "/");
    let plain_crc = {
        let mut h = crc32fast::Hasher::new();
        h.update(data);
        h.finalize()
    };
    let plain_blake = if cx.write_ctx().meta.blake2 {
        Some(crate::format::rar5::blake2sp::hash(data))
    } else {
        None
    };
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;
    let time_extra = mtime_record(cx, mtime);

    let method = level_to_method(compression_level);
    cx.report_progress(0, data.len() as u64);
    if method == COMP_METHOD_STORE || sample_is_incompressible(data, method) {
        crate::format::shared::write_ops::reset_solid_chain(cx);
        let (header_crc, extra_data, stored_hash, encr) =
            super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
        let packed_data = super::emit::encrypt_payload_with(encr.as_ref(), data);
        let mut plan = MemberPlan {
            name: name.clone(),
            unpacked_size: data.len() as u64,
            file_crc: header_crc,
            method: COMP_METHOD_STORE,
            dict_size_log: 0,
            dict_size_bytes: None,
            extra_data,
            attrs: crate::platform::memory_attributes(),
            mtime,
            solid: false,
            stored_hash,
        };
        plan.push_extra(time_extra.as_deref(), None);
        super::emit::write_file_entry(cx, &plan, &packed_data)?;
    } else {
        let (dsl, dict_bytes) = dict_params_for(
            data.len(),
            cx.write_ctx().compression.dict_size_log,
            cx.write_ctx().compression.dict_size_bytes,
            method,
            cx.write_ctx().compression.force_v70,
        );
        // WinRAR `-se`: reset the solid statistics when the extension changes.
        crate::format::shared::write_ops::maybe_reset_solid_for_extension(cx, &name);
        let (dsl, dict_bytes) = solid_dict_params(cx, dsl, dict_bytes);
        let chain_solid = cx.write_ctx().solid.mode && cx.write_ctx().solid.encoder_state.is_some();
        if cx.write_ctx().solid.mode {
            let state = cx
                .write_ctx_mut()
                .solid
                .encoder_state
                .get_or_insert_with(Default::default);
            // Each member starts its own frame; see
            // `EncoderState::begin_member`.
            state.begin_member();
        }
        let (member, shared) = match cx.progress_slot() {
            Some((tracker, member)) => (member, Some(tracker)),
            None => (0, None),
        };
        let mut cb = move |done: u64, total: u64| {
            if let Some(shared) = &shared {
                shared
                    .lock()
                    .expect("progress lock")
                    .report(member, done, total);
            }
        };
        let progress: Option<&mut dyn FnMut(u64, u64)> = Some(&mut cb);
        let packed = lzss_huff::encode_chunked(
            data,
            lzss_huff::EncodeOptions {
                chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                state: cx.write_ctx_mut().solid.encoder_state.as_mut(),
                is_final: true,
                variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                progress,
                // Already probed above (`sample_is_incompressible`).
                skip_incompressible_probe: true,
                ..lzss_huff::EncodeOptions::new(method, dsl)
            },
        )?;
        if packed.len() >= data.len() {
            crate::format::shared::write_ops::reset_solid_chain(cx);
            let (header_crc, extra_data, stored_hash, encr) =
                super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
            let packed_data = super::emit::encrypt_payload_with(encr.as_ref(), data);
            let mut plan = MemberPlan {
                name: name.clone(),
                unpacked_size: data.len() as u64,
                file_crc: header_crc,
                method: COMP_METHOD_STORE,
                dict_size_log: 0,
                dict_size_bytes: None,
                extra_data,
                attrs: crate::platform::memory_attributes(),
                mtime,
                solid: false,
                stored_hash,
            };
            plan.push_extra(time_extra.as_deref(), None);
            super::emit::write_file_entry(cx, &plan, &packed_data)?;
        } else {
            let (header_crc, extra_data, stored_hash, encr) =
                super::emit::payload_extra_and_crc(cx.password(), plain_crc, plain_blake);
            let packed_data = super::emit::encrypt_payload_with(encr.as_ref(), &packed);
            let mut plan = MemberPlan {
                name: name.clone(),
                unpacked_size: data.len() as u64,
                file_crc: header_crc,
                method,
                dict_size_log: dsl,
                dict_size_bytes: dict_bytes,
                extra_data,
                attrs: crate::platform::memory_attributes(),
                mtime,
                solid: chain_solid,
                stored_hash,
            };
            plan.push_extra(time_extra.as_deref(), None);
            super::emit::write_file_entry(cx, &plan, &packed_data)?;
        }
    }

    cx.report_progress(data.len() as u64, data.len() as u64);

    Ok(())
}
