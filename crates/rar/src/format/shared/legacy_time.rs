//! Legacy (RAR 1.3–4.x) DOS-time fields.
//!
//! The civil/Unix conversions live in the public [`crate::time`] module (the
//! CLI needs them too, so they are not duplicated); this module owns the DOS
//! packing the legacy containers store in their headers. `unix_to_dos_time`
//! and `dos_time_to_unix` are shared by RAR 1.5–4.x and RAR 1.3/1.4, so the
//! two families do not reach into each other for them.

pub(crate) use crate::time::{
    civil_from_days, days_from_civil, epoch_to_local_civil, local_civil_to_epoch,
};

/// Convert a RAR4 MS-DOS date/time (10/6/6 packed fields) to a Unix
/// timestamp (seconds). Best effort: DOS times predate the Unix epoch only
/// for pre-1980, so the result is a near-epoch non-negative value there.
pub(crate) fn dos_time_to_unix(dos: u32) -> u32 {
    let year = ((dos >> 25) & 0x7f) as i64 + 1980;
    let month = (dos >> 21) & 0x0f;
    let day = (dos >> 16) & 0x1f;
    let hour = (dos >> 11) & 0x1f;
    let minute = (dos >> 5) & 0x3f;
    let second = (dos & 0x1f) * 2;

    let days_since_epoch = days_from_civil(year, month, day);
    let secs = days_since_epoch * 86400
        + (i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second));
    secs.clamp(0, u32::MAX as i64) as u32
}

/// Convert a Unix timestamp (seconds since epoch) to the RAR4/RAR13 DOS
/// time field. The field stores *local* wall-clock time (WinRAR's
/// convention); pre-1980 years wrap like the official writers instead of
/// underflowing.
pub(crate) fn unix_to_dos_time(secs: u32) -> u32 {
    let local = epoch_to_local_civil(secs);
    let days = local / 86_400;
    let time_of_day = local % 86_400;
    let hour = time_of_day / 3_600;
    let minute = (time_of_day % 3_600) / 60;
    let second = time_of_day % 60;
    let (year, month, day) = civil_from_days(i64::from(days));

    // Pack into DOS format: Y(7) M(4) D(5) H(5) M(6) S(5/2). A pre-1980
    // year wraps into the 7-bit field exactly like WinRAR's writers.
    let year_bits = ((year - 1980) & 0x7F) as u32;
    (year_bits << 25) | (month << 21) | (day << 16) | (hour << 11) | (minute << 5) | (second / 2)
}
