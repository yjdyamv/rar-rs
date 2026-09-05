//! Option mapping: JS-facing option structs validate and convert onto
//! the rar5 typed option structs.

use napi::bindgen_prelude::*;

use crate::{AppendArchiveOptions, CreateArchiveOptions, ExtractArchiveOptions};

pub(crate) const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

// ── Conversions from the JS-facing option structs to the rar5 library
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

  pub(crate) fn to_writer_options(&self) -> Result<rar5::WriterOptions> {
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
    let dictionary = match self.dict_size.as_deref() {
      Some(s) => {
        let (dict_log, dict_bytes) = parse_dict_size(s)?;
        let bytes = dict_bytes
          .or_else(|| dict_log.map(|log| (128u64 * 1024) << log))
          .expect("dictionary parse returns a log or a byte count");
        Some(rar5::DictionarySize::try_from(bytes).map_err(|err| {
          Error::new(
            Status::InvalidArg,
            format!("invalid dictionary size: {err}"),
          )
        })?)
      }
      None => None,
    };
    let opts = rar5::WriterOptions::new()
      .solid_mode(if self.solid.unwrap_or(false) {
        rar5::SolidMode::Continuous
      } else {
        rar5::SolidMode::Disabled
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
    let opts = if let Some(threads) = threads {
      opts.thread_count(
        rar5::ThreadCount::try_from(threads)
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
  pub(crate) fn to_extract_options(&self) -> Result<rar5::ExtractOptions> {
    let max_dict_size = match checked_optional_js_integer(
      self.max_dict_size,
      "maxDictSize",
      0,
      JS_MAX_SAFE_INTEGER as u64,
    )? {
      None => Some(4 * 1024 * 1024 * 1024),
      Some(0) => None,
      Some(value) => Some(value),
    };
    Ok(rar5::ExtractOptions {
      flat_paths: self.flat.unwrap_or(false),
      max_unpacked_bytes: None,
      max_total_unpacked_bytes: None,
      max_dict_size,
      ..Default::default()
    })
  }
}

/// Parse a WinRAR-style dictionary size (`-md<size>[k|m|g]`, no unit =
/// MiB) into the two `CreateOptions` fields: values up to 4 GiB must be
/// powers of two (RAR5 dict log), anything above is accepted as-is and
/// selects RAR7 (v70) with an actual byte size.
pub(crate) fn parse_dict_size(s: &str) -> Result<(Option<u8>, Option<u64>)> {
  rar5::parse_dict_size(s)
    .ok_or_else(|| Error::new(Status::InvalidArg, format!("invalid dictionary size: {s}")))
}
