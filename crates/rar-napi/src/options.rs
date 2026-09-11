//! Option mapping: JS-facing option structs validate and convert onto
//! the rar-rs typed option structs.

use napi::bindgen_prelude::*;

use crate::{AppendArchiveOptions, CreateArchiveOptions, ExtractArchiveOptions};

pub(crate) const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

// ── Conversions from the JS-facing option structs to the rar-rs library
// structs, so each field mapping lives in one place next to its struct.

pub(crate) fn checked_js_integer(value: f64, field: &str, min: u64, max: u64) -> Result<u64> {
  if !value.is_finite() || value.fract() != 0.0 || value.abs() > JS_MAX_SAFE_INTEGER {
    return Err(Error::new(
      Status::InvalidArg,
      format!("`{field}` must be a safe integer"),
    ));
  }
  if value < min as f64 || value > max as f64 {
    return Err(Error::new(
      Status::InvalidArg,
      format!("`{field}` must be in the range {min}..={max}"),
    ));
  }
  Ok(value as u64)
}

pub(crate) fn checked_optional_js_integer(
  value: Option<f64>,
  field: &str,
  min: u64,
  max: u64,
) -> Result<Option<u64>> {
  value
    .map(|value| checked_js_integer(value, field, min, max))
    .transpose()
}

impl CreateArchiveOptions {
  pub(crate) fn level(&self) -> Result<u8> {
    checked_js_integer(self.level.unwrap_or(3.0), "level", 0, 5).map(|value| value as u8)
  }

  pub(crate) fn max_total_bytes(&self) -> Result<Option<u64>> {
    checked_optional_js_integer(
      self.max_total_bytes,
      "maxTotalBytes",
      0,
      JS_MAX_SAFE_INTEGER as u64,
    )
  }

  pub(crate) fn to_writer_options(&self) -> Result<rar_rs::WriterOptions> {
    let recovery_percent =
      checked_optional_js_integer(self.recovery_percent, "recoveryPercent", 0, 100)?
        .filter(|&value| value != 0)
        .map(|value| value as u8);
    let recovery_volume_count = checked_optional_js_integer(
      self.recovery_volume_count,
      "recoveryVolumeCount",
      0,
      u32::MAX as u64,
    )?
    .filter(|&value| value != 0)
    .map(|value| value as u32);
    let volume_size = checked_optional_js_integer(
      self.volume_size,
      "volumeSize",
      1,
      JS_MAX_SAFE_INTEGER as u64,
    )?;
    let threads =
      checked_optional_js_integer(self.threads, "threads", 1, 64)?.map(|value| value as usize);
    let password = self.password.as_deref().filter(|p| !p.is_empty());
    let format = self.format.as_deref().unwrap_or("rar5");
    let compression = match format {
      "rar5" => None,
      "rar7" => Some(rar_rs::ArchiveVersion::V70),
      "rar4" => Some(rar_rs::ArchiveVersion::V29),
      "rar2" => Some(rar_rs::ArchiveVersion::V20),
      "rar15" => Some(rar_rs::ArchiveVersion::V15),
      other => {
        return Err(Error::new(
          Status::InvalidArg,
          format!("unknown format: `{other}` (expected rar5, rar7, rar4, rar2, or rar15)"),
        ));
      }
    };
    let dictionary = match self.dict_size.as_deref() {
      Some(_) if format == "rar4" || format == "rar2" || format == "rar15" => {
        return Err(Error::new(
          Status::InvalidArg,
          "RAR4 archives do not support configurable dictionary sizes",
        ));
      }
      Some(s) => {
        // `rar7` may use any byte dictionary (like `-ma7`), including
        // non-power-of-two sizes through 4 GiB; `rar5` keeps the strict
        // power-of-two rule because a plain v50 log has no way to carry
        // them.
        let parsed = if format == "rar7" {
          rar_rs::parse_dict_size(s)
            .or_else(|| rar_rs::parse_dict_bytes(s).map(|bytes| (None, Some(bytes))))
            .ok_or_else(|| Error::new(Status::InvalidArg, format!("invalid dictionary size: {s}")))
        } else {
          parse_dict_size(s)
        };
        let (dict_log, dict_bytes) = parsed?;
        let bytes = dict_bytes
          .or_else(|| dict_log.map(|log| (128u64 * 1024) << log))
          .expect("dictionary parse returns a log or a byte count");
        Some(rar_rs::DictionarySize::try_from(bytes).map_err(|err| {
          Error::new(
            Status::InvalidArg,
            format!("invalid dictionary size: {err}"),
          )
        })?)
      }
      None => None,
    };
    let opts = rar_rs::WriterOptions::new()
      .solid_mode(if self.solid.unwrap_or(false) {
        rar_rs::SolidMode::Continuous
      } else {
        rar_rs::SolidMode::Disabled
      })
      .quick_open(self.quick_open.unwrap_or(false))
      .blake2(self.blake2.unwrap_or(false))
      .encrypt_headers(self.encrypt_headers.unwrap_or(false))
      .save_ctime(self.save_ctime.unwrap_or(false))
      .save_atime(self.save_atime.unwrap_or(false))
      .save_mtime(true)
      .time_precision_seconds(self.time_precision_seconds.unwrap_or(false))
      .save_owner(self.save_owner.unwrap_or(false))
      .save_streams(self.save_streams.unwrap_or(false));
    let opts = if let Some(pw) = password {
      opts.password(pw.to_string())
    } else {
      opts
    };
    let opts = if let Some(percent) = recovery_percent {
      opts.recovery_percent(percent)
    } else {
      opts
    };
    let opts = if let Some(count) = recovery_volume_count {
      opts.recovery_volume_count(count)
    } else {
      opts
    };
    let opts = if let Some(size) = volume_size {
      opts.volume_size(size)
    } else {
      opts
    };
    let opts = if let Some(size) = dictionary {
      opts.dictionary_size(size)
    } else {
      opts
    };
    let opts = if let Some(version) = compression {
      opts.compression(version)
    } else {
      opts
    };
    let opts = if let Some(threads) = threads {
      opts.thread_count(
        rar_rs::ThreadCount::try_from(threads)
          .map_err(|err| Error::new(Status::InvalidArg, format!("{err}")))?,
      )
    } else {
      opts
    };
    Ok(opts)
  }
}

impl AppendArchiveOptions {
  pub(crate) fn level(&self) -> Result<u8> {
    checked_js_integer(self.level.unwrap_or(3.0), "level", 0, 5).map(|value| value as u8)
  }
}

impl ExtractArchiveOptions {
  /// `max_dict_size`: None (unset) keeps the WinRAR-style 4 GiB default
  /// cap; Some(0) means unlimited; other values raise/lower the cap.
  pub(crate) fn to_extract_options(&self) -> Result<rar_rs::ExtractOptions> {
    let max_dict_size = match checked_optional_js_integer(
      self.max_dict_size,
      "maxDictSize",
      0,
      JS_MAX_SAFE_INTEGER as u64,
    )? {
      None => Some(rar_rs::ExtractOptions::DEFAULT_MAX_DICT_SIZE),
      Some(0) => None,
      Some(value) => Some(value),
    };
    // Same encoding as `maxDictSize`: unset keeps the default ceiling,
    // 0 removes it.
    let max_metadata_bytes = match checked_optional_js_integer(
      self.max_metadata_bytes,
      "maxMetadataBytes",
      0,
      JS_MAX_SAFE_INTEGER as u64,
    )? {
      None => Some(rar_rs::ExtractOptions::DEFAULT_MAX_METADATA_BYTES),
      Some(0) => None,
      Some(value) => Some(value),
    };
    Ok(rar_rs::ExtractOptions {
      safe_paths: true,
      flat_paths: self.flat.unwrap_or(false),
      max_unpacked_bytes: None,
      max_total_unpacked_bytes: None,
      max_dict_size,
      max_metadata_bytes,
      skip_existing: self.skip_existing.unwrap_or(false),
      auto_rename: self.auto_rename.unwrap_or(false),
      keep_broken: self.keep_broken.unwrap_or(false),
      set_creation_time: self.set_creation_time.unwrap_or(false),
      set_access_time: self.set_access_time.unwrap_or(false),
    })
  }
}

/// Parse a WinRAR-style dictionary size (`-md<size>[k|m|g]`, no unit =
/// MiB) into the two `CreateOptions` fields: values up to 4 GiB must be
/// powers of two (RAR5 dict log), anything above is accepted as-is and
/// selects RAR7 (v70) with an actual byte size.
pub(crate) fn parse_dict_size(s: &str) -> Result<(Option<u8>, Option<u64>)> {
  rar_rs::parse_dict_size(s)
    .ok_or_else(|| Error::new(Status::InvalidArg, format!("invalid dictionary size: {s}")))
}
