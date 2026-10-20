//! Messages for the person at the terminal, and the diagnostic logs: both go
//! to stderr, so stdout carries nothing but the port's data.
//!
//! While the terminal is in raw mode its output processing is off, and a bare
//! LF moves down without returning to the start of the line: [`set_raw`]
//! switches every line written here to CR LF.

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The target of the CLI's own log events. The binary's crate is named
/// `rfc2217`, like the library, so the default target (the module path)
/// would put them under the library's filter: every event names this one.
pub(crate) const LOG: &str = "rfc2217_cli";

static RAW: AtomicBool = AtomicBool::new(false);
/// In raw mode: the terminal's cursor is not at the start of a line.
static MID_LINE: AtomicBool = AtomicBool::new(false);
static LEVEL: AtomicU8 = AtomicU8::new(Level::Normal as u8);

/// How much the user asked to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum Level {
    /// `-q`: errors only.
    Quiet,
    /// The default: errors, warnings and notices.
    Normal,
    /// `-v` and more: also the bridge's events.
    Verbose,
}

/// Sets up the message level and the diagnostic logs for `-v` (CLI debug
/// logs), `-vv` (the library's debug logs) and `-vvv` (the library's trace).
pub(crate) fn init(verbose: u8, quiet: bool) {
    let level = match (quiet, verbose) {
        (true, _) => Level::Quiet,
        (false, 0) => Level::Normal,
        (false, _) => Level::Verbose,
    };
    LEVEL.store(level as u8, Ordering::Relaxed);
    let (cli, lib) = match verbose {
        0 => (LevelFilter::WARN, LevelFilter::WARN),
        1 => (LevelFilter::DEBUG, LevelFilter::INFO),
        2 => (LevelFilter::DEBUG, LevelFilter::DEBUG),
        _ => (LevelFilter::TRACE, LevelFilter::TRACE),
    };
    let filter = Targets::new()
        .with_target(LOG, cli)
        .with_target("rfc2217", lib);
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(Stderr)
        .with_ansi(false)
        .with_target(true)
        .without_time();
    tracing_subscriber::registry().with(fmt).with(filter).init();
}

pub(crate) fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Quiet,
        1 => Level::Normal,
        _ => Level::Verbose,
    }
}

/// Raw mode on or off for everything written to stderr from now on.
// Only Unix has a raw mode (see term.rs).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn set_raw(on: bool) {
    RAW.store(on, Ordering::Relaxed);
}

/// In raw mode, the terminal shows `bytes` (the port's output, or the local
/// echo): the next message starts on a line of its own unless they ended
/// one. A CR alone is not enough: the message would overwrite the line.
// Only Unix has a raw mode (see term.rs).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn shown(bytes: &[u8]) {
    if let Some(&last) = bytes.last() {
        MID_LINE.store(last != b'\n', Ordering::Relaxed);
    }
}

/// A notice, e.g. "connected" or a status report. Hidden by `-q`.
pub(crate) fn notice(msg: impl fmt::Display) {
    if level() >= Level::Normal {
        line(format_args!("rfc2217: {msg}"));
    }
}

/// A warning. Hidden by `-q`.
pub(crate) fn warn(msg: impl fmt::Display) {
    if level() >= Level::Normal {
        line(format_args!("rfc2217: warning: {msg}"));
    }
}

/// An error, always shown.
pub(crate) fn error(msg: impl fmt::Display) {
    line(format_args!("rfc2217: error: {msg}"));
}

/// Only with `-v`: what the bridge reported.
pub(crate) fn verbose(msg: impl fmt::Display) {
    if level() >= Level::Verbose {
        line(format_args!("rfc2217: {msg}"));
    }
}

fn line(args: fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    // In raw mode the cursor may be anywhere on a line of the port's output:
    // then start a fresh line first.
    let _ = if RAW.load(Ordering::Relaxed) {
        let lead = if MID_LINE.swap(false, Ordering::Relaxed) {
            "\r\n"
        } else {
            ""
        };
        write!(stderr, "{lead}{args}\r\n")
    } else {
        writeln!(stderr, "{args}")
    };
    let _ = stderr.flush();
}

/// stderr for the log formatter, with LF turned into CR LF in raw mode.
#[derive(Debug, Clone, Copy)]
struct Stderr;

impl<'a> MakeWriter<'a> for Stderr {
    type Writer = StderrLines;

    fn make_writer(&'a self) -> Self::Writer {
        StderrLines
    }
}

#[derive(Debug)]
struct StderrLines;

impl Write for StderrLines {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut stderr = io::stderr().lock();
        if RAW.load(Ordering::Relaxed) {
            for chunk in buf.split_inclusive(|&b| b == b'\n') {
                match chunk.strip_suffix(b"\n") {
                    Some(text) => {
                        stderr.write_all(text)?;
                        stderr.write_all(b"\r\n")?;
                    }
                    None => stderr.write_all(chunk)?,
                }
            }
        } else {
            stderr.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}
