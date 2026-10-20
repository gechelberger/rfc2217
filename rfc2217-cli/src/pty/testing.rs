//! Test helpers: a local program's end of the pty, reads with deadlines, and
//! temporary directories.

// Most pty tests check behaviour verified on Linux only, so elsewhere many of
// these helpers have no user.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rfc2217::PortSettings;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::termios::{self, OptionalActions, Termios};

use super::sys::Pty;

/// How long "nothing happens" is watched for.
pub(super) const SHORT: Duration = Duration::from_millis(100);

/// The longest any expected event may take.
const DEADLINE: Duration = Duration::from_secs(5);

/// 8N1 without flow control at `baud_rate` (0: keep).
pub(super) fn settings(baud_rate: u32) -> PortSettings {
    PortSettings {
        baud_rate,
        ..PortSettings::default()
    }
}

/// Whether `fd` polls readable (or hung up) within `timeout`.
pub(super) fn readable(fd: BorrowedFd<'_>, timeout: Duration) -> bool {
    let timeout = Timespec::try_from(timeout).unwrap();
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    rustix::event::poll(&mut fds, Some(&timeout)).unwrap() > 0
}

/// Reads `len` bytes from the master, waiting at most [`DEADLINE`] for each
/// chunk.
pub(super) fn read_master(pty: &Pty, len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0; 64];
    while out.len() < len {
        assert!(readable(pty.master(), DEADLINE), "timed out after {out:?}");
        let n = (&*pty).read(&mut buf).unwrap();
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// A local program: the device opened by its path, non-blocking.
#[derive(Debug)]
pub(super) struct Program(OwnedFd);

impl Program {
    pub(super) fn open(pty: &Pty) -> Self {
        Self::open_with(pty.path(), OFlags::RDWR)
    }

    pub(super) fn open_read_only(pty: &Pty) -> Self {
        Self::open_with(pty.path(), OFlags::RDONLY)
    }

    /// [`open`](Self::open), returning the error instead of panicking.
    pub(super) fn try_open(pty: &Pty) -> rustix::io::Result<Self> {
        Self::try_open_with(pty.path(), OFlags::RDWR)
    }

    fn open_with(path: &Path, access: OFlags) -> Self {
        Self::try_open_with(path, access).unwrap()
    }

    fn try_open_with(path: &Path, access: OFlags) -> rustix::io::Result<Self> {
        let flags = access | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC;
        rustix::fs::open(path, flags, Mode::empty()).map(Self)
    }

    /// Exclusive mode (`TIOCEXCL`), as serialport-rs sets it on every open.
    pub(super) fn set_exclusive(&self) {
        termios::ioctl_tiocexcl(&self.0).unwrap();
    }

    /// Suspends the device's output (`tcflow(TCOOFF)`).
    pub(super) fn suspend_output(&self) {
        termios::tcflow(&self.0, termios::Action::OOff).unwrap();
    }

    pub(super) fn termios(&self) -> Termios {
        termios::tcgetattr(&self.0).unwrap()
    }

    /// tcgetattr, `f`, tcsetattr, as a program changes its settings.
    pub(super) fn change(&self, f: impl FnOnce(&mut Termios)) {
        self.try_change(f).unwrap();
    }

    /// [`change`](Self::change) without checking tcsetattr's result.
    pub(super) fn try_change(&self, f: impl FnOnce(&mut Termios)) -> rustix::io::Result<()> {
        let mut t = self.termios();
        f(&mut t);
        termios::tcsetattr(&self.0, OptionalActions::Now, &t)
    }

    pub(super) fn write(&self, data: &[u8]) {
        assert_eq!(rustix::io::write(&self.0, data).unwrap(), data.len());
    }

    /// Reads until `len` bytes have arrived, for at most [`DEADLINE`].
    pub(super) fn read_exactly(&self, len: usize) -> Vec<u8> {
        let out = self.read_until(Instant::now() + DEADLINE, len);
        assert_eq!(out.len(), len, "timed out after {out:?}");
        out
    }

    /// Everything that arrives within `period`.
    pub(super) fn read_for(&self, period: Duration) -> Vec<u8> {
        self.read_until(Instant::now() + period, usize::MAX)
    }

    fn read_until(&self, deadline: Instant, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0; 256];
        while out.len() < len {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !readable(self.0.as_fd(), left) {
                break;
            }
            match rustix::io::read(&self.0, &mut buf) {
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(rustix::io::Errno::AGAIN) => {}
                Err(e) => panic!("program read: {e}"),
            }
        }
        out
    }
}

/// A directory under the system's temporary directory, removed on drop.
#[derive(Debug)]
pub(super) struct TempDir(PathBuf);

impl TempDir {
    pub(super) fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rfc2217-cli-test-{}-{n}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
