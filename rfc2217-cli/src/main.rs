//! `rfc2217`: RFC 2217 (Telnet COM Port Control) serial ports on the command
//! line, built on the [`rfc2217`] crate.

use std::process::ExitCode;

use clap::Parser;

mod cli;
mod connect;
// Linux and macOS only: other Unix systems lack calls it needs (`ptsname`
// in rustix) or behave differently (FreeBSD caps a pty's speed), untested.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod pty;
mod session;
mod shutdown;
mod target;
#[cfg(unix)]
mod term;
mod ui;

use cli::{Cli, Command};

/// How the process ends. The codes are part of the CLI's interface: scripts
/// check them, so they only change in a minor release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    /// 0: the user quit, the input ended, or the bridge closed the session.
    Ok,
    /// 1: an I/O error during the session.
    Io,
    /// 2: a usage error (clap uses 2 for its own as well).
    // Only the paths for other systems (an interactive `connect` on Windows,
    // `pty` beyond Linux and macOS) construct it.
    #[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
    Usage,
    /// 3: the host could not be resolved or did not accept the connection.
    Connect,
    /// 4: negotiation failed: the server refused RFC 2217, never answered,
    /// or (with `--strict`) answered with other values or not at all.
    Negotiation,
    /// 128 + N: stopped by signal N.
    Signal(i32),
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        match exit {
            Exit::Ok => Self::SUCCESS,
            Exit::Io => Self::from(1),
            Exit::Usage => Self::from(2),
            Exit::Connect => Self::from(3),
            Exit::Negotiation => Self::from(4),
            Exit::Signal(n) => Self::from(u8::try_from(128 + n).unwrap_or(u8::MAX)),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    ui::init(cli.verbose, cli.quiet);
    shutdown::install();
    let exit = match cli.command {
        Command::Connect(args) => connect::run(args),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Command::Pty(args) => pty::run(args),
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Command::Pty(_) => {
            ui::error(if cfg!(windows) {
                "pty needs Linux or macOS; on Windows, use a virtual COM port driver"
            } else {
                "pty needs Linux or macOS"
            });
            Exit::Usage
        }
    };
    // Every way out of a subcommand comes back here; signals and panics run
    // the same cleanup from their own paths (see `shutdown`).
    shutdown::run_all();
    exit.into()
}
