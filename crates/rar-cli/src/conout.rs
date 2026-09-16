//! Console-aware text output for the CLI binaries.
//!
//! Windows consoles run a code page (cp936, cp1252, ...) and `println!`
//! hands them UTF-8 bytes through `WriteFile`, so non-ASCII member names and
//! messages render as mojibake. These writers detect a console handle with
//! `GetConsoleMode`, decode the message as UTF-8 and re-encode it to UTF-16
//! for `WriteConsoleW`; the console maps that to its own code page and font,
//! and the process never calls `SetConsoleOutputCP`, so the user's console
//! is left exactly as it was found. Redirected handles (files, pipes) keep
//! the plain byte path through `WriteFile`.
//!
//! The binary crates pull this module in with `#[macro_use] mod conout;` as
//! their first module declaration, which textually shadows the four prelude
//! macros for the rest of the crate; existing `println!`/`eprintln!` call
//! sites therefore route through here without any changes. clap's own
//! help/error output is not affected: it writes ASCII/byte output through
//! its own stdout channel.
//!
//! Data output (`-so` extraction, `p`) stays on the raw byte path: `ops`
//! writes payload bytes straight to `std::io::stdout()` and is deliberately
//! not routed through these macros.

use std::io::Write;

#[cfg(any(windows, test))]
const CONSOLE_CHUNK: usize = 8192;

/// Write `bytes` to stdout, converting to UTF-16 when stdout is a console.
pub fn write_stdout(bytes: &[u8]) {
    if let Err(error) = write_stdout_impl(bytes) {
        handle_write_error(error, "stdout");
    }
}

/// Write `bytes` to stderr, converting to UTF-16 when stderr is a console.
pub fn write_stderr(bytes: &[u8]) {
    if let Err(error) = write_stderr_impl(bytes) {
        handle_write_error(error, "stderr");
    }
}

/// A closed pipe (`rar l big.rar | head -1`) is not an output failure worth
/// a panic: the reader is gone on purpose. Every other error keeps the loud
/// behavior so a full disk or a broken console handle is not swallowed.
fn handle_write_error(error: std::io::Error, target: &str) {
    if error.kind() != std::io::ErrorKind::BrokenPipe {
        panic!("failed printing to {target}: {error}");
    }
}

/// Format `args` to stdout without a trailing newline.
#[allow(dead_code)]
pub fn print_stdout(args: std::fmt::Arguments<'_>) {
    write_stdout(std::fmt::format(args).as_bytes());
}

/// Format `args` to stderr without a trailing newline.
#[allow(dead_code)]
pub fn print_stderr(args: std::fmt::Arguments<'_>) {
    write_stderr(std::fmt::format(args).as_bytes());
}

/// Format `args` to stdout and append a newline.
pub fn println_stdout(args: std::fmt::Arguments<'_>) {
    let mut text = std::fmt::format(args);
    text.push('\n');
    write_stdout(text.as_bytes());
}

/// Format `args` to stderr and append a newline.
pub fn println_stderr(args: std::fmt::Arguments<'_>) {
    let mut text = std::fmt::format(args);
    text.push('\n');
    write_stderr(text.as_bytes());
}

#[allow(unused_macros)]
macro_rules! print {
    () => {
        $crate::conout::print_stdout(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::conout::print_stdout(format_args!($($arg)*))
    };
}

macro_rules! println {
    () => {
        $crate::conout::println_stdout(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::conout::println_stdout(format_args!($($arg)*))
    };
}

#[allow(unused_macros)]
macro_rules! eprint {
    () => {
        $crate::conout::print_stderr(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::conout::print_stderr(format_args!($($arg)*))
    };
}

macro_rules! eprintln {
    () => {
        $crate::conout::println_stderr(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::conout::println_stderr(format_args!($($arg)*))
    };
}

fn write_stdout_impl(bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if test_capture::record(bytes) {
        return Ok(());
    }
    #[cfg(windows)]
    if let Some(handle) = console_handle(windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE) {
        return write_console(handle, bytes);
    }
    std::io::stdout().lock().write_all(bytes)
}

fn write_stderr_impl(bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if test_capture::record(bytes) {
        return Ok(());
    }
    #[cfg(windows)]
    if let Some(handle) = console_handle(windows_sys::Win32::System::Console::STD_ERROR_HANDLE) {
        return write_console(handle, bytes);
    }
    std::io::stderr().lock().write_all(bytes)
}

#[cfg(windows)]
fn console_handle(std_handle: u32) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle};

    let handle = unsafe { GetStdHandle(std_handle) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut mode = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return None;
    }
    Some(handle)
}

#[cfg(windows)]
fn write_console(
    handle: windows_sys::Win32::Foundation::HANDLE,
    bytes: &[u8],
) -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::WriteConsoleW;

    let text = String::from_utf8_lossy(bytes);
    let units: Vec<u16> = text.encode_utf16().collect();
    for chunk in console_chunks(&units) {
        let mut offset = 0;
        while offset < chunk.len() {
            let mut written = 0u32;
            let ok = unsafe {
                WriteConsoleW(
                    handle,
                    chunk[offset..].as_ptr(),
                    (chunk.len() - offset) as u32,
                    &mut written,
                    std::ptr::null(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if written == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            offset += written as usize;
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn console_chunks(units: &[u16]) -> Vec<&[u16]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < units.len() {
        let mut end = (start + CONSOLE_CHUNK).min(units.len());
        if end < units.len()
            && (0xD800..=0xDBFF).contains(&units[end - 1])
            && (0xDC00..=0xDFFF).contains(&units[end])
        {
            end -= 1;
        }
        chunks.push(&units[start..end]);
        start = end;
    }
    chunks
}

#[cfg(test)]
pub(crate) mod test_capture {
    use std::sync::{Mutex, MutexGuard};

    static CAPTURE: Mutex<Option<Vec<u8>>> = Mutex::new(None);
    static SERIAL: Mutex<()> = Mutex::new(());

    pub(crate) fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn start() {
        *CAPTURE.lock().unwrap_or_else(|error| error.into_inner()) = Some(Vec::new());
    }

    pub(crate) fn take() -> Option<Vec<u8>> {
        CAPTURE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    pub(super) fn record(bytes: &[u8]) -> bool {
        let mut capture = CAPTURE.lock().unwrap_or_else(|error| error.into_inner());
        match capture.as_mut() {
            Some(buffer) => {
                buffer.extend_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::console_chunks;
    use super::test_capture::{self, serial};

    #[test]
    fn console_chunks_respect_the_size_cap() {
        let units = vec![b'a' as u16; 20_000];
        let lengths: Vec<usize> = console_chunks(&units)
            .iter()
            .map(|chunk| chunk.len())
            .collect();
        assert_eq!(lengths, vec![8192, 8192, 3616]);
    }

    #[test]
    fn console_chunks_do_not_split_surrogate_pairs() {
        let text = format!("{}😀", "a".repeat(8191));
        let units: Vec<u16> = text.encode_utf16().collect();
        let chunks = console_chunks(&units);
        let lengths: Vec<usize> = chunks.iter().map(|chunk| chunk.len()).collect();
        assert_eq!(lengths, vec![8191, 2]);
        let joined: Vec<u16> = chunks.concat();
        assert_eq!(String::from_utf16(&joined).unwrap(), text);
    }

    #[test]
    fn println_routes_through_conout() {
        let _serial = serial();
        test_capture::start();
        println!("é");
        let captured = test_capture::take().expect("capture was on");
        assert_eq!(String::from_utf8(captured).unwrap(), "é\n");
    }

    #[test]
    fn eprintln_routes_through_conout() {
        let _serial = serial();
        test_capture::start();
        eprintln!("错误");
        let captured = test_capture::take().expect("capture was on");
        assert_eq!(String::from_utf8(captured).unwrap(), "错误\n");
    }

    #[test]
    fn print_and_eprint_route_through_conout() {
        let _serial = serial();
        test_capture::start();
        print!("a");
        eprint!("b");
        let captured = test_capture::take().expect("capture was on");
        assert_eq!(captured, b"ab");
    }

    /// A broken pipe must not panic (`rar l archive | head -1`).
    #[test]
    fn broken_pipe_is_swallowed() {
        super::handle_write_error(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe"),
            "stdout",
        );
    }

    /// Any other write failure keeps the loud behavior.
    #[test]
    #[should_panic(expected = "failed printing to stdout")]
    fn other_write_errors_still_panic() {
        super::handle_write_error(std::io::Error::other("disk full"), "stdout");
    }
}
