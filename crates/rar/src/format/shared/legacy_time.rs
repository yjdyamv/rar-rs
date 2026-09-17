//! Legacy (RAR 1.3–4.x) civil-time conversions.
//!
//! Legacy containers store local wall-clock time in their DOS/`u32` fields.
//! The catalog carries it as "local civil" seconds (a platform-neutral
//! civil encoding), and the same helpers convert it back to an instant when
//! the extracted file's timestamp is restored.

/// Days since 1970-01-01 for a proleptic Gregorian civil date (Howard
/// Hinnant's `days_from_civil`).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Real time zones are whole minutes. Snap a raw `local - utc` sample to the
/// nearest minute so a platform clock that lags the high-resolution UTC sample
/// — Windows' `GetLocalTime` only advances on the ~15.6 ms system tick — cannot
/// yield an offset off by a second. A one-second wobble would flip the legacy
/// DOS field's `ADD_SECOND` parity (2-second resolution) and make the encoded
/// timestamp depend on *when* it was read.
#[cfg(any(unix, windows))]
fn snap_to_minute(secs: i64) -> i64 {
    let rem = secs.rem_euclid(60);
    if rem >= 30 {
        secs + (60 - rem)
    } else {
        secs - rem
    }
}

/// Seconds east of UTC for the local time zone, at "now" (minute precision;
/// targets without a local-time API report UTC).
fn local_offset_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    #[cfg(windows)]
    {
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st) };
        let civil = days_from_civil(
            i64::from(st.wYear),
            u32::from(st.wMonth),
            u32::from(st.wDay),
        ) * 86_400
            + i64::from(st.wHour) * 3_600
            + i64::from(st.wMinute) * 60
            + i64::from(st.wSecond);
        snap_to_minute(civil - utc)
    }
    #[cfg(unix)]
    {
        let secs = utc as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        let civil = days_from_civil(
            i64::from(tm.tm_year) + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
        ) * 86_400
            + i64::from(tm.tm_hour) * 3_600
            + i64::from(tm.tm_min) * 60
            + i64::from(tm.tm_sec);
        snap_to_minute(civil - utc)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = utc;
        0
    }
}

/// Convert a Unix instant to the "local civil" seconds the legacy catalog
/// stores (the encoding [`crate::format::rar4::dos_time_to_unix`] produces).
pub(crate) fn epoch_to_local_civil(secs: u32) -> u32 {
    (i64::from(secs) + local_offset_secs()).clamp(0, u32::MAX as i64) as u32
}

/// Convert a legacy "local civil" time back to a Unix instant.
pub(crate) fn local_civil_to_epoch(secs: u32) -> u32 {
    (i64::from(secs) - local_offset_secs()).clamp(0, u32::MAX as i64) as u32
}

#[cfg(test)]
mod tests {
    use super::{local_offset_secs, snap_to_minute};

    #[test]
    fn snap_to_minute_rounds_the_clock_tick_wobble() {
        assert_eq!(snap_to_minute(28_800), 28_800);
        // `GetLocalTime` a tick behind/ahead still lands on the whole minute.
        assert_eq!(snap_to_minute(28_799), 28_800);
        assert_eq!(snap_to_minute(28_801), 28_800);
        assert_eq!(snap_to_minute(-28_799), -28_800);
        assert_eq!(snap_to_minute(-28_801), -28_800);
        assert_eq!(snap_to_minute(0), 0);
    }

    /// Every conversion of one member must see the same offset: a one-second
    /// change flips the 2-second DOS field's `ADD_SECOND` bit, so sequential
    /// and batch writes of the same file would differ byte-for-byte.
    #[test]
    fn local_offset_is_minute_aligned() {
        for _ in 0..200_000 {
            assert_eq!(local_offset_secs() % 60, 0);
        }
    }
}
