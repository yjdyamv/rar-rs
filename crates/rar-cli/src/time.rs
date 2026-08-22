//! DOS/Unix date formatting and -ts spec parsing.

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
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
