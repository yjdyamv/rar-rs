//! Legacy civil-time primitives.
//!
//! RAR 1.3–4.x store local wall-clock time in DOS/`u32` header fields. The
//! catalog carries it as "local civil" seconds (a platform-neutral civil
//! encoding, i.e. the local wall clock reinterpreted as UTC), and the same
//! helpers convert it back to an instant when an extracted file's timestamp
//! is restored.
//!
//! These primitives are public because the CLI's `-ts`/`-tl` handling needs
//! exactly the same conversions; before this module existed the offset and
//! civil-day math was duplicated there, which meant the two copies could
//! drift. The legacy format code (`format::shared::legacy_time`) builds the
//! DOS fields on top of these.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 for a proleptic Gregorian civil date (Howard
/// Hinnant's `days_from_civil`).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: the civil date for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
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

/// Current local civil time as `(year, month, day, hour, minute, second)`.
pub fn local_civil_now() -> (i64, u32, u32, u32, u32, u32) {
    #[cfg(windows)]
    {
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st) };
        (
            i64::from(st.wYear),
            u32::from(st.wMonth),
            u32::from(st.wDay),
            u32::from(st.wHour),
            u32::from(st.wMinute),
            u32::from(st.wSecond),
        )
    }
    #[cfg(unix)]
    {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as libc::time_t)
            .unwrap_or(0);
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        (
            i64::from(tm.tm_year) + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
            tm.tm_min as u32,
            tm.tm_sec as u32,
        )
    }
    #[cfg(not(any(windows, unix)))]
    {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = (secs / 86_400) as i64;
        let (y, mo, d) = civil_from_days(days);
        let tod = secs % 86_400;
        (
            y,
            mo,
            d,
            (tod / 3600) as u32,
            ((tod % 3600) / 60) as u32,
            (tod % 60) as u32,
        )
    }
}

/// Seconds east of UTC for the local time zone, at "now" (minute precision;
/// targets without a local-time API report UTC).
fn local_offset_secs() -> i64 {
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    #[cfg(any(unix, windows))]
    {
        // `local_civil_now` samples the clock separately from `utc`; snap the
        // difference so the two reads can never straddle a second boundary.
        let (y, mo, d, h, mi, s) = local_civil_now();
        snap_to_minute(civil_to_epoch_secs(y, mo, d, h, mi, s) - utc)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = utc;
        0
    }
}

/// Interpret a civil time as if it were UTC, returning Unix seconds.
pub fn civil_to_epoch_secs(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    days_from_civil(y, mo, d) * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(s)
}

/// Convert a Unix instant to the "local civil" seconds the legacy catalog
/// stores.
pub fn epoch_to_local_civil(secs: u32) -> u32 {
    (i64::from(secs) + local_offset_secs()).clamp(0, u32::MAX as i64) as u32
}

/// Convert a legacy "local civil" time back to a Unix instant.
pub fn local_civil_to_epoch(secs: u32) -> u32 {
    (i64::from(secs) - local_offset_secs()).clamp(0, u32::MAX as i64) as u32
}

/// Convert a *local* civil time to a [`SystemTime`] using the current UTC
/// offset.
pub fn local_civil_to_system_time(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> SystemTime {
    let local = civil_to_epoch_secs(y, mo, d, h, mi, s) - local_offset_secs();
    if local >= 0 {
        UNIX_EPOCH + Duration::from_secs(local as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs((-local) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        civil_from_days, civil_to_epoch_secs, days_from_civil, local_offset_secs, snap_to_minute,
    };

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

    #[test]
    fn civil_day_conversions_are_inverse() {
        for days in [-20_000i64, -1, 0, 1, 19_723, 40_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        // The Unix epoch is day 0, 1970-01-01.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_to_epoch_secs(1970, 1, 1, 0, 0, 0), 0);
    }

    #[test]
    fn epoch_and_civil_conversions_are_inverse_and_clamped() {
        use super::{epoch_to_local_civil, local_civil_to_epoch};
        for secs in [0u32, 1, 1_000_000, 1_700_000_000, u32::MAX - 1] {
            // Near the top the offset may saturate, so only the safe range
            // round-trips.
            if secs < u32::MAX - 100_000 {
                assert_eq!(local_civil_to_epoch(epoch_to_local_civil(secs)), secs);
            }
        }
        // Saturating, not wrapping.
        assert!(epoch_to_local_civil(u32::MAX) >= epoch_to_local_civil(0));
    }
}
