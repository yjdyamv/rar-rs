//! Time/size/mask filters shared by update, list and test.

/// Normalize a path argument into an archive name: relative paths stay as
/// given, absolute paths drop the leading slash (like `rar`).
pub(crate) fn arg_to_name(arg: &str) -> String {
    crate::name_policy::arg_to_name(arg)
}

/// Read one mask per line from a filter list file (like `-x@listfile`);
/// blank lines are skipped. Uses the shared list-file decoder, so UTF-8
/// BOMs, UTF-16 lists and legacy single-byte encodings behave like `@`
/// list files.
pub(crate) fn read_mask_file(path: &str) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read mask list {path}: {e}"))?;
    Ok(crate::listfile::decode(&bytes)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Parse a WinRAR date (`-ta`/`-tb`) into unix seconds. Accepts
/// `YYYY[MM[DD[HH[MM[SS]]]]]`; missing trailing parts default to their
/// minimum (month/day 01, time 00:00:00). The date is a *local* civil date
/// (like `-tk`), so it is resolved through the current UTC offset before it
/// is compared against file mtimes.
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
    let local = rar_rs::time::local_civil_to_system_time(y, m, d, hh, mm, ss);
    let secs = match local.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    };
    u32::try_from(secs).map_err(|_| format!("date out of range: {s}"))
}

/// Which file timestamp a `-ta`/`-tb`/`-tn`/`-to` filter compares (`m`/`c`/`a`
/// modifiers; `m` is the default when none is named).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum TimeKind {
    Modified,
    Created,
    Accessed,
}

impl TimeKind {
    /// Slot in the per-kind filter table (`m`/`c`/`a`).
    pub(crate) const fn index(self) -> usize {
        match self {
            TimeKind::Modified => 0,
            TimeKind::Created => 1,
            TimeKind::Accessed => 2,
        }
    }
}

/// Parse the leading `m`/`c`/`a`/`o` modifiers of a time filter.
///
/// Returns every named kind (WinRAR's `-tnmc30d` applies the period to both
/// mtime and ctime), whether the `o` modifier asked for OR logic, and the
/// byte offset where the date/period starts. No kind is the default mtime.
fn parse_time_modifiers(s: &str) -> (Vec<TimeKind>, bool, usize) {
    let mut kinds = Vec::new();
    let mut or = false;
    let mut idx = 0;
    for ch in s.chars() {
        match ch {
            'm' => kinds.push(TimeKind::Modified),
            'c' => kinds.push(TimeKind::Created),
            'a' => kinds.push(TimeKind::Accessed),
            'o' => or = true,
            _ => break,
        }
        idx += 1;
    }
    if kinds.is_empty() {
        kinds.push(TimeKind::Modified);
    }
    (kinds, or, idx)
}

/// Parse a WinRAR `-tn`/`-to` filter: optional leading `m`/`c`/`a`/`o`
/// modifiers followed by a period `[<ndays>d][<nhours>h][<nminutes>m][<nseconds>s]`.
/// Returns the time kinds, whether the `o` modifier selected OR logic, and
/// the period in seconds. Like WinRAR, an empty or unparsable period is
/// treated as 0 seconds.
pub(crate) fn parse_period_filter(s: &str) -> (Vec<TimeKind>, bool, u64) {
    let (kinds, or, idx) = parse_time_modifiers(s);
    (kinds, or, parse_period(&s[idx..]))
}

/// Parse a WinRAR `-ta`/`-tb` filter: the same modifiers followed by a
/// `YYYYMMDDHHMMSS` date (separators allowed, trailing fields omitted).
pub(crate) fn parse_date_filter(s: &str) -> Result<(Vec<TimeKind>, bool, u32), String> {
    let (kinds, or, idx) = parse_time_modifiers(s);
    parse_rar_date(&s[idx..]).map(|date| (kinds, or, date))
}

/// The comparison a time filter applies.
#[derive(Clone, Copy)]
pub(crate) enum TimeBound {
    /// `-ta<date>`: the time is at or after this Unix second (WinRAR
    /// includes a file matching the date exactly).
    After(u32),
    /// `-tb<date>`: the time is before it.
    Before(u32),
    /// `-tn<period>`: the time is within the last `period` seconds.
    Newer(u64),
    /// `-to<period>`: the time is older than `period` seconds.
    Older(u64),
}

/// Whether `meta`'s `kind` timestamp satisfies `bound`.
pub(crate) fn bound_matches(
    kind: TimeKind,
    bound: TimeBound,
    meta: &std::fs::Metadata,
    now_ns: u128,
) -> bool {
    const NS: u128 = 1_000_000_000;
    let t = file_time(meta, kind);
    match bound {
        TimeBound::After(secs) => t >= u128::from(secs) * NS,
        TimeBound::Before(secs) => t < u128::from(secs) * NS,
        TimeBound::Newer(period) => t >= now_ns.saturating_sub(u128::from(period) * NS),
        TimeBound::Older(period) => t < now_ns.saturating_sub(u128::from(period) * NS),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// WinRAR's modifiers name several time kinds (`-tnmc30d`) and the `o`
    /// flag selects OR logic; one kind is the default.
    #[test]
    fn modifiers_collect_kinds_and_or() {
        let (kinds, or, period) = parse_period_filter("mc30d");
        assert!(!or);
        assert_eq!(period, 30 * 86400);
        assert_eq!(kinds, vec![TimeKind::Modified, TimeKind::Created]);

        let (kinds, or, _) = parse_period_filter("co30d");
        assert!(or);
        assert_eq!(kinds, vec![TimeKind::Created]);

        let (kinds, or, _) = parse_period_filter("1h");
        assert!(!or);
        assert_eq!(kinds, vec![TimeKind::Modified]);
    }

    #[test]
    fn date_filter_parses_modifiers_and_separators() {
        let (kinds, or, date) = parse_date_filter("mc2019-02-15").unwrap();
        assert!(!or);
        assert_eq!(kinds, vec![TimeKind::Modified, TimeKind::Created]);
        assert!(date > 1_500_000_000);
    }

    /// Every named kind must pass: a file with an old mtime but a fresh ctime
    /// fails `-tamc` while `-tac` passes.
    #[test]
    fn bound_matches_checks_the_named_kind() {
        let dir = std::env::temp_dir().join(format!("rar-timefilter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, b"x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let yesterday = u32::try_from(now / 1_000_000_000 - 86400).unwrap();

        assert!(!bound_matches(
            TimeKind::Modified,
            TimeBound::After(yesterday),
            &meta,
            now
        ));
        // The creation/ctime is fresh, so a ctime-only filter passes; the old
        // mtime is what makes the combined `-tamc` fail.
        assert!(bound_matches(
            TimeKind::Created,
            TimeBound::After(yesterday),
            &meta,
            now
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
