//! Whether a local program has the device open.
//!
//! The bridge holds the slave open itself, so the master never sees a
//! program come or go. On Linux, inotify on the device path still reports
//! every open and close by others: [`OpenWatch`] counts them. macOS has no
//! inotify, nor another known way to see a device node being opened, so
//! there the answer is "unknown, assume open": [`OpenWatch::new`] returns
//! `None`, and data from the bridge is always written, to reach whichever
//! program opens the device next.
//!
//! What the bridge does with the answer: while no program has the device
//! open it drops data from the bridge (it would reach the next program,
//! stale) instead of writing it to the master. [`OpenWatch::wait`] flushes
//! the slave's input queue itself, on each first open and on each last
//! close, in the order that keeps the bridge's later writes out of the
//! flush. On each last close it also undoes the exclusive mode and the
//! suspended output a program may have left ([`Pty::reset_after_close`]).
//! On macOS both presumably survive as well (XNU clears them only on the
//! last close of the replica, from its source, untested), and with nothing
//! to say when the last program has gone they stay until the bridge ends.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use super::sys::Pty;

const UNKNOWN: u8 = 0;
const CLOSED: u8 = 1;
const OPEN: u8 = 2;

/// Whether a program has the device open, shared with the threads that write
/// to the master. Cheap to clone and to read.
#[derive(Debug, Clone)]
pub(crate) struct Presence(Arc<AtomicU8>);

impl Presence {
    /// For a platform (or a failed [`OpenWatch::new`]) that cannot tell:
    /// always counts as open.
    pub(crate) fn unknown() -> Self {
        Self(Arc::new(AtomicU8::new(UNKNOWN)))
    }

    /// Whether data from the bridge should be written to the master: false
    /// only when no program is known to have the device open.
    pub(crate) fn is_open(&self) -> bool {
        // Acquire: a write that follows a true here comes after the flush
        // that preceded the open (see `OpenWatch::wait`).
        self.0.load(Ordering::Acquire) != CLOSED
    }

    /// `Some(open)`, or `None` when it cannot be known.
    pub(crate) fn get(&self) -> Option<bool> {
        match self.0.load(Ordering::Acquire) {
            CLOSED => Some(false),
            OPEN => Some(true),
            _ => None,
        }
    }

    fn set(&self, state: u8) {
        self.0.store(state, Ordering::Release);
    }
}

/// A change in whether any program has the device open, from
/// [`OpenWatch::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// The first program opened the device; the input queue was flushed
    /// before the presence turned open.
    Opened,
    /// The last program closed it; what it left unread was flushed, and its
    /// exclusive mode and suspended output were undone.
    Closed,
    /// The kernel's event queue overflowed and opens or closes were lost:
    /// from now on the device counts as open ([`Presence::get`] is `None`).
    Lost,
}

/// Counts the opens and closes of the device by programs (Linux, inotify).
///
/// The count has one blind spot: inotify merges an event into the previous
/// one when both are identical and still unread, so two opens within the
/// same instant (before this thread reads the first) count as one. The
/// count can then reach zero while one of them still has the device open:
/// data is dropped until the next open, and that program's exclusive mode or
/// suspended output is undone. Two closes merged the same way
/// leave the device counted as open, the same as no detection at all.
///
/// Events arrive after the fact, so the flushes are not exact either. A
/// program that reads at once after opening can read bytes before the flush
/// on its open runs; there are few, since the flush on the last close
/// already emptied the queue and the bridge drops data from then on. Only a
/// write the bridge started just before seeing the close can be among them.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct OpenWatch {
    inotify: OwnedFd,
    opens: u32,
    lost: bool,
    presence: Presence,
}

#[cfg(target_os = "linux")]
impl OpenWatch {
    /// Starts counting. Create it right after [`Pty::open`], before the path
    /// is shown to anyone: the held slave opened before the watch, so it is
    /// not counted, and a program cannot have opened the device yet.
    ///
    /// An error (e.g. the per-user inotify limit) leaves the bridge without
    /// detection: carry on with [`Presence::unknown`].
    pub(crate) fn new(pty: &Pty) -> io::Result<Option<Self>> {
        use rustix::fs::inotify::{self, CreateFlags, WatchFlags};

        let what = format!("watching {} for programs", pty.path().display());
        let inotify = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)
            .map_err(super::sys::failed(&what))?;
        inotify::add_watch(&inotify, pty.path(), WatchFlags::OPEN | WatchFlags::CLOSE)
            .map_err(super::sys::failed(&what))?;
        Ok(Some(Self {
            inotify,
            opens: 0,
            lost: false,
            presence: Presence(Arc::new(AtomicU8::new(CLOSED))),
        }))
    }

    pub(crate) fn presence(&self) -> Presence {
        self.presence.clone()
    }

    /// Opens of the device by programs that have not closed it yet, as far
    /// as the events tell (see the type's documentation).
    pub(crate) fn opens(&self) -> u32 {
        self.opens
    }

    /// Readable when events are queued: for `poll` with other fds, then
    /// [`wait`](Self::wait) with a zero timeout.
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
    }

    /// Waits up to `timeout` (`None`: no limit) for opens and closes, and
    /// handles all that are queued. Returns the transitions they made, which
    /// is empty after a timeout, a signal, or opens and closes that did not
    /// start or end the device's use.
    ///
    /// On a first open, the input queue is flushed before the presence turns
    /// open; on the last close, the presence turns closed first. So nothing
    /// the bridge writes after seeing the device open is flushed. The last
    /// close also calls [`Pty::reset_after_close`]. A failed flush or reset
    /// is a warning (`ui::warn`), not an error.
    pub(crate) fn wait(
        &mut self,
        pty: &Pty,
        timeout: Option<Duration>,
    ) -> io::Result<Vec<Transition>> {
        use rustix::event::{PollFd, PollFlags, Timespec};
        use rustix::io::Errno;

        // A timeout too large for a timespec is as good as none.
        let timeout = timeout.and_then(|t| Timespec::try_from(t).ok());
        let mut fds = [PollFd::new(&self.inotify, PollFlags::IN)];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(0) | Err(Errno::INTR) => return Ok(Vec::new()),
            Ok(_) => {}
            Err(e) => return Err(super::sys::failed("waiting for programs")(e)),
        }
        let mut transitions = Vec::new();
        for event in self.read_events()? {
            self.handle(event, pty, &mut transitions);
        }
        Ok(transitions)
    }

    /// Everything queued, without blocking.
    fn read_events(&self) -> io::Result<Vec<rustix::fs::inotify::ReadFlags>> {
        use rustix::fs::inotify::Reader;
        use rustix::io::Errno;
        use std::mem::MaybeUninit;

        // Events for a watched file carry no name: 16 bytes each.
        let mut buf = [MaybeUninit::uninit(); 1024];
        let mut reader = Reader::new(&self.inotify, &mut buf);
        let mut events = Vec::new();
        loop {
            match reader.next() {
                Ok(event) => events.push(event.events()),
                Err(Errno::AGAIN) => return Ok(events),
                Err(Errno::INTR) => {}
                Err(e) => {
                    return Err(super::sys::failed(
                        "reading the pty's open and close events",
                    )(e));
                }
            }
        }
    }

    fn handle(
        &mut self,
        event: rustix::fs::inotify::ReadFlags,
        pty: &Pty,
        transitions: &mut Vec<Transition>,
    ) {
        use rustix::fs::inotify::ReadFlags;

        if self.lost {
            return;
        }
        if event.contains(ReadFlags::QUEUE_OVERFLOW) {
            self.lost = true;
            self.presence.set(UNKNOWN);
            transitions.push(Transition::Lost);
        } else if event.contains(ReadFlags::OPEN) {
            self.opens += 1;
            if self.opens == 1 {
                warn_on_error(pty.flush_input());
                self.presence.set(OPEN);
                transitions.push(Transition::Opened);
            }
        } else if event.intersects(ReadFlags::CLOSE_WRITE | ReadFlags::CLOSE_NOWRITE) {
            // Saturating: a merged open (see the type's documentation) can
            // leave one close more than opens.
            let was_open = self.opens > 0;
            self.opens = self.opens.saturating_sub(1);
            if was_open && self.opens == 0 {
                self.presence.set(CLOSED);
                warn_on_error(pty.flush_input());
                warn_on_error(pty.reset_after_close());
                transitions.push(Transition::Closed);
            }
        }
        // IGNORED: the device went away, which happens only when the pty
        // itself is dropped.
    }
}

/// A failed flush or reset costs the next program some stale bytes or a
/// device it cannot use; the count must go on regardless, so it is a
/// warning, not an error.
#[cfg(target_os = "linux")]
fn warn_on_error(result: io::Result<()>) {
    if let Err(e) = result {
        crate::ui::warn(e);
    }
}

/// No detection outside Linux: [`OpenWatch::new`] returns `None`, so no
/// value of this type exists and its methods cannot be called.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct OpenWatch {
    never: std::convert::Infallible,
}

#[cfg(not(target_os = "linux"))]
impl OpenWatch {
    /// `None`: this platform cannot tell; use [`Presence::unknown`].
    pub(crate) fn new(pty: &Pty) -> io::Result<Option<Self>> {
        let _ = pty;
        Ok(None)
    }

    pub(crate) fn presence(&self) -> Presence {
        match self.never {}
    }

    pub(crate) fn opens(&self) -> u32 {
        match self.never {}
    }

    pub(crate) fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        match self.never {}
    }

    pub(crate) fn wait(
        &mut self,
        pty: &Pty,
        timeout: Option<Duration>,
    ) -> io::Result<Vec<Transition>> {
        let _ = (pty, timeout);
        match self.never {}
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::Write;
    use std::time::Instant;

    use super::*;
    use crate::pty::testing::{Program, SHORT, read_master, settings};

    fn watched() -> (Pty, OpenWatch) {
        let pty = Pty::open(&settings(9600)).unwrap();
        let watch = OpenWatch::new(&pty).unwrap().unwrap();
        (pty, watch)
    }

    /// Waits until the watch reports transitions, for at most 5 s.
    fn next(watch: &mut OpenWatch, pty: &Pty) -> Vec<Transition> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no transition within 5 s");
            let transitions = watch.wait(pty, Some(left)).unwrap();
            if !transitions.is_empty() {
                return transitions;
            }
        }
    }

    /// Handles what is queued (open and close events are queued before the
    /// syscall returns), with no transitions expected.
    fn quiet(watch: &mut OpenWatch, pty: &Pty) {
        assert_eq!(watch.wait(pty, Some(SHORT)).unwrap(), []);
    }

    #[test]
    fn counts_opens_by_others_but_not_the_held_slave() {
        let (pty, mut watch) = watched();
        let presence = watch.presence();
        assert_eq!(presence.get(), Some(false));
        assert!(!presence.is_open());
        quiet(&mut watch, &pty);
        // Nor does what the bridge does through the held slave.
        pty.set_termios(&pty.termios().unwrap()).unwrap();
        pty.flush_input().unwrap();
        pty.reset_after_close().unwrap();
        quiet(&mut watch, &pty);

        let first = Program::open(&pty);
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        assert_eq!(presence.get(), Some(true));
        let second = Program::open(&pty);
        quiet(&mut watch, &pty);
        assert_eq!(watch.opens(), 2);

        drop(first);
        quiet(&mut watch, &pty);
        assert_eq!((watch.opens(), presence.get()), (1, Some(true)));
        drop(second);
        assert_eq!(next(&mut watch, &pty), [Transition::Closed]);
        assert_eq!((watch.opens(), presence.get()), (0, Some(false)));
    }

    #[test]
    fn a_read_only_open_counts_too() {
        let (pty, mut watch) = watched();
        let reader = Program::open_read_only(&pty);
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        drop(reader);
        assert_eq!(next(&mut watch, &pty), [Transition::Closed]);
    }

    #[test]
    fn the_first_open_flushes_stale_input() {
        let (pty, mut watch) = watched();
        (&pty).write_all(b"stale").unwrap();
        let program = Program::open(&pty);
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        assert_eq!(program.read_for(SHORT), b"");
        (&pty).write_all(b"fresh").unwrap();
        assert_eq!(program.read_exactly(5), b"fresh");

        // Left unread at the last close: flushed before the next program.
        (&pty).write_all(b"unread").unwrap();
        drop(program);
        assert_eq!(next(&mut watch, &pty), [Transition::Closed]);
        let next_program = Program::open(&pty);
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        assert_eq!(next_program.read_for(SHORT), b"");
    }

    #[test]
    fn the_last_close_ends_exclusive_mode() {
        // serialport-rs sets TIOCEXCL on every open by default and clears it
        // only when it closes the port itself. A real port
        // forgets it on its last close, which never comes while the bridge
        // holds the slave: after such a program was killed, every open by a
        // normal user failed with EBUSY until the bridge ended.
        let (pty, mut watch) = watched();
        let killed = Program::open(&pty);
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        killed.set_exclusive();
        match Program::try_open(&pty) {
            Err(e) => assert_eq!(e, rustix::io::Errno::BUSY),
            // CAP_SYS_ADMIN opens an exclusive tty anyway: nothing to test.
            Ok(_) => return,
        }
        drop(killed);
        assert_eq!(next(&mut watch, &pty), [Transition::Closed]);
        let program = Program::try_open(&pty).expect("still in exclusive mode");
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        drop(program);
    }

    #[test]
    fn the_last_close_restarts_suspended_output() {
        // tcflow(TCOOFF) suspends the device's output until a TCOON, which a
        // program killed in between never sends. A real port forgets it on
        // its last close; here every write of the next program failed.
        let (pty, mut watch) = watched();
        let killed = Program::open(&pty);
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        killed.suspend_output();
        drop(killed);
        assert_eq!(next(&mut watch, &pty), [Transition::Closed]);
        let program = Program::open(&pty);
        assert_eq!(next(&mut watch, &pty), [Transition::Opened]);
        program.write(b"out");
        assert_eq!(read_master(&pty, 3), b"out");
    }

    #[test]
    fn a_quick_open_and_close_are_both_seen() {
        let (pty, mut watch) = watched();
        drop(Program::open(&pty));
        let mut seen = next(&mut watch, &pty);
        if seen == [Transition::Opened] {
            seen.extend(next(&mut watch, &pty));
        }
        assert_eq!(seen, [Transition::Opened, Transition::Closed]);
        assert_eq!(watch.presence().get(), Some(false));
    }

    #[test]
    fn wait_times_out() {
        let (pty, mut watch) = watched();
        let start = Instant::now();
        assert_eq!(watch.wait(&pty, Some(SHORT)).unwrap(), []);
        assert!(start.elapsed() >= SHORT);
        assert_eq!(watch.wait(&pty, Some(Duration::ZERO)).unwrap(), []);
    }
}
