//! Archive read/write state owned by [`RarArchive`](super::RarArchive).
//!
//! [`ReadState`]/[`WriteState`] carry everything that survives across the
//! calls of one operation: solid-chain decoders/encoders, extraction
//! options, member metadata policy, quick-open/recovery backfill positions
//! and the staged-commit target. The `RarArchive` engine facade and its
//! constructors live in `super`.

use std::fs;
use std::path::PathBuf;

use crate::codec::DecoderState;
use crate::crypto;

use super::volume_path;

/// Decrypted member payload plus the key material needed for integrity
/// verification.
pub(crate) struct DecryptedPayload {
    pub(crate) data: Vec<u8>,
    pub(crate) params: Option<crypto::EncryptionParams>,
    pub(crate) keys: Option<crypto::DerivedKeys>,
}

/// Read-side state for extraction and listing.
///
/// Groups fields exclusively used by read/extract paths (extract.rs).
/// Owned as `Option<ReadState>` inside [`RarArchive`](super::RarArchive); `None` when the
/// archive is opened for writing only.
pub(crate) struct ReadState {
    /// Persistent decoder state for RAR5 solid archive chains.
    pub solid_state: Option<DecoderState>,
    /// Index of the last file decoded in the solid chain (-1 = none).
    pub solid_decoded_through: isize,
    /// Persistent legacy decoder for solid chains (RAR 1.5/2.x/3.x).
    pub rar4_decoder: Option<crate::format::rar4::LegacyDecoder>,
    /// Index of the last legacy-solid member decoded (-1 = none).
    pub rar4_decoded_through: isize,
    /// Options for the current read/extract operation (set per call).
    pub extract_options: crate::options::ExtractOptions,
    /// NTFS alternate data streams ("STM" service records) attached to
    /// members, in archive order.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub streams: Vec<StreamRecord>,
    /// Mark of the Web propagation for extraction (WinRAR `-om`).
    pub motw: Option<crate::options::MarkOfTheWeb>,
    /// RAR 1.3/1.4 main-header flags of the first opened volume (`RE~^`
    /// family; comment/volume/solid bits).
    pub rar13_flags: u8,
}

impl Default for ReadState {
    fn default() -> Self {
        Self {
            solid_state: None,
            solid_decoded_through: -1,
            rar4_decoder: None,
            rar4_decoded_through: -1,
            extract_options: crate::options::ExtractOptions::default(),
            streams: Vec::new(),
            motw: None,
            rar13_flags: 0,
        }
    }
}

/// Persistent encoder for solid RAR 1.5/2.x chains (unp_ver 15/20): the
/// encoder instance carries the adaptive tables (and `Unpack20Encoder`'s
/// sliding window) across the members of a solid run, mirroring rars'
/// `solid_encoder` reuse. RAR29 solid chains use a separate
/// `rar4_solid_encoder` slot; only one is active per archive since member
/// versions are fixed at create time.
pub(crate) enum LegacySolidEncoder {
    Rar15(Box<crate::codec::legacy::rar15_encoder::Unpack15Encoder>),
    Rar20(crate::codec::legacy::rar20_encoder::Unpack20Encoder),
}

/// Write-side state for creation, append, and rewrite.
///
/// Groups fields exclusively used by write/create/append paths
/// (write/mod.rs + transaction.rs). Owned as `Option<WriteState>` inside
/// [`RarArchive`]; `None` when the archive is opened for reading only.
///
/// The fields are grouped by role: [`SolidChain`] carries the shared LZ
/// window and its reset policy, [`Rar4Append`] the legacy append
/// bookkeeping, [`CompressionSettings`] the `-mt`/`-md` knobs,
/// [`MetadataSettings`] the `-ts*`/`-ow`/`-os`/`-htb` policy,
/// [`LocatorState`] the quick-open/recovery offset backfill and
/// [`OutputState`] the staged commit and volume counters.
#[derive(Default)]
pub(crate) struct WriteState {
    pub solid: SolidChain,
    pub rar4: Rar4Append,
    pub compression: CompressionSettings,
    pub meta: MetadataSettings,
    pub locator: LocatorState,
    pub output: OutputState,
}

/// Shared solid-chain state for one run (RAR5 and the legacy codecs).
pub(crate) struct SolidChain {
    /// Create a solid archive (shared LZ window across compressed members).
    pub mode: bool,
    /// How the solid chain is split (WinRAR `-s` modifiers `-sd`/`-sv`/`-se`).
    pub reset: crate::options::SolidReset,
    /// File extension of the last member added to the solid chain; used by
    /// `SolidReset::PerExtension` to detect when to reset the statistics.
    pub last_ext: Option<String>,
    /// Persistent RAR5 encoder state for solid archives.
    pub encoder_state: Option<crate::codec::EncoderState>,
    /// Persistent RAR4 LZSS encoder for solid archives; the sliding window
    /// and Huffman table state carry across the members of a solid run.
    pub rar4_encoder: Option<crate::codec::legacy::rar29_encoder::Unpack29Encoder>,
    /// Persistent legacy (RAR 1.5/2.x) encoder for solid archives; see
    /// [`LegacySolidEncoder`]. `None` when the run has not started (STORE
    /// members and solid-extension resets drop it, rebuilding the chain).
    pub legacy_encoder: Option<LegacySolidEncoder>,
    /// The legacy member `unp_ver` the RAR4 write pipeline emits for this
    /// archive: 29 (RAR29, default) when the archive targets `v29`/`v36`,
    /// 20 for `v20`, 15 for `v15`. Drives member-codec dispatch in
    /// `add_rar4_data` and stamps every FILE_HEAD it writes.
    pub rar4_unp_ver: u8,
    /// True once the current RAR4 solid run has emitted a member, so the next
    /// compressed member is flagged as a chain continuation (`FHD_SOLID`).
    pub rar4_run_has_member: bool,
}

impl Default for SolidChain {
    fn default() -> Self {
        Self {
            mode: false,
            reset: crate::options::SolidReset::Continuous,
            last_ext: None,
            encoder_state: None,
            rar4_encoder: None,
            legacy_encoder: None,
            rar4_unp_ver: 29,
            rar4_run_has_member: false,
        }
    }
}

/// Legacy (RAR 1.5–4.x) append bookkeeping.
#[derive(Default)]
pub(crate) struct Rar4Append {
    /// Appending to an existing RAR4 archive that carried a NEWSUB recovery
    /// record: rebuild the record at close with this parity-sector count
    /// (the record's original strength; sector counts are not recoverable
    /// from a percent).
    pub rr_sectors: Option<u32>,
    /// Appending to a SOLID RAR4 archive defers to a whole-archive repack
    /// at close: the added members are buffered here (they cannot be
    /// streamed after a solid chain). `solid_append_entries` holds the
    /// buffered additions.
    pub solid_append: bool,
    /// Buffered additions for a deferred solid-append (see
    /// [`Self::solid_append`]).
    pub solid_append_entries: Vec<crate::archive::rar4_edit::SolidAppendEntry>,
    /// Archive-comment text queued for a RAR4 create/repack writer: emitted
    /// as a NEWSUB `CMT` block right before the first member (create writes
    /// members to a stream, so the comment must be queued before the first
    /// add). `None` = no comment.
    pub writer_comment: Option<Vec<u8>>,
}

/// Compression knobs (`-mt`, `-md`, and the RAR7 test seam).
#[derive(Default)]
pub(crate) struct CompressionSettings {
    /// Per-archive compression thread count (`-mt`); `None` = process-global
    /// default. The compression pool is selected per thread count, so
    /// concurrent archives with different values never interfere.
    pub threads: Option<usize>,
    /// Requested dictionary log for compression (WinRAR `-md`);
    /// `None` = default selection.
    pub dict_size_log: Option<u8>,
    /// Requested dictionary size in bytes for RAR7 (v70) members
    /// (WinRAR `-md` above 4 GiB, up to the 126 GiB encoding limit).
    pub dict_size_bytes: Option<u64>,
    /// Force RAR7 (v70) member headers even below the 4 GiB threshold
    /// (test seam; see `CreateOptions::force_v70`).
    pub force_v70: bool,
    /// Compression filter policy (`-mc`): automatic, disabled or forced
    /// delta / x86 filters.
    pub filters: crate::options::FilterOptions,
}

/// Member metadata policy (`-ts*`, `-ow`, `-os`, `-htb`).
pub(crate) struct MetadataSettings {
    /// Save creation/change time in the FILE_TIME extra record (`-tsc`).
    pub ctime: bool,
    /// Save last access time in the FILE_TIME extra record (`-tsa`).
    pub atime: bool,
    /// Save the modification time (`-tsm`; false with `-tsm-`/`-ts-`).
    pub mtime: bool,
    /// Save owner/group on Unix (`-ow`).
    pub owner: bool,
    /// Save NTFS alternate data streams (`-os`; Windows only).
    pub streams: bool,
    /// Store timestamps at 1-second precision (`-ts...1`).
    pub time_precision_seconds: bool,
    /// Write BLAKE2sp hash records for members.
    pub blake2: bool,
}

impl Default for MetadataSettings {
    fn default() -> Self {
        Self {
            ctime: false,
            atime: false,
            mtime: true,
            owner: false,
            streams: false,
            time_precision_seconds: false,
            blake2: false,
        }
    }
}

/// Quick-open and recovery locator backfill positions.
#[derive(Default)]
pub(crate) struct LocatorState {
    /// Write a quick-open ("QO") service record at close time.
    pub quick_open: bool,
    /// Cached (offset, full header bytes) of file headers for quick-open.
    pub quick_open_entries: Vec<(u64, Vec<u8>)>,
    /// File offset of the quick-open offset vint inside the main header's
    /// locator record (preallocated, patched at close time).
    pub qo_offset_field_pos: Option<u64>,
    /// File offset of the main archive header (for the recovery-record
    /// locator patch written at close time).
    pub main_header_start: Option<u64>,
    /// File offset of the recovery-record offset vint inside the main
    /// header's locator record (preallocated, patched at close time).
    pub rr_offset_field_pos: Option<u64>,
}

/// Staged-commit and volume counters.
#[derive(Default)]
pub(crate) struct OutputState {
    /// Staged write target during an uncommitted create/append: the data
    /// goes to temporary sibling files and is moved over the final paths
    /// only after `close` succeeds, so a failed or interrupted operation
    /// never leaves a partial archive at the final path.
    pub pending: Option<PendingCommit>,
    /// Volume size limit for multi-volume creation (None = single volume).
    pub volume_size: Option<u64>,
    /// Current volume number during creation (1-indexed).
    pub current_volume: usize,
    /// Bytes written in the current volume during creation.
    pub bytes_written: u64,
    /// RAR 1.3/1.4 create: the signature is written at open and the main
    /// header is deferred until the archive comment is known (first member
    /// or close).
    pub rar13_header_pending: bool,
}

/// An NTFS alternate data stream ("STM" service record) attached to an
/// archive member: the member index, the stream name (with the leading
/// colon, e.g. `:Zone.Identifier`), the stream payload location and its
/// compression parameters (the payload may be RAR5-compressed).
///
/// Fields are only read back on Windows (extraction restores streams via
/// `file:name`); on other platforms they are parsed and stored but never
/// consumed, so dead-code linting is relaxed there.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone)]
pub(crate) struct StreamRecord {
    pub owner_index: usize,
    pub name: String,
    pub data_offset: u64,
    pub data_size: u64,
    pub unpacked_size: u64,
    pub method: u8,
    pub dict_size_log: u8,
    /// Stored CRC32 over the decoded stream payload.
    pub crc32: Option<u32>,
    /// Encryption parameters from the stream block's ENCR extra record.
    pub params: Option<crypto::EncryptionParams>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Read,
    Write,
    Append,
}

/// Staged write target: new archive data is written to temporary sibling
/// files first and moved over the final paths on successful close.
pub(crate) enum PendingCommit {
    /// Single-volume write: the temporary file staged for the final path.
    Single(PathBuf),
    /// Multi-volume write: volumes are staged as `{tmp_base}.partN.rar`
    /// (in `parent`) and moved to `{final_base}.partN.rar` on close.
    Volumes {
        parent: PathBuf,
        tmp_base: String,
        final_base: String,
    },
}

impl PendingCommit {
    /// Remove staged files that were never committed.
    pub(super) fn cleanup(&self, volume_count: usize) {
        match self {
            PendingCommit::Single(tmp) => {
                let _ = fs::remove_file(tmp);
            }
            PendingCommit::Volumes {
                parent, tmp_base, ..
            } => {
                for n in 1..=volume_count {
                    let _ = fs::remove_file(volume_path(parent, tmp_base, n));
                }
            }
        }
    }
}
