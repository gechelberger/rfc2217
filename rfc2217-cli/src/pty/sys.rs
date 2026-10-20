//! The pseudo-terminal: the master end the bridge reads and writes, and the
//! slave end that a local program opens by its path.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use rfc2217::PortSettings;
use rustix::fs::{Mode, OFlags};
use rustix::pty::OpenptFlags;
use rustix::termios::{self, OptionalActions, QueueSelector, Termios};

use super::settings;

/// A new pty whose slave end this process holds open until it is dropped.
///
/// Holding the slave keeps the pty well-behaved while no program has it open:
/// - Linux: once the last slave fd closes, the master reports POLLHUP and its
///   reads fail with EIO until the next open, so a poll loop on the master
///   spins, and nothing on the master announces the next open.
/// - macOS: the first open of a replica resets its termios to the defaults,
///   and the master refuses most ioctls while no replica is open.
///
/// The held fd is never read: everything written to the master stays queued
/// for the program. The termios ioctls go through it, not through the
/// master, on every platform. It is the device the program configures, so it
/// shows exactly what the program set. On Linux a termios ioctl on the master
/// is redirected to the slave anyway (drivers/tty/tty_ioctl.c), so the two
/// agree. macOS refuses termios ioctls on the master while no replica is
/// open, and FreeBSD answers them on the master only in its Linux mode.
///
/// `&Pty` implements [`io::Read`] and [`io::Write`] on the master (as `&File`
/// does), so one thread can read while another writes. The master starts in
/// blocking mode; see [`Pty::set_nonblocking`].
#[derive(Debug)]
pub(crate) struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
    path: PathBuf,
}

impl Pty {
    /// Opens a new pty in raw mode (no echo, no output processing, no
    /// software flow control) with the speed, stop bits and flow control of
    /// `settings`.
    ///
    /// A baud rate of 0 (`--baud keep`) leaves the pty's default speed
    /// (38400 on Linux, 9600 on macOS) until the bridge's rate is written
    /// with [`SettingsWatch::set_speed`]. Data bits and parity are never
    /// written: a Linux pty forces 8 data bits and no parity whatever is
    /// asked, and 8N on every platform passes each byte through unchanged.
    /// 1.5 stop bits and DCD or DSR flow control have no termios encoding a
    /// pty can hold, so they show as 1 stop bit and no flow control.
    ///
    /// The device belongs to the user running this process, with the mode
    /// the system gives new terminals (0600 or 0620): on Linux devpts sets
    /// both when the pty is created, on macOS `grantpt` does.
    ///
    /// [`SettingsWatch::set_speed`]: super::settings::SettingsWatch::set_speed
    pub(crate) fn open(settings: &PortSettings) -> io::Result<Self> {
        // O_NOCTTY on both ends: the pty must never become this process's
        // controlling terminal (as it would for a session leader without
        // one), or its signals would reach the bridge: a hangup, or SIGINT for
        // a ^C from the remote device once a program turns ISIG on.
        #[cfg(target_os = "linux")]
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY;
        let master = rustix::pty::openpt(flags).map_err(failed("opening a new pty"))?;
        // posix_openpt has no O_CLOEXEC outside Linux. A child process that
        // inherited the master would keep the pty alive after the bridge.
        #[cfg(not(target_os = "linux"))]
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)
            .map_err(failed("setting close-on-exec on the pty"))?;
        rustix::pty::grantpt(&master).map_err(failed("granting access to the pty"))?;
        rustix::pty::unlockpt(&master).map_err(failed("unlocking the pty"))?;
        let name = rustix::pty::ptsname(&master, Vec::new()).map_err(failed("naming the pty"))?;
        let path = PathBuf::from(OsString::from_vec(name.into_bytes()));
        // O_NONBLOCK matters only to the open: without it XNU's replica open
        // waits for carrier. Nothing ever reads or writes this fd.
        let slave = rustix::fs::open(
            &path,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(failed(format_args!("opening {}", path.display())))?;
        let pty = Self {
            master,
            slave,
            path,
        };
        let mut termios = pty.termios()?;
        settings::seed(&mut termios, settings).map_err(failed(format_args!(
            "setting {} to {} baud",
            pty.path.display(),
            settings.baud_rate
        )))?;
        pty.set_termios(&termios)?;
        Ok(pty)
    }

    /// The device path a local program opens, e.g. `/dev/pts/3` or
    /// `/dev/ttys003`.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The master end, for `poll` alongside the socket.
    pub(crate) fn master(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    /// Switches the master between blocking and non-blocking reads and
    /// writes. Non-blocking, a write the program is too slow to take fails
    /// with [`io::ErrorKind::WouldBlock`] (a pty queues about 12 to 24 KiB
    /// per direction) instead of stalling the writer.
    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        let flags =
            rustix::fs::fcntl_getfl(&self.master).map_err(failed("reading the pty's flags"))?;
        let flags = if on {
            flags | OFlags::NONBLOCK
        } else {
            flags - OFlags::NONBLOCK
        };
        rustix::fs::fcntl_setfl(&self.master, flags).map_err(failed("setting the pty's flags"))
    }

    /// Drops what the bridge wrote and no program has read yet: the slave's
    /// input queue (`tcflush(TCIFLUSH)` through the held slave). Called when
    /// a program first opens the device, so it does not read bytes that
    /// arrived for an earlier program or for nobody.
    pub(crate) fn flush_input(&self) -> io::Result<()> {
        termios::tcflush(&self.slave, QueueSelector::IFlush)
            .map_err(failed(format_args!("flushing {}", self.path.display())))
    }

    /// The termios the local program sees.
    pub(super) fn termios(&self) -> io::Result<Termios> {
        termios::tcgetattr(&self.slave).map_err(failed(format_args!(
            "reading the settings of {}",
            self.path.display()
        )))
    }

    /// Replaces the termios at once (`TCSANOW`).
    pub(super) fn set_termios(&self, termios: &Termios) -> io::Result<()> {
        termios::tcsetattr(&self.slave, OptionalActions::Now, termios).map_err(failed(
            format_args!("changing the settings of {}", self.path.display()),
        ))
    }
}

impl io::Read for &Pty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(rustix::io::read(&self.master, buf)?)
    }
}

impl io::Write for &Pty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(rustix::io::write(&self.master, buf)?)
    }

    /// A pty has nothing to flush: a write lands in the slave's input queue.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Turns an errno into an [`io::Error`] that says what failed, e.g. "opening
/// /dev/pts/3: Permission denied (os error 13)".
pub(super) fn failed(what: impl fmt::Display) -> impl FnOnce(rustix::io::Errno) -> io::Error {
    let what = what.to_string();
    move |errno| {
        let e = io::Error::from(errno);
        io::Error::new(e.kind(), format!("{what}: {e}"))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::{Read, Write};
    use std::time::Duration;

    use rustix::termios::{ControlModes, InputModes, LocalModes, OutputModes};

    use super::*;
    use crate::pty::testing::{Program, SHORT, readable, settings};

    #[test]
    fn opens_a_device_the_user_can_use() {
        let pty = Pty::open(&settings(9600)).unwrap();
        let meta = std::fs::metadata(pty.path()).unwrap();
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        assert!(
            meta.file_type().is_char_device(),
            "{}",
            pty.path().display()
        );
        assert_eq!(meta.uid(), rustix::process::geteuid().as_raw());
        // Opening it read-write, as a program does, is the real test.
        let program = Program::open(&pty);
        program.write(b"hi");
        assert_eq!(read_master(&pty, 2), b"hi");
    }

    #[test]
    fn starts_raw_with_the_settings() {
        let pty = Pty::open(&rfc2217::PortSettings {
            baud_rate: 115_200,
            stop_bits: rfc2217::StopBits::Two,
            flow_control: rfc2217::FlowControl::RtsCts,
            ..settings(0)
        })
        .unwrap();
        let t = Program::open(&pty).termios();
        assert_eq!((t.input_speed(), t.output_speed()), (115_200, 115_200));
        assert!(
            t.control_modes
                .contains(ControlModes::CS8 | ControlModes::CSTOPB | ControlModes::CRTSCTS)
        );
        assert!(!t.control_modes.contains(ControlModes::PARENB));
        // The kernel's default pty termios has IXON on: raw mode clears it,
        // so 0x11 and 0x13 pass through as data.
        assert!(
            !t.input_modes
                .intersects(InputModes::IXON | InputModes::IXOFF | InputModes::ICRNL)
        );
        assert!(!t.output_modes.contains(OutputModes::OPOST));
        assert!(
            !t.local_modes
                .intersects(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG)
        );
    }

    #[test]
    fn keep_leaves_the_default_speed() {
        let pty = Pty::open(&settings(0)).unwrap();
        assert_eq!(pty.termios().unwrap().output_speed(), 38_400);
    }

    #[test]
    fn data_passes_both_ways_without_echo() {
        let pty = Pty::open(&settings(9600)).unwrap();
        let program = Program::open(&pty);
        (&pty).write_all(b"to\r\n\x11\x13\xff").unwrap();
        assert_eq!(program.read_exactly(7), b"to\r\n\x11\x13\xff");
        // Raw mode: nothing the bridge writes comes back to it.
        assert!(!readable(pty.master(), SHORT));
        program.write(b"from\n");
        assert_eq!(read_master(&pty, 5), b"from\n");
    }

    #[test]
    fn master_reads_survive_the_program_closing() {
        let pty = Pty::open(&settings(9600)).unwrap();
        pty.set_nonblocking(true).unwrap();
        // Before any program: nothing to read, and no error.
        assert_eq!(read_error(&pty), io::ErrorKind::WouldBlock);
        let program = Program::open(&pty);
        program.write(b"bye");
        drop(program);
        assert_eq!(read_master(&pty, 3), b"bye");
        // The held slave keeps the master from reporting a hangup (POLLHUP,
        // EIO) once the program has gone.
        assert!(!readable(pty.master(), SHORT));
        assert_eq!(read_error(&pty), io::ErrorKind::WouldBlock);
        let again = Program::open(&pty);
        again.write(b"again");
        assert_eq!(read_master(&pty, 5), b"again");
    }

    #[test]
    fn stale_input_reaches_the_next_program_unless_flushed() {
        // Without the flush, bytes written while nobody had the device open
        // are what the next program reads first.
        let pty = Pty::open(&settings(9600)).unwrap();
        (&pty).write_all(b"stale").unwrap();
        assert_eq!(Program::open(&pty).read_exactly(5), b"stale");

        let pty = Pty::open(&settings(9600)).unwrap();
        (&pty).write_all(b"stale").unwrap();
        pty.flush_input().unwrap();
        let program = Program::open(&pty);
        assert_eq!(program.read_for(SHORT), b"");
        (&pty).write_all(b"fresh").unwrap();
        assert_eq!(program.read_exactly(5), b"fresh");
    }

    fn read_master(pty: &Pty, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0; 64];
        while out.len() < len {
            assert!(
                readable(pty.master(), Duration::from_secs(5)),
                "timed out after {out:?}"
            );
            let n = (&*pty).read(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn read_error(pty: &Pty) -> io::ErrorKind {
        (&*pty).read(&mut [0; 16]).unwrap_err().kind()
    }
}
