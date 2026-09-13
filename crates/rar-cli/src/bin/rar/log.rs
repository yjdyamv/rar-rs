//! `-log[fmt][=name]`: write archive and processed file names to log files.
//!
//! `A` logs archive names (every volume of a volume set), `F` logs the
//! processed member names, `P` appends to an existing log and `U` writes
//! UTF-16LE. When neither `A` nor `F` is present, `A` is assumed; each
//! switch writes its own file, so repeated `-log` switches are allowed.

use std::io::Write;
use std::path::PathBuf;

use crate::common;
use crate::error;

/// One parsed `-log` switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogSpec {
    pub archives: bool,
    pub files: bool,
    pub append: bool,
    pub unicode: bool,
    pub path: PathBuf,
}

/// Parse the global `--log` values (one per switch occurrence).
pub(crate) fn specs_from(misc: &common::MiscSwitches) -> Result<Vec<LogSpec>, String> {
    misc.log_specs.iter().map(|spec| parse_spec(spec)).collect()
}

fn parse_spec(spec: &str) -> Result<LogSpec, String> {
    let (flags, name) = match spec.split_once('=') {
        Some((flags, name)) => (flags, Some(name)),
        None => (spec, None),
    };
    let mut archives = false;
    let mut files = false;
    let mut append = false;
    let mut unicode = false;
    for flag in flags.chars() {
        match flag {
            'A' | 'a' => archives = true,
            'F' | 'f' => files = true,
            'P' | 'p' => append = true,
            'U' | 'u' => unicode = true,
            other => return Err(format!("Unknown option: log{other}")),
        }
    }
    if !archives && !files {
        archives = true;
    }
    let path = name
        .filter(|name| !name.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rarinfo.log"));
    Ok(LogSpec {
        archives,
        files,
        append,
        unicode,
        path,
    })
}

/// Write the log lines for every parsed switch (a missing log file is
/// created; `P` appends instead of truncating).
pub(crate) fn write_logs(
    specs: &[LogSpec],
    archives: &[PathBuf],
    files: &[String],
) -> Result<(), error::CliError> {
    for spec in specs {
        let mut text = String::new();
        if spec.archives {
            for archive in archives {
                text.push_str(&archive.display().to_string());
                text.push_str("\r\n");
            }
        }
        if spec.files {
            for file in files {
                text.push_str(file);
                text.push_str("\r\n");
            }
        }
        if text.is_empty() {
            continue;
        }
        let bytes: Vec<u8> = if spec.unicode {
            let mut bytes = Vec::with_capacity(text.len() * 2);
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        } else {
            text.into_bytes()
        };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true);
        if spec.append {
            options.append(true);
        } else {
            options.truncate(true);
        }
        options
            .open(&spec.path)
            .and_then(|mut file| file.write_all(&bytes))
            .map_err(|err| {
                error::CliError::with_code(
                    format!("Cannot create {}: {err}", spec.path.display()),
                    error::EXIT_CREATE,
                )
            })?;
    }
    Ok(())
}
