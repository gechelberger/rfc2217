//! `rfc2217 pty`: a remote serial port as a local pseudo-terminal.
//!
//! The OS side, one module each:
//! - [`sys::Pty`]: the pty, raw, with its slave held open for the bridge's
//!   whole life; reads and writes on the master, and the input flush.
//! - [`settings`]: the speed, stop bits and flow control a local program
//!   sets, found by polling the termios ([`settings::SettingsWatch`]), and
//!   the bridge's rate written back without counting as a change.
//! - [`presence`]: whether a program has the device open (Linux; macOS
//!   cannot tell and assumes it is).
//! - [`link`]: the `--link` symlink.
//!
//! What a pty cannot carry, on Linux and macOS alike: DTR, RTS, BREAK and
//! the modem lines do not exist on a pty (their ioctls fail with ENOTTY, a
//! BREAK is accepted and dropped), and nothing paces writes at the baud rate.
//! Data bits and parity never come from the program on Linux (the driver
//! forces 8N); they are the command line's.

use crate::Exit;
use crate::cli::PtyArgs;

// The bridge (the next stage of `run`) is what uses these modules; until it
// is written only their tests do.
#[allow(dead_code)]
mod link;
#[allow(dead_code)]
mod presence;
#[allow(dead_code)]
mod settings;
#[allow(dead_code)]
mod sys;
#[cfg(test)]
mod testing;

pub(crate) fn run(args: PtyArgs) -> Exit {
    let _ = args;
    todo!("pty")
}
