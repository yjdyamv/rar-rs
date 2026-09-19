//! DOS/Unix date formatting and -ts spec parsing.
//!
//! The civil/local-time primitives themselves live in the library
//! ([`rar_rs::time`]): the CLI and the legacy format code need exactly the
//! same offset and calendar math, and keeping two copies meant they could
//! drift. Callers reach the library module directly (`rar_rs::time::…`).

use std::time::SystemTime;

use rar_rs::time::{civil_from_days, local_civil_to_system_time};

/// Parse a `-tk<date>` argument: `YYYYMMDDHHMMSS` with optional `-`/`:`
/// separators and optional trailing components (`2020`, `202001`,
/// `2020-01-02-03-04-05`). Missing components default to January 1st,
/// midnight; the result is a *local* time.
pub fn parse_tk_date(spec: &str) -> Result<SystemTime, String> {
    let digits: String = spec.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return Err(format!("invalid -tk date: {spec}"));
    }
    let mut fields = [0u32; 6];
    let mut cursor = 0usize;
    let mut index = 0usize;
    while cursor < digits.len() && index < fields.len() {
        let width = if index == 0 { 4 } else { 2 };
        let end = (cursor + width).min(digits.len());
        fields[index] = digits[cursor..end]
            .parse()
            .map_err(|_| format!("invalid -tk date: {spec}"))?;
        cursor = end;
        index += 1;
    }
    if cursor < digits.len() {
        return Err(format!("invalid -tk date: {spec}"));
    }
    let (year, month, day, hour, minute, second) = (
        fields[0],
        fields[1].max(1),
        fields[2].max(1),
        fields[3],
        fields[4],
        fields[5],
    );
    if year < 1970
        || !(1..=12).contains(&fields[1].max(1))
        || !(1..=31).contains(&fields[2].max(1))
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(format!("invalid -tk date: {spec}"));
    }
    Ok(local_civil_to_system_time(
        i64::from(year),
        month,
        day,
        hour,
        minute,
        second,
    ))
}

/// Format an `-ag[fmt]` archive-name stamp. A bare `-ag` uses
/// `YYYYMMDDHHMMSS`; otherwise `YYYY`/`MM`/`DD`/`HH`/`mm`/`SS` are
/// substituted (`mm` is minutes, like WinRAR).
pub fn format_auto_name(fmt: &str, y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> String {
    if fmt.is_empty() {
        return format!("{y:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}");
    }
    fmt.replace("YYYY", &format!("{y:04}"))
        .replace("MM", &format!("{mo:02}"))
        .replace("DD", &format!("{d:02}"))
        .replace("HH", &format!("{h:02}"))
        .replace("mm", &format!("{mi:02}"))
        .replace("SS", &format!("{s:02}"))
}

/// Format a Unix timestamp as a local civil timestamp
/// (`YYYY-MM-DD HH:MM:SS`), the way WinRAR lists member times.
pub fn format_local_time(secs: u32) -> String {
    let local = i64::from(rar_rs::time::epoch_to_local_civil(secs));
    let days = local.div_euclid(86_400);
    let tod = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Render a RAR 1.5–4.x timestamp as stored: those formats keep DOS local
/// time with no zone, so no offset is applied.
pub fn format_civil_time(secs: u32) -> String {
    let secs = i64::from(secs);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Set a file's modification time.
pub fn set_file_mtime(path: &std::path::Path, time: SystemTime) -> std::io::Result<()> {
    let file = std::fs::File::options().write(true).open(path)?;
    file.set_times(std::fs::FileTimes::new().set_modified(time))
}

/// Parsed `-ts` settings: which times to save and at what precision.
#[derive(Clone, Copy, Default)]
pub struct TsSettings {
    pub save_mtime: bool,
    pub save_ctime: bool,
    pub save_atime: bool,
    pub precision_seconds: bool,
}

/// Parse repeatable `-ts[m,c,a][+,-,1]` specs with WinRAR semantics:
/// a bare `-ts` (or no kinds) selects all three times; `-` omits a time,
/// `1` selects 1-second precision, `+` high precision (default). All
/// times of a member share one precision (`+` wins over `1`).
pub fn parse_ts_specs(specs: &[String]) -> Result<TsSettings, String> {
    let mut settings = TsSettings {
        save_mtime: true,
        ..Default::default()
    };
    if specs.is_empty() {
        return Ok(settings);
    }
    let mut save = [false, false, false]; // m, c, a
    let mut saw_plus = false;
    let mut saw_one = false;
    for spec in specs {
        let mut kinds = 0u8; // bit 0 = m, 1 = c, 2 = a
        let mut mode: Option<char> = None;
        for ch in spec.chars() {
            match ch {
                'm' => kinds |= 1,
                'c' => kinds |= 2,
                'a' => kinds |= 4,
                '+' | '1' | '-' => {
                    if mode.is_some() {
                        return Err(format!("invalid -ts spec: {spec}"));
                    }
                    mode = Some(ch);
                }
                _ => return Err(format!("invalid -ts spec: {spec}")),
            }
        }
        if kinds == 0 {
            kinds = 7; // bare -ts: all three
        }
        match mode {
            Some('-') => {
                // Omit the selected times entirely.
                if kinds & 1 != 0 {
                    save[0] = false;
                }
                if kinds & 2 != 0 {
                    save[1] = false;
                }
                if kinds & 4 != 0 {
                    save[2] = false;
                }
            }
            Some('1') => {
                saw_one = true;
                for (i, bit) in [1u8, 2, 4].iter().enumerate() {
                    if kinds & bit != 0 {
                        save[i] = true;
                    }
                }
            }
            _ => {
                // '+' or implicit: high precision.
                if mode == Some('+') {
                    saw_plus = true;
                }
                for (i, bit) in [1u8, 2, 4].iter().enumerate() {
                    if kinds & bit != 0 {
                        save[i] = true;
                    }
                }
            }
        }
    }
    settings.save_mtime = save[0];
    settings.save_ctime = save[1];
    settings.save_atime = save[2];
    settings.precision_seconds = saw_one && !saw_plus;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tk_date_parses_forms_and_defaults() {
        // `parse_tk_date` must agree with the library's civil-to-instant
        // conversion (same offset source, no second copy).
        let expect = rar_rs::time::local_civil_to_system_time;
        assert_eq!(
            parse_tk_date("2020-01-01").unwrap(),
            expect(2020, 1, 1, 0, 0, 0)
        );
        assert_eq!(
            parse_tk_date("20200102030405").unwrap(),
            expect(2020, 1, 2, 3, 4, 5)
        );
        assert_eq!(
            parse_tk_date("2020-01-02-03-04-05").unwrap(),
            expect(2020, 1, 2, 3, 4, 5)
        );
        assert_eq!(
            parse_tk_date("202001020304").unwrap(),
            expect(2020, 1, 2, 3, 4, 0)
        );
        assert_eq!(parse_tk_date("2021").unwrap(), expect(2021, 1, 1, 0, 0, 0));
        assert!(parse_tk_date("").is_err());
        assert!(parse_tk_date("abcd").is_err());
        assert!(parse_tk_date("2020-13-01").is_err());
        assert!(parse_tk_date("2020-01-01-25").is_err());
    }

    #[test]
    fn auto_name_formats_the_stamp() {
        assert_eq!(
            format_auto_name("", 2026, 9, 13, 16, 18, 52),
            "20260913161852"
        );
        assert_eq!(
            format_auto_name("YYYY-MM-DD", 2026, 9, 13, 16, 18, 52),
            "2026-09-13"
        );
        assert_eq!(
            format_auto_name("x_YYYYMMDD-HHmmSS", 2026, 9, 13, 16, 18, 52),
            "x_20260913-161852"
        );
    }

    /// The legacy-civil conversion must add the local offset (and clamp);
    /// update comparisons depend on it being monotone with the stored time.
    #[test]
    fn local_timestamp_uses_the_local_offset() {
        let secs: u32 = 1_700_000_000;
        // The CLI's formatting must agree with the library's civil encoding
        // (the offset has exactly one implementation).
        let local = i64::from(rar_rs::time::epoch_to_local_civil(secs));
        let days = local.div_euclid(86_400);
        let tod = local.rem_euclid(86_400);
        let (y, mo, d) = civil_from_days(days);
        assert_eq!(
            format_local_time(secs),
            format!(
                "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}",
                tod / 3600,
                (tod % 3600) / 60,
                tod % 60
            )
        );
    }
}
