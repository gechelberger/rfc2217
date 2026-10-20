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
#[cfg(target_os = "linux")]
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

use super::sys::Pty;

const UNKNOWN: u8 = 0;
const CLOSED: u8 = 1;
// Only Linux's `OpenWatch` sees a program open the device.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn get(&self) -> Option<bool> {
        match self.0.load(Ordering::Acquire) {
            CLOSED => Some(false),
            OPEN => Some(true),
            _ => None,
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn set(&self, state: u8) {
        self.0.store(state, Ordering::Release);
    }
}

/// A change in whether any program has the device open, from
/// [`OpenWatch::wait`].
// Only Linux's `OpenWatch` makes them.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
/// inotify merges an event into the one before it when both are identical
/// and the first is still unread, so two opens (or closes) back to back
/// would count as one: always for one process that opens the device twice,
/// or exits with two fds on it. The count would then drift: down to zero
/// while a program still has the device open (its data dropped, its
/// exclusive mode undone), or stuck above zero for good. So the directory
/// (`/dev/pts`) is watched as well: each open and close of the device also
/// queues an event for the directory, in between, and no two of the
/// device's events are ever next to each other. The directory's events are
/// read and dropped; other ptys opening wake the thread for nothing, and a
/// flood of them can overflow the queue, which ends the count
/// ([`Transition::Lost`]) without harm.
///
/// Events arrive after the fact, so the flushes are not exact either. A
/// program that reads at once after opening can read bytes before the flush
/// on its open runs; there are few, since the flush on the last close
/// already emptied the queue and the bridge drops data from then on. Only a
/// write the bridge started just before seeing the close can be among them.
///
/// A thread of its own waits for the events ([`wait`](Self::wait)), and
/// the pty's writer handles those already queued ([`update`](Self::update))
/// before it drops data for a device counted as closed: the kernel queues
/// a program's open before its `open` call returns, so before anything the
/// program writes, and before the bridge's answer to that. Without this, a
/// program that opens the device and sends a command at once could lose the
/// answer whenever the watching thread is slow to run.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct OpenWatch {
    inotify: OwnedFd,
    /// The device's watch: the events of every other are dropped.
    device: i32,
    /// Taken while reading and handling events: the threads that do take
    /// turns, so the events are counted in order.
    count: Mutex<Count>,
    presence: Presence,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct Count {
    opens: u32,
    lost: bool,
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
        let events = WatchFlags::OPEN | WatchFlags::CLOSE;
        let device =
            inotify::add_watch(&inotify, pty.path(), events).map_err(super::sys::failed(&what))?;
        // Only so that the device's events are never merged (see above).
        if let Some(dir) = pty.path().parent() {
            inotify::add_watch(&inotify, dir, events).map_err(super::sys::failed(&what))?;
        }
        Ok(Some(Self {
            inotify,
            device,
            count: Mutex::default(),
            presence: Presence(Arc::new(AtomicU8::new(CLOSED))),
        }))
    }

    pub(crate) fn presence(&self) -> Presence {
        self.presence.clone()
    }

    /// Opens of the device by programs that have not closed it yet, as far
    /// as the events tell (see the type's documentation).
    #[cfg(test)]
    pub(crate) fn opens(&self) -> u32 {
        self.lock().opens
    }

    /// A panic anywhere ends the process, so a poisoned lock only means
    /// that the thread holding it is gone.
    fn lock(&self) -> MutexGuard<'_, Count> {
        self.count.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// is a warning (`ui::warn`), not an error. An error ends the count: the
    /// presence is unknown (counts as open) from then on.
    pub(crate) fn wait(&self, pty: &Pty, timeout: Option<Duration>) -> io::Result<Vec<Transition>> {
        use rustix::event::{PollFd, PollFlags, Timespec};
        use rustix::io::Errno;

        // A timeout too large for a timespec is as good as none.
        let timeout = timeout.and_then(|t| Timespec::try_from(t).ok());
        let mut fds = [PollFd::new(&self.inotify, PollFlags::IN)];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(0) | Err(Errno::INTR) => Ok(Vec::new()),
            Ok(_) => self.update(pty),
            Err(e) => {
                let mut count = self.lock();
                Err(self.give_up(&mut count, super::sys::failed("waiting for programs")(e)))
            }
        }
    }

    /// Handles the opens and closes queued now, without waiting, as
    /// [`wait`](Self::wait) does.
    pub(crate) fn update(&self, pty: &Pty) -> io::Result<Vec<Transition>> {
        let mut count = self.lock();
        // Read even once the count is lost (`handle` ignores them then): a
        // queue left full would keep `wait` from ever waiting.
        let events = self
            .read_events()
            .map_err(|e| self.give_up(&mut count, e))?;
        let mut transitions = Vec::new();
        for event in events {
            self.handle(&mut count, event, pty, &mut transitions);
        }
        Ok(transitions)
    }

    /// After an error the count can no longer be trusted: as after
    /// [`Transition::Lost`], the device counts as open from then on, so that
    /// a count stuck at "closed" cannot drop every byte from the bridge.
    fn give_up(&self, count: &mut Count, e: io::Error) -> io::Error {
        count.lost = true;
        self.presence.set(UNKNOWN);
        e
    }

    /// The device's events queued now, and an overflow, without blocking.
    fn read_events(&self) -> io::Result<Vec<rustix::fs::inotify::ReadFlags>> {
        use rustix::fs::inotify::{ReadFlags, Reader};
        use rustix::io::Errno;
        use std::mem::MaybeUninit;

        // The directory's events carry a name of up to 255 bytes.
        let mut buf = [MaybeUninit::uninit(); 4096];
        let mut reader = Reader::new(&self.inotify, &mut buf);
        let mut events = Vec::new();
        loop {
            match reader.next() {
                Ok(event)
                    if event.wd() == self.device
                        || event.events().contains(ReadFlags::QUEUE_OVERFLOW) =>
                {
                    events.push(event.events());
                }
                Ok(_) => {}
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
        &self,
        count: &mut Count,
        event: rustix::fs::inotify::ReadFlags,
        pty: &Pty,
        transitions: &mut Vec<Transition>,
    ) {
        use rustix::fs::inotify::ReadFlags;

        if count.lost {
            return;
        }
        if event.contains(ReadFlags::QUEUE_OVERFLOW) {
            count.lost = true;
            self.presence.set(UNKNOWN);
            transitions.push(Transition::Lost);
        } else if event.contains(ReadFlags::OPEN) {
            count.opens += 1;
            if count.opens == 1 {
                warn_on_error(pty.flush_input());
                self.presence.set(OPEN);
                transitions.push(Transition::Opened);
            }
        } else if event.intersects(ReadFlags::CLOSE_WRITE | ReadFlags::CLOSE_NOWRITE) {
            // Saturating: a merged open (see the type's documentation) can
            // leave one close more than opens.
            let was_open = count.opens > 0;
            count.opens = count.opens.saturating_sub(1);
            if was_open && count.opens == 0 {
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

    pub(crate) fn wait(&self, pty: &Pty, timeout: Option<Duration>) -> io::Result<Vec<Transition>> {
        let _ = (pty, timeout);
        match self.never {}
    }

    pub(crate) fn update(&self, pty: &Pty) -> io::Result<Vec<Transition>> {
        let _ = pty;
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
    fn next(watch: &OpenWatch, pty: &Pty) -> Vec<Transition> {
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
    fn quiet(watch: &OpenWatch, pty: &Pty) {
        assert_eq!(watch.wait(pty, Some(SHORT)).unwrap(), []);
    }

    #[test]
    fn counts_opens_by_others_but_not_the_held_slave() {
        let (pty, watch) = watched();
        let presence = watch.presence();
        assert_eq!(presence.get(), Some(false));
        assert!(!presence.is_open());
        quiet(&watch, &pty);
        // Nor does what the bridge does through the held slave.
        pty.set_termios(&pty.termios().unwrap()).unwrap();
        pty.flush_input().unwrap();
        pty.reset_after_close().unwrap();
        quiet(&watch, &pty);

        let first = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        assert_eq!(presence.get(), Some(true));
        let second = Program::open(&pty);
        quiet(&watch, &pty);
        assert_eq!(watch.opens(), 2);

        drop(first);
        quiet(&watch, &pty);
        assert_eq!((watch.opens(), presence.get()), (1, Some(true)));
        drop(second);
        assert_eq!(next(&watch, &pty), [Transition::Closed]);
        assert_eq!((watch.opens(), presence.get()), (0, Some(false)));
    }

    #[test]
    fn a_read_only_open_counts_too() {
        let (pty, watch) = watched();
        let reader = Program::open_read_only(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        drop(reader);
        assert_eq!(next(&watch, &pty), [Transition::Closed]);
    }

    #[test]
    fn the_first_open_flushes_stale_input() {
        let (pty, watch) = watched();
        (&pty).write_all(b"stale").unwrap();
        let program = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        assert_eq!(program.read_for(SHORT), b"");
        (&pty).write_all(b"fresh").unwrap();
        assert_eq!(program.read_exactly(5), b"fresh");

        // Left unread at the last close: flushed before the next program.
        (&pty).write_all(b"unread").unwrap();
        drop(program);
        assert_eq!(next(&watch, &pty), [Transition::Closed]);
        let next_program = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        assert_eq!(next_program.read_for(SHORT), b"");
    }

    #[test]
    fn the_last_close_ends_exclusive_mode() {
        // serialport-rs sets TIOCEXCL on every open by default and clears it
        // only when it closes the port itself. A real port
        // forgets it on its last close, which never comes while the bridge
        // holds the slave: after such a program was killed, every open by a
        // normal user failed with EBUSY until the bridge ended.
        let (pty, watch) = watched();
        let killed = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        killed.set_exclusive();
        match Program::try_open(&pty) {
            Err(e) => assert_eq!(e, rustix::io::Errno::BUSY),
            // CAP_SYS_ADMIN opens an exclusive tty anyway: nothing to test.
            Ok(_) => return,
        }
        drop(killed);
        assert_eq!(next(&watch, &pty), [Transition::Closed]);
        let program = Program::try_open(&pty).expect("still in exclusive mode");
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        drop(program);
    }

    #[test]
    fn the_last_close_restarts_suspended_output() {
        // tcflow(TCOOFF) suspends the device's output until a TCOON, which a
        // program killed in between never sends. A real port forgets it on
        // its last close; here every write of the next program failed.
        let (pty, watch) = watched();
        let killed = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        killed.suspend_output();
        drop(killed);
        assert_eq!(next(&watch, &pty), [Transition::Closed]);
        let program = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        program.write(b"out");
        assert_eq!(read_master(&pty, 3), b"out");
    }

    #[test]
    fn the_last_close_restarts_output_stopped_by_an_xoff() {
        // With IXON the port's XOFF stops the device's output, and the XON
        // that would restart it comes for a program that has gone.
        use rustix::termios::InputModes;

        let (pty, watch) = watched();
        let killed = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        killed.change(|t| t.input_modes |= InputModes::IXON | InputModes::IXOFF);
        (&pty).write_all(b"\x13").unwrap();
        drop(killed);
        assert_eq!(next(&watch, &pty), [Transition::Closed]);
        let program = Program::open(&pty);
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        program.write(b"out");
        assert_eq!(read_master(&pty, 3), b"out");
        // The program's settings are as it left them.
        assert!(program.termios().input_modes.contains(InputModes::IXON));
    }

    #[test]
    fn a_quick_open_and_close_are_both_seen() {
        let (pty, watch) = watched();
        drop(Program::open(&pty));
        let mut seen = next(&watch, &pty);
        if seen == [Transition::Opened] {
            seen.extend(next(&watch, &pty));
        }
        assert_eq!(seen, [Transition::Opened, Transition::Closed]);
        assert_eq!(watch.presence().get(), Some(false));
    }

    #[test]
    fn an_open_is_queued_before_it_returns() {
        // What the pty's writer relies on before it drops data: `update`,
        // which does not wait, already sees the program.
        let (pty, watch) = watched();
        let program = Program::open(&pty);
        assert_eq!(watch.update(&pty).unwrap(), [Transition::Opened]);
        assert!(watch.presence().is_open());
        drop(program);
        assert_eq!(watch.update(&pty).unwrap(), [Transition::Closed]);
        assert!(!watch.presence().is_open());
    }

    #[test]
    fn two_opens_and_two_closes_back_to_back_both_count() {
        // Without the directory's watch the kernel merges each pair.
        let (pty, watch) = watched();
        let (first, second) = (Program::open(&pty), Program::open(&pty));
        assert_eq!(next(&watch, &pty), [Transition::Opened]);
        assert_eq!(watch.opens(), 2);
        drop(second);
        quiet(&watch, &pty);
        assert!(watch.presence().is_open());
        let third = Program::open(&pty);
        drop((first, third));
        assert_eq!(next(&watch, &pty), [Transition::Closed]);
        assert_eq!(watch.opens(), 0);
    }

    #[test]
    fn an_overflow_ends_the_count_and_the_queue_still_drains() {
        use rustix::fs::inotify::ReadFlags;

        // Filling the kernel's queue for real would overflow the watches of
        // the tests running beside this one: they see its ptys' directory
        // events.
        let (pty, watch) = watched();
        let mut transitions = Vec::new();
        watch.handle(
            &mut watch.lock(),
            ReadFlags::QUEUE_OVERFLOW,
            &pty,
            &mut transitions,
        );
        assert_eq!(transitions, [Transition::Lost]);
        assert_eq!(watch.presence().get(), None);
        assert!(watch.presence().is_open());
        // Later events are still read, or `wait` would never wait again.
        drop(Program::open(&pty));
        assert_eq!(watch.update(&pty).unwrap(), []);
        assert_eq!(watch.read_events().unwrap(), []);
    }

    #[test]
    fn wait_times_out() {
        // Without the time it took: other tests' ptys wake it early.
        let (pty, watch) = watched();
        assert_eq!(watch.wait(&pty, Some(SHORT)).unwrap(), []);
        assert_eq!(watch.wait(&pty, Some(Duration::ZERO)).unwrap(), []);
    }
}
