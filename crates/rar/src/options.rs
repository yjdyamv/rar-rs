//! Public option structs for archive creation and extraction.

use crate::error::{RarError, RarResult};
use crate::version::ArchiveVersion;

pub(crate) const MAX_COMPRESSION_THREADS: usize = 64;
const MIN_DICTIONARY_BYTES: u64 = 128 * 1024;
const MAX_RAR5_DICTIONARY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub(crate) const DEFAULT_RAR7_DICTIONARY_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_RAR7_DICTIONARY_BYTES: u64 = 126 * 1024 * 1024 * 1024;

/// Upper bound on a block's *declared* data size when the payload has to be
/// buffered in memory to be interpreted (archive comment "CMT", NTFS stream
/// "STM", quick-open "QO").
///
/// The RAR5 block reader only validates the header CRC, so a hand-made
/// archive can declare an arbitrarily large data area — a buffer sized
/// straight from that field would abort the process on allocation instead of
/// returning an error. Service payloads are metadata, not member data (member
/// data goes through the caller-configurable [`ExtractOptions`] limits), so
/// one fixed ceiling covers all of them. Owned here because it is also the
/// default of [`ExtractOptions::DEFAULT_MAX_METADATA_BYTES`]; the RAR5 parser
/// imports it from this leaf rather than the other way round.
pub(crate) const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;

/// A validated dictionary size accepted by the RAR5 and RAR7 writers.
///
/// Sizes from 128 KiB through 4 GiB may be powers of two (with a RAR5
/// dictionary log) or arbitrary byte counts (RAR7-only, declared with the
/// 5-bit base plus 1/32 increment encoding). Any byte count through
/// 126 GiB is supported.
///
/// A size above 4 GiB selects RAR7 (v70) members. Under the default
/// compression version [`ArchiveVersion::V50`] that selection is
/// automatic, like WinRAR's `-md`: the request is capped at twice the
/// member size, so small members stay plain v50 and only members whose
/// effective dictionary exceeds 4 GiB are written as v70. Use
/// [`ArchiveVersion::V70`] to force v70 members for every member — this
/// is the only way to get a non-power-of-two dictionary through 4 GiB,
/// since a plain v50 member's `comp_dict_size` field is a log.
///
/// Lives in the option layer rather than with the writer facade: the RAR5
/// create policy names it too, and `format` must not depend on `archive`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DictionarySize(u64);

impl DictionarySize {
    /// Smallest supported dictionary size (128 KiB).
    pub const MIN: Self = Self(MIN_DICTIONARY_BYTES);
    /// Default dictionary requested for RAR5 and RAR7 creation (32 MiB).
    pub const DEFAULT: Self = Self(DEFAULT_RAR7_DICTIONARY_BYTES);
    /// Largest supported dictionary size (126 GiB).
    pub const MAX: Self = Self(MAX_RAR7_DICTIONARY_BYTES);

    /// Construct a dictionary size from a RAR5 log (`128 KiB << log`).
    pub fn from_rar5_log(log: u8) -> RarResult<Self> {
        if log > 15 {
            return Err(RarError::InvalidOption(format!(
                "RAR5 dictionary log must be in 0..=15, got {log}"
            )));
        }
        Ok(Self(MIN_DICTIONARY_BYTES << log))
    }

    /// Return the dictionary size in bytes.
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// Return the RAR5 dictionary log, or `None` for a RAR7-only size
    /// (any request above 4 GiB or a non-power-of-two byte count, which
    /// only a v70 header can declare exactly).
    pub const fn rar5_log(self) -> Option<u8> {
        if self.0 <= MAX_RAR5_DICTIONARY_BYTES && self.0.is_power_of_two() {
            Some((self.0.trailing_zeros() - MIN_DICTIONARY_BYTES.trailing_zeros()) as u8)
        } else {
            None
        }
    }
}

impl TryFrom<u64> for DictionarySize {
    type Error = RarError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if !(MIN_DICTIONARY_BYTES..=MAX_RAR7_DICTIONARY_BYTES).contains(&value) {
            return Err(RarError::InvalidOption(format!(
                "dictionary size must be in {MIN_DICTIONARY_BYTES}..={MAX_RAR7_DICTIONARY_BYTES} bytes, got {value}"
            )));
        }
        Ok(Self(value))
    }
}

/// How the solid compression chain is split (WinRAR `-s` modifiers).
///
/// A solid archive packs several consecutive members as one continuous LZ
/// stream. Resetting the statistics (clearing the shared window / Huffman
/// tables) between groups typically lowers compression but speeds access to
/// individual members and improves damage resistance. `Continuous` matches
/// this implementation's default (and WinRAR's `-sd`): the statistics are
/// kept across the whole archive, including volume boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SolidReset {
    /// Keep the solid statistics across the whole archive (WinRAR `-sd`).
    #[default]
    Continuous,
    /// Always reset the solid statistics at the start of each new volume
    /// (WinRAR `-sv`). Single-volume archives are unaffected.
    PerVolume,
    /// Reset the solid statistics whenever the file extension of the next
    /// member changes (WinRAR `-se`); members sharing an extension stay in
    /// one group.
    PerExtension,
}

/// How a compression filter participates in encoding (WinRAR `-mc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    /// Apply the filter only when it beats plain compression (the default).
    #[default]
    Auto,
    /// Never apply this filter (`-mc<mode>-`).
    Disabled,
    /// Apply this filter to all data, whether or not it helps
    /// (`-mc<mode>+`).
    Forced,
}

/// Compression filter policy from WinRAR's `-mc` switch.
///
/// The long-range (`-mcl`) and exhaustive (`-mcx`) modes have no
/// configuration here: long-range matching is always enabled for methods
/// 2–5, and the exhaustive parser is not implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilterOptions {
    /// Delta (multimedia) filter mode (`-mcd`).
    pub delta: FilterMode,
    /// x86 E8/E8E9 filter mode (`-mce`).
    pub x86: FilterMode,
    /// Channel count for a forced delta filter (`-mcd<N>+`, 1–31); `None`
    /// auto-selects among the supported channel counts.
    pub delta_channels: Option<u8>,
}

/// Options controlling RAR archive creation.
///
/// All fields default to the plain unencrypted single-volume create
/// behavior; enable only the features you need.
///
/// Internal create-options struct consumed by `archive/create.rs`, the RAR4
/// repack pipeline and the in-tree tests. The supported public builder is
/// [`crate::WriterOptions`], which validates the full combination of format,
/// dictionary, threads, recovery and encryption options before the archive is
/// opened; this struct is not part of the public API (ADR 0006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateOptions {
    /// Target member compression version. [`ArchiveVersion::V50`]
    /// (default) selects the modern RAR5 container; combine with
    /// `force_v70` (and `dict_size_bytes`) to request v70. The legacy
    /// RAR 1.5–4.x container pipeline (`Rar!\x1a\x07\x00`, fixed-width
    /// headers, 16-bit CRC) is selected by [`ArchiveVersion::V29`]
    /// (per-member `unp_ver 29`), [`ArchiveVersion::V20`] (`unp_ver 20`)
    /// or [`ArchiveVersion::V15`] (`unp_ver 15`), and the DOS-era
    /// RAR 1.3/1.4 container (`RE~^`, 4-byte signature) by
    /// [`ArchiveVersion::V14`] (members report `unp_ver 2`).
    ///
    /// Only writable versions are accepted: `v14`, `v15`, `v20`, `v29`,
    /// `v50` and `v70` (see [`ArchiveVersion::is_writable`]). `v26` and
    /// `v36` are read-only — their codecs are identical to `v20`/`v29` and
    /// writers emit the upstream base version instead, so they are rejected
    /// rather than silently downgraded.
    pub compression: ArchiveVersion,
    /// Create a solid archive: consecutive compressed members share one
    /// LZ window (better ratio, slower random access). Solid state can remain
    /// continuous across volumes or reset according to [`SolidReset`].
    pub solid: bool,
    /// How the solid chain is split (WinRAR `-s` modifiers `-sd`/`-sv`/`-se`).
    /// `Continuous` keeps the statistics across the whole archive (the
    /// default); `PerVolume` resets at every volume boundary; `PerExtension`
    /// resets when the member's file extension changes. Non-solid archives
    /// ignore this.
    pub solid_reset: SolidReset,
    /// Add a RAR5 quick-open ("QO") service record containing a copy of
    /// every file header. Only effective for single-volume archives
    /// without header encryption.
    pub quick_open: bool,
    /// Expected final archive size, used to reserve the locator's QO/RR
    /// offset fields at the width WinRAR would use for an archive that size
    /// (see [`WriterOptions::estimated_size`](crate::WriterOptions::estimated_size)).
    /// `None` keeps the default preallocated 5-byte fields. RAR5 only.
    pub estimated_size: Option<u64>,
    /// Write a BLAKE2sp hash record for every member, replacing the regular
    /// CRC32 field, matching WinRAR's `-htb` behavior.
    pub blake2: bool,
    /// Optional AES-256 password for file-level encryption.
    pub password: Option<String>,
    /// Encrypt archive headers (file names and structure). Requires
    /// `password`; in multi-volume archives each volume carries the required
    /// encryption setup.
    pub encrypt_headers: bool,
    /// Add an inline recovery record protecting this percent (0-100) of the
    /// archive (WinRAR `-rr<N>%`). Combined with `volume_size`, every data
    /// volume carries its own record protecting that volume (WinRAR `-rr`
    /// with `-v`), and it may be combined with `.rev` recovery volumes.
    pub recovery_percent: Option<u8>,
    /// Add an inline recovery record with exactly this many parity sectors
    /// (WinRAR RAR4 `-rr<N>`). Mutually exclusive with `recovery_percent`.
    pub recovery_sectors: Option<u32>,
    /// Create this many `.rev` recovery volumes as a percentage of the
    /// data volume count (WinRAR `-rvN%`). Requires `volume_size`.
    pub recovery_volumes_percent: Option<u8>,
    /// Create exactly this many `.rev` recovery volumes, auto-capped at
    /// the data volume count. Requires `volume_size`.
    pub recovery_volume_count: Option<u32>,
    /// Volume size in bytes; when set, produces a multi-volume archive.
    pub volume_size: Option<u64>,
    /// RAR4 only (WinRAR `-vn`): name the volume set the old way —
    /// `{base}.rar`, `{base}.r00`, … — and leave `MHD_NEWNUMBERING` clear,
    /// instead of the default zero-padded `{base}.partNN.rar` naming with the
    /// flag set. RAR 1.3/1.4 always use the old names; RAR5 has no old
    /// naming, so the flag is ignored there.
    pub old_numbering: bool,
    /// Dictionary size as a RAR5 log (`128 KiB << log`), like WinRAR's
    /// `-md`; `None` = WinRAR's default (32 MiB, capped at 2x the file
    /// size rounded down to a power of two). Valid logs: 0..=15
    /// (128 KiB .. 4 GiB).
    pub dict_size_log: Option<u8>,
    /// Actual dictionary size in bytes for RAR7 (v70) members (WinRAR's
    /// `-md` above 4 GiB). Values need not be powers of two; the header uses
    /// a 5-bit power-of-two base plus a 1/32 increment and can represent up
    /// to 126 GiB. Mutually exclusive with `dict_size_log` in practice (one
    /// `-md` switch only).
    pub dict_size_bytes: Option<u64>,
    /// Write RAR7 (v70) members (`comp_version` 1, DCX distance table)
    /// even when `dict_size_bytes` is at or below the 4 GiB threshold
    /// that normally selects v70 (WinRAR's `-md` semantics). The header
    /// is legal v70 — the format does not require a > 4 GiB dictionary —
    /// but WinRAR compatibility at this scale is not part of the
    /// validated surface, so this is mainly a test seam that runs the
    /// v70 code paths at small scale. Requires `dict_size_bytes`; no-op
    /// without it.
    pub force_v70: bool,
    /// Compression filter policy (WinRAR's `-mc`): automatic, disabled or
    /// forced delta / x86 filters.
    pub filters: FilterOptions,
    /// Save the creation time (Windows) / ctime (Unix inode change time)
    /// in the FILE_TIME extra record, like WinRAR's `-tsc`.
    pub save_ctime: bool,
    /// Save the last access time in the FILE_TIME extra record, like
    /// WinRAR's `-tsa`.
    pub save_atime: bool,
    /// Store timestamps at 1-second precision instead of nanoseconds,
    /// like WinRAR's `-ts...1` (all times of a member share one precision).
    pub time_precision_seconds: bool,
    /// Save the modification time (like WinRAR's `-tsm`; always on unless
    /// `-tsm-` / `-ts-` is given).
    pub save_mtime: bool,
    /// Save the owner and group (numeric ids) in an OWNER extra record on
    /// Unix (like WinRAR's `-ow`); no-op elsewhere.
    pub save_owner: bool,
    /// Save NTFS alternate data streams as "STM" service records (like
    /// WinRAR's `-os`); no-op off Windows.
    pub save_streams: bool,
    /// Compression threads for this archive (like `-mt<N>`), in the range
    /// 0..=64. `Some(0)` requests automatic sizing for this archive regardless
    /// of the process-global setting; `None` uses the process-global
    /// [`set_compression_threads`] setting. Scoped to the archive: concurrent
    /// archives with different thread counts each run on their own pool and
    /// never interfere. This field is only consulted when the `parallel`
    /// feature is enabled; without it compression is sequential regardless.
    pub threads: Option<usize>,
}

impl CreateOptions {
    pub(crate) fn validate(&self) -> RarResult<()> {
        require_writable_version(self.compression)?;
        validate_locator_estimate(self.estimated_size, self.compression)?;
        validate_solid_reset(self.compression, self.solid_reset)?;
        validate_dictionary(self.dict_size_log, self.dict_size_bytes)?;
        validate_threads(self.threads)?;
        validate_combinations(CombinationRules {
            quick_open: self.quick_open,
            encrypt_headers: self.encrypt_headers,
            password: self.password.as_deref(),
            recovery_percent: self.recovery_percent,
            recovery_sectors: self.recovery_sectors,
            recovery_volumes_percent: self.recovery_volumes_percent,
            recovery_volume_count: self.recovery_volume_count,
            volume_size: self.volume_size,
        })
    }
}

/// Refuse archive versions the writer cannot produce. `v26` and `v36` are
/// read-only (their codecs match `v20`/`v29` and writers emit the base
/// version); shared by `CreateOptions` and `WriterOptions` so both surfaces
/// report the same error text.
pub(crate) fn require_writable_version(version: ArchiveVersion) -> RarResult<()> {
    if !version.is_writable() {
        return Err(RarError::InvalidOption(format!(
            "only versions v14, v15, v20, v29, v50 and v70 are writable, got {version}"
        )));
    }
    Ok(())
}

/// Solid-chain resets the legacy writers cannot express: pre-RAR3 chains
/// are position-derived (`MHD_SOLID`) with no per-member reset flag, and the
/// RAR4 writer implements `-se` only for `v29` (no `-sv`).
pub(crate) fn validate_solid_reset(
    compression: ArchiveVersion,
    solid_reset: SolidReset,
) -> RarResult<()> {
    if solid_reset == SolidReset::Continuous {
        return Ok(());
    }
    if compression.is_rar13() || matches!(compression, ArchiveVersion::V15 | ArchiveVersion::V20) {
        return Err(RarError::InvalidOption(
            "solid-chain resets (-se/-sv) are not supported for RAR 1.3/1.4/1.5/2.x archives"
                .into(),
        ));
    }
    if compression.is_legacy() && solid_reset == SolidReset::PerVolume {
        return Err(RarError::InvalidOption(
            "per-volume solid resets (-sv) are not supported for RAR4 archives".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_dictionary(
    dict_size_log: Option<u8>,
    dict_size_bytes: Option<u64>,
) -> RarResult<()> {
    if let Some(log) = dict_size_log
        && log > 15
    {
        return Err(RarError::InvalidOption(format!(
            "dictionary size log {log} exceeds the supported maximum 15"
        )));
    }
    if dict_size_log.is_some() && dict_size_bytes.is_some() {
        return Err(RarError::InvalidOption(
            "dict_size_log and dict_size_bytes are mutually exclusive".into(),
        ));
    }
    if let Some(bytes) = dict_size_bytes
        && !(MIN_DICTIONARY_BYTES..=MAX_RAR7_DICTIONARY_BYTES).contains(&bytes)
    {
        return Err(RarError::InvalidOption(format!(
            "dictionary size {bytes} bytes is outside the supported range {MIN_DICTIONARY_BYTES}..={MAX_RAR7_DICTIONARY_BYTES}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_threads(threads: Option<usize>) -> RarResult<()> {
    if let Some(threads) = threads
        && threads > MAX_COMPRESSION_THREADS
    {
        return Err(RarError::InvalidOption(format!(
            "compression threads must be in 0..={MAX_COMPRESSION_THREADS}, got {threads}"
        )));
    }
    Ok(())
}

/// The locator size estimate only shapes the RAR5 main header's offset fields;
/// the legacy writers have no locator, so a set estimate would otherwise be
/// dropped silently.
pub(crate) fn validate_locator_estimate(
    estimate: Option<u64>,
    version: ArchiveVersion,
) -> RarResult<()> {
    let Some(estimate) = estimate else {
        return Ok(());
    };
    if estimate == 0 {
        return Err(RarError::InvalidOption(
            "the locator size estimate must be greater than zero".into(),
        ));
    }
    if version.is_legacy() || version.is_rar13() {
        return Err(RarError::InvalidOption(
            "the locator size estimate is a RAR5 option".into(),
        ));
    }
    Ok(())
}

/// The create-time combination rules shared by the plain [`CreateOptions`]
/// struct and the typed [`WriterOptions`](crate::WriterOptions) builder.
///
/// The typed builder refuses combinations the writer would otherwise silently
/// drop; validating the plain struct with the same rules keeps the two public
/// surfaces from disagreeing about what is legal.
pub(crate) struct CombinationRules<'a> {
    pub quick_open: bool,
    pub encrypt_headers: bool,
    pub password: Option<&'a str>,
    pub recovery_percent: Option<u8>,
    pub recovery_sectors: Option<u32>,
    pub recovery_volumes_percent: Option<u8>,
    pub recovery_volume_count: Option<u32>,
    pub volume_size: Option<u64>,
}

pub(crate) fn validate_combinations(rules: CombinationRules<'_>) -> RarResult<()> {
    if rules.quick_open && rules.encrypt_headers {
        return Err(RarError::InvalidOption(
            "quick-open cannot be combined with header encryption".into(),
        ));
    }
    if rules.quick_open && rules.volume_size.is_some() {
        return Err(RarError::InvalidOption(
            "quick-open cannot be combined with data volumes".into(),
        ));
    }
    for (name, percent) in [
        ("recovery percent", rules.recovery_percent),
        ("recovery-volume percent", rules.recovery_volumes_percent),
    ] {
        if percent.is_some_and(|value| value > 100) {
            return Err(RarError::InvalidOption(format!(
                "{name} must be in 0..=100"
            )));
        }
    }
    if rules.volume_size == Some(0) {
        return Err(RarError::InvalidOption(
            "volume size must be greater than zero".into(),
        ));
    }
    if rules.encrypt_headers && rules.password.is_none_or(str::is_empty) {
        return Err(RarError::InvalidOption(
            "header encryption requires a non-empty password".into(),
        ));
    }
    if rules.recovery_percent.is_some() && rules.recovery_sectors.is_some() {
        return Err(RarError::InvalidOption(
            "recovery percent and an exact recovery-sector count are mutually exclusive".into(),
        ));
    }
    if rules.recovery_sectors == Some(0) {
        return Err(RarError::InvalidOption(
            "recovery sector count must be greater than zero".into(),
        ));
    }
    if rules.recovery_volumes_percent.is_some() && rules.recovery_volume_count.is_some() {
        return Err(RarError::InvalidOption(
            "recovery-volume percent and exact count are mutually exclusive".into(),
        ));
    }
    if (rules.recovery_volumes_percent.is_some() || rules.recovery_volume_count.is_some())
        && rules.volume_size.is_none()
    {
        return Err(RarError::InvalidOption(
            "recovery volumes require a data-volume size".into(),
        ));
    }
    Ok(())
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            compression: ArchiveVersion::V50,
            solid: false,
            solid_reset: SolidReset::Continuous,
            quick_open: false,
            estimated_size: None,
            blake2: false,
            password: None,
            encrypt_headers: false,
            recovery_percent: None,
            recovery_sectors: None,
            recovery_volumes_percent: None,
            recovery_volume_count: None,
            volume_size: None,
            old_numbering: false,
            dict_size_log: None,
            dict_size_bytes: None,
            force_v70: false,
            filters: FilterOptions::default(),
            save_ctime: false,
            save_atime: false,
            time_precision_seconds: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            threads: None,
        }
    }
}

/// Options controlling extraction and buffered reads.
///
/// The defaults are deliberately safe: unsafe member names are rejected,
/// and per-file / total output sizes are bounded. Relax them only for
/// trusted archives.
///
/// Note: extraction to disk (`extract` / `extract_all`) is fully
/// streaming, so arbitrarily large members (multi-GiB) only need
/// `max_unpacked_bytes: None`. The 4 GiB default primarily guards the
/// in-memory `read` API, which materializes whole members in a `Vec`.
///
/// The type is `Clone` but deliberately not `Copy` (like
/// [`WriterOptions`](crate::WriterOptions)): it carries owned per-run policies
/// such as [`mark_web`](Self::mark_web).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOptions {
    /// Reject member names that could escape the destination directory
    /// (absolute paths, `..`, Windows drive components, NUL bytes) and
    /// verify resolved paths stay inside the destination.
    pub safe_paths: bool,
    /// Maximum uncompressed bytes allowed for a single member
    /// (`None` = unlimited).
    pub max_unpacked_bytes: Option<u64>,
    /// Maximum total uncompressed bytes allowed across one extraction
    /// (`None` = unlimited).
    pub max_total_unpacked_bytes: Option<u64>,
    /// Extraction worker threads for this run (like `-mt<N>`), mirroring
    /// [`WriterOptions::threads`](crate::WriterOptions). `None` falls back to
    /// [`set_extraction_threads`](crate::set_extraction_threads) and then to
    /// automatic sizing; `Some(0)` selects automatic sizing without consulting
    /// the global setting. Scoped to the run, so concurrent extractions with
    /// different counts do not configure each other. Like the writer's, the
    /// field is only consulted when the `parallel` feature is enabled;
    /// without it extraction is sequential regardless.
    pub threads: Option<usize>,
    /// Propagate the archive file's Mark of the Web onto every extracted file
    /// (WinRAR's `-om`); `None` disables it. See [`MarkOfTheWeb`]. A no-op on
    /// non-Windows platforms.
    pub mark_web: Option<MarkOfTheWeb>,
    /// Extract members flat: each member is written to the destination
    /// directory under its basename (no directory tree), like `rar e` /
    /// `unrar e`. The safe-path policy still applies — the member name is
    /// sanitized and contained before its basename is used.
    pub flat_paths: bool,
    /// Skip members whose destination already exists (like `-o-`): no
    /// overwrites, and existing files are left untouched.
    pub skip_existing: bool,
    /// Rename the destination automatically when it already exists
    /// (like `-or`): `name.ext` becomes `name(1).ext`, `name(2).ext`, ...
    pub auto_rename: bool,
    /// Ask the installed overwrite prompt about every existing destination,
    /// like WinRAR's interactive console mode. The prompt is installed with
    /// [`ArchiveReader::set_overwrite_prompt`](crate::ArchiveReader::set_overwrite_prompt);
    /// with none installed this falls back to skipping, so setting the flag
    /// alone never overwrites. `-y` / `-o+` / `-o-` / `-or` take precedence
    /// (the CLI does not set the flag in those cases).
    pub prompt_overwrite: bool,
    /// Freshen (`-f`): extract a member only when its destination exists and
    /// the archived modification time is newer; a missing destination is
    /// skipped.
    pub freshen: bool,
    /// Update (`-u`): like [`freshen`](Self::freshen), but a missing
    /// destination is extracted. Takes precedence when both are set.
    pub update: bool,
    /// Keep partially extracted files when a member fails to decode
    /// (like `-kb`): the incomplete output is left on disk.
    pub keep_broken: bool,
    /// Also restore the creation time (Windows) from the FILE_TIME extra
    /// record (like WinRAR's `-tsc` on extraction). Ignored on Unix,
    /// where the change time cannot be set.
    pub set_creation_time: bool,
    /// Also restore the last access time from the FILE_TIME extra record
    /// (like WinRAR's `-tsa` on extraction).
    pub set_access_time: bool,
    /// Maximum dictionary size accepted when decoding a member
    /// (`None` = unlimited). Defaults to 4 GiB, like WinRAR, which
    /// refuses archives whose dictionary exceeds 4 GiB (RAR7) unless
    /// `-mdx<size>` raises the cap.
    pub max_dict_size: Option<u64>,
    /// Ceiling on the *declared* size of a service payload that has to be
    /// buffered whole to be interpreted: the archive comment (`CMT`) and NTFS
    /// alternate data streams (`STM`).
    ///
    /// Those sizes come from the block header and only the header CRC is
    /// checked — which a hand-made archive can compute for any value — so
    /// without a ceiling a hostile block makes the reader allocate the
    /// declared size and abort instead of failing. Defaults to
    /// [`Self::DEFAULT_MAX_METADATA_BYTES`] (64 MiB); raise it for archives
    /// with genuinely large alternate data streams, or pass `None` to remove
    /// the bound entirely for archives you trust.
    ///
    /// The quick-open record is not covered: it is consumed while the archive
    /// is being opened, before any caller options apply, so it always uses
    /// the default ceiling.
    pub max_metadata_bytes: Option<u64>,
    /// Skip link/copy redirect members during extraction (`-ol-`: WinRAR
    /// skips symbolic links when this switch is present).
    pub skip_links: bool,
    /// Extract links with dangerous targets as-is (`-ola`): the link safety
    /// checks (`safe_paths` for link bodies) are disabled. Placing links
    /// outside the destination is a security risk; only use this for
    /// trusted archives.
    pub allow_unsafe_links: bool,
}

impl ExtractOptions {
    /// Default dictionary cap (`Some(4 GiB)`), like WinRAR, which refuses
    /// archives whose dictionary exceeds 4 GiB (RAR7) unless `-mdx<size>`
    /// raises the cap.
    pub const DEFAULT_MAX_DICT_SIZE: u64 = 4 * 1024 * 1024 * 1024;

    /// Default service-payload ceiling (64 MiB): generous enough for real
    /// comments and alternate data streams, small enough that a forged size
    /// cannot drive an enormous allocation.
    pub const DEFAULT_MAX_METADATA_BYTES: u64 = MAX_METADATA_BYTES;

    /// The effective service-payload ceiling; `None` means unbounded.
    pub(crate) fn metadata_limit(&self) -> u64 {
        self.max_metadata_bytes.unwrap_or(u64::MAX)
    }
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            safe_paths: true,
            max_unpacked_bytes: Some(4 * 1024 * 1024 * 1024),
            max_total_unpacked_bytes: Some(32 * 1024 * 1024 * 1024),
            threads: None,
            mark_web: None,
            flat_paths: false,
            skip_existing: false,
            auto_rename: false,
            prompt_overwrite: false,
            freshen: false,
            update: false,
            keep_broken: false,
            set_creation_time: false,
            set_access_time: false,
            max_dict_size: Some(Self::DEFAULT_MAX_DICT_SIZE),
            max_metadata_bytes: Some(Self::DEFAULT_MAX_METADATA_BYTES),
            skip_links: false,
            allow_unsafe_links: false,
        }
    }
}

/// The answer the interactive overwrite prompt gave for one destination
/// (WinRAR's `Y`/`N`/`A`/`R`/`Q` on an existing file).
///
/// [`ExtractOptions::prompt_overwrite`] makes the extraction loop ask the
/// installed [`OverwritePrompt`] for one of these before replacing a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteChoice {
    /// Yes: overwrite this destination.
    Overwrite,
    /// No: leave this destination untouched.
    Skip,
    /// All: overwrite this and every later existing destination. The prompt
    /// callback is responsible for remembering this and answering the later
    /// calls without asking again.
    OverwriteAll,
    /// Rename: write to the next free `name(N).ext` instead.
    Rename,
    /// Quit: abort the extraction with [`RarError::Cancelled`].
    Quit,
}

/// Callback asked for each existing destination when
/// [`ExtractOptions::prompt_overwrite`] is set.
///
/// Installed with
/// [`ArchiveReader::set_overwrite_prompt`](crate::ArchiveReader::set_overwrite_prompt).
/// The library never reads the terminal itself, so a front end supplies the
/// prompt (and any "all" state) here.
pub type OverwritePrompt = dyn Fn(&std::path::Path) -> OverwriteChoice + Send + Sync;

/// Mark of the Web propagation for extraction (WinRAR's `-om`).
///
/// Browsers tag downloaded files with a `Zone.Identifier` alternate data
/// stream; when set as [`ExtractOptions::mark_web`], the archive file's
/// own stream is copied onto every extracted file. Windows only: the
/// setting is ignored on other platforms, where the concept does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkOfTheWeb {
    /// Copy every field; when `false` only the security zone value
    /// (`ZoneId=`) is propagated, matching `-om` without the `1` modifier.
    pub all_fields: bool,
    /// Restrict propagation to these file extensions (lowercase, no
    /// leading dot); `None` propagates to every extracted file.
    pub extensions: Option<Vec<String>>,
}

/// Parse a WinRAR `-md<size>[k|m|g]` dictionary size into the
/// `(dict_size_log, dict_size_bytes)` pair used by the internal
/// `CreateOptions`. No unit means MiB.
///
/// Sizes in the RAR5 range (128 KiB ..= 4 GiB) must be a power of two and
/// map to a dict log (WinRAR rejects e.g. `-md3m` with "Unknown option");
/// anything above 4 GiB is accepted as-is (RAR7 v70 members), capped at the
/// exactly encodable maximum of 126 GiB (a 64 GiB base plus 31/32).
///
/// Non-power-of-two sizes through 4 GiB are rejected here (`None`): a plain
/// v50 header has no way to carry them, matching WinRAR. Callers that
/// force v70 members (the `-ma7` extension, `format: "rar7"`) should fall
/// back to [`parse_dict_bytes`], which accepts any byte count in the
/// supported range for the v70 byte-dictionary header field.
///
/// Returns `None` for empty, unparsable or out-of-range values.
pub fn parse_dict_size(s: &str) -> Option<(Option<u8>, Option<u64>)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1024 * 1024),
    };
    let bytes = num
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|b| *b >= 128 * 1024)?;
    if bytes <= 4 * 1024 * 1024 * 1024 {
        if !bytes.is_power_of_two() {
            return None;
        }
        // 128 KiB = 2^17, so log = trailing_zeros - 17 (0..=15).
        return Some((Some((bytes.trailing_zeros() - 17) as u8), None));
    }
    if bytes > MAX_RAR7_DICTIONARY_BYTES {
        return None;
    }
    Some((None, Some(bytes)))
}

/// Parse a WinRAR-style dictionary size into a raw byte count without the
/// RAR5 power-of-two gate: the escape hatch for forces v70 (`-ma7` /
/// `format: "rar7"`) that need a small non-power-of-two dictionary such as
/// 6 MiB. Any supported range passes; returns `None` for empty, unparsable
/// or out-of-range values.
pub fn parse_dict_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1024 * 1024),
    };
    let bytes = num
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|b| (MIN_DICTIONARY_BYTES..=MAX_RAR7_DICTIONARY_BYTES).contains(b))?;
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RAR7_DICTIONARY_BYTES, parse_dict_bytes, parse_dict_size};

    #[test]
    fn dict_size_parses_rar5_range() {
        assert_eq!(parse_dict_size("128k"), Some((Some(0), None)));
        assert_eq!(parse_dict_size("1m"), Some((Some(3), None)));
        assert_eq!(parse_dict_size("32m"), Some((Some(8), None)));
        assert_eq!(parse_dict_size("4g"), Some((Some(15), None)));
        // No unit means MiB.
        assert_eq!(parse_dict_size("64"), Some((Some(9), None)));
        // Case-insensitive suffix.
        assert_eq!(parse_dict_size("128K"), Some((Some(0), None)));
    }

    #[test]
    fn dict_size_rejects_invalid_rar5_values() {
        // Below the 128 KiB floor, non-power-of-two, unparsable, empty.
        assert_eq!(parse_dict_size("1k"), None);
        assert_eq!(parse_dict_size("3m"), None);
        assert_eq!(parse_dict_size("abc"), None);
        assert_eq!(parse_dict_size(""), None);
    }

    #[test]
    fn dict_size_parses_v70_range() {
        assert_eq!(
            parse_dict_size("5g"),
            Some((None, Some(5 * 1024 * 1024 * 1024)))
        );
        assert_eq!(
            parse_dict_size("64g"),
            Some((None, Some(64 * 1024 * 1024 * 1024)))
        );
        assert_eq!(
            parse_dict_size("126g"),
            Some((None, Some(MAX_RAR7_DICTIONARY_BYTES)))
        );
        assert_eq!(parse_dict_size("127g"), None);
        assert_eq!(parse_dict_size("128g"), None);
    }

    #[test]
    fn dict_bytes_parses_non_power_of_two_for_v70() {
        // The v70 escape hatch accepts the sizes parse_dict_size rejects.
        assert_eq!(parse_dict_bytes("6m"), Some(6 * 1024 * 1024));
        assert_eq!(parse_dict_bytes("6M"), Some(6 * 1024 * 1024));
        assert_eq!(parse_dict_bytes("3g"), Some(3 * 1024 * 1024 * 1024));
        assert_eq!(parse_dict_bytes("128k"), Some(128 * 1024));
        assert_eq!(parse_dict_bytes("32m"), Some(32 * 1024 * 1024));
        assert_eq!(parse_dict_bytes("126g"), Some(MAX_RAR7_DICTIONARY_BYTES));
        // Out of range / unparsable still rejected.
        assert_eq!(parse_dict_bytes("1k"), None);
        assert_eq!(parse_dict_bytes("127g"), None);
        assert_eq!(parse_dict_bytes("abc"), None);
        assert_eq!(parse_dict_bytes(""), None);
    }
}
