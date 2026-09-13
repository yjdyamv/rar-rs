//! Time/size/mask filters shared by update, list and test.

/// Normalize a path argument into an archive name: relative paths stay as
/// given, absolute paths drop the leading slash (like `rar`).
pub(crate) fn arg_to_name(arg: &str) -> String {
    crate::name_policy::arg_to_name(arg)
}

/// Read one mask per line from a filter list file (like `-x@listfile`);
/// blank lines are skipped.
pub(crate) fn read_mask_file(path: &str) -> Result<Vec<String>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read mask list {path}: {e}"))?;
    Ok(content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Parse a WinRAR date (`-ta`/`-tb`) into unix seconds. Accepts
/// `YYYY[MM[DD[HH[MM[SS]]]]]`; missing trailing parts default to their
/// minimum (month/day 01, time 00:00:00).
pub(crate) fn parse_rar_date(s: &str) -> Result<u32, String> {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let (y, m, d, hh, mm, ss) = match digits.len() {
        4 => (digits.parse::<i64>().unwrap(), 1, 1, 0, 0, 0),
        6 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            1,
            0,
            0,
            0,
        ),
        8 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            0,
            0,
            0,
        ),
        10 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            digits[8..10].parse().unwrap(),
            0,
            0,
        ),
        12 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            digits[8..10].parse().unwrap(),
            digits[10..12].parse().unwrap(),
            0,
        ),
        14 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            digits[8..10].parse().unwrap(),
            digits[10..12].parse().unwrap(),
            digits[12..14].parse().unwrap(),
        ),
        _ => return Err(format!("invalid date: {s} (use YYYYMMDDHHMMSS)")),
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return Err(format!("invalid date: {s}"));
    }
    let days = crate::time::days_from_civil(y, m, d);
    let secs = days * 86400 + i64::from(hh) * 3600 + i64::from(mm) * 60 + i64::from(ss);
    u32::try_from(secs).map_err(|_| format!("date out of range: {s}"))
}

/// Which file timestamp a `-tn`/`-to` filter compares (`m`/`c`/`a`
/// modifiers; `m` is the default, `o` is accepted but has no effect since
/// every filter here uses a single time kind).
#[derive(Clone, Copy)]
pub(crate) enum TimeKind {
    Modified,
    Created,
    Accessed,
}

/// Parse a WinRAR `-tn`/`-to` filter: optional leading `m`/`c`/`a`/`o`
/// modifiers followed by a period `[<ndays>d][<nhours>h][<nminutes>m][<nseconds>s]`.
/// Returns the time kind and the period in seconds. Like WinRAR, an empty
/// or unparsable period is treated as 0 seconds.
pub(crate) fn parse_period_filter(s: &str) -> (TimeKind, u64) {
    let mut kind = TimeKind::Modified;
    let mut idx = 0;
    for ch in s.chars() {
        match ch {
            'm' => kind = TimeKind::Modified,
            'c' => kind = TimeKind::Created,
            'a' => kind = TimeKind::Accessed,
            'o' => {} // OR logic: no effect with a single time kind
            _ => break,
        }
        idx += 1;
    }
    (kind, parse_period(&s[idx..]))
}

/// Parse a period string `[<ndays>d][<nhours>h][<nminutes>m][<nseconds>s]`
/// into seconds. Anything that does not parse (including a bare number or
/// an empty string) yields 0, matching WinRAR.
fn parse_period(s: &str) -> u64 {
    let mut secs: u64 = 0;
    let mut num = String::new();
    let mut seen_unit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let mult = match ch {
            'd' => 86400,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return 0,
        };
        seen_unit = true;
        let n: u64 = num.parse().unwrap_or(0);
        secs = secs.saturating_add(n.saturating_mul(mult));
        num.clear();
    }
    if !num.is_empty() || !seen_unit {
        return 0; // trailing digits without a unit, or no unit at all
    }
    secs
}

/// Read one of the three file timestamps as unix nanoseconds. Access time
/// is not exposed by std on Windows, so it falls back to the mtime there.
pub(crate) fn file_time(meta: &std::fs::Metadata, kind: TimeKind) -> u128 {
    let t = match kind {
        TimeKind::Modified => meta.modified(),
        TimeKind::Created => meta.created(),
        TimeKind::Accessed => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(meta.atime() as u64))
            }
            #[cfg(not(unix))]
            {
                meta.modified()
            }
        }
    };
    t.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
