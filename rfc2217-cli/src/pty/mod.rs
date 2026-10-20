//! `rfc2217 pty`: a remote serial port as a local pseudo-terminal.
//!
//! The session engine (`crate::session`) runs between the bridge and the
//! pty's master; the OS side has one module each:
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

use std::io::{self, Read, Write};
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rfc2217::{ComPortState, Event, PortSettings};

use crate::Exit;
use crate::cli::PtyArgs;
use crate::session::{self, Ctl, InputEnd, Line, Local, LocalIo, Setup};
use crate::shutdown;
use crate::target::Target;
use crate::ui;

use self::link::Link;
use self::presence::{OpenWatch, Presence, Transition};
use self::settings::{SettingsWatch, Update};
use self::sys::Pty;

mod link;
mod presence;
mod settings;
mod sys;
#[cfg(test)]
mod testing;

/// How long the pty's writer may take, once the session has ended, to write
/// what is still queued: a program that stopped reading must not keep the
/// process alive.
const DRAIN: Duration = Duration::from_millis(500);

pub(crate) fn run(args: PtyArgs) -> Exit {
    let requested = args.line.settings();
    let pty = match Pty::open(&requested) {
        Ok(pty) => Arc::new(pty),
        Err(e) => {
            ui::error(format_args!("cannot create a pty: {e}"));
            return Exit::Io;
        }
    };
    // Both watches start before anyone can know the path: what the pty says
    // now is the baseline for the program's changes, and no program can have
    // opened it yet.
    let watch = match SettingsWatch::new(&pty) {
        Ok(watch) => watch,
        Err(e) => {
            ui::error(e);
            return Exit::Io;
        }
    };
    let (presence, opens) = watch_opens(&pty);
    let link = match &args.link {
        Some(path) => match Link::create(path, pty.path()) {
            Ok(link) => Some(remove_on_exit(link)),
            Err(e) => {
                ui::error(e);
                return Exit::Io;
            }
        },
        None => None,
    };
    let stream = match args.target.connect() {
        Ok(stream) => stream,
        Err(e) => {
            ui::error(format_args!("cannot connect to {}: {e}", args.target));
            return Exit::Connect;
        }
    };
    ui::verbose(format_args!("connected to {}", args.target));
    let setup = Setup {
        target: &args.target,
        line: &args.line,
        args: &args.session,
    };
    let io = LocalIo {
        reader: Box::new(Master(Arc::clone(&pty))),
        writer: Box::new(Output {
            pty: Arc::clone(&pty),
            presence,
            opens,
        }),
        input: "the pty",
        output: "the pty",
        // A program writing faster than the bridge takes it is pushed back
        // through the pty, as a UART's transmit buffer would.
        throttle: true,
        drain: Some(DRAIN),
    };
    let mut bridge = Bridge {
        pty,
        watch,
        requested,
        target: args.target.clone(),
        link,
        next_poll: None,
        told_refused: false,
    };
    session::run(setup, stream, io, &mut bridge)
}

/// Starts counting the programs that open the pty, on a thread of its own.
/// Returns what the pty's writer checks before each write, and the count,
/// which the writer brings up to date before it drops anything.
fn watch_opens(pty: &Arc<Pty>) -> (Presence, Option<Arc<OpenWatch>>) {
    let watch = match OpenWatch::new(pty) {
        Ok(Some(watch)) => Arc::new(watch),
        // macOS: nothing tells.
        Ok(None) => return (Presence::unknown(), None),
        Err(e) => {
            ui::warn(format_args!(
                "{e}: the port's data is kept for the next program while none has {} open",
                pty.path().display()
            ));
            return (Presence::unknown(), None);
        }
    };
    let spawned = thread::Builder::new().name("pty opens".into()).spawn({
        let (pty, watch) = (Arc::clone(pty), Arc::clone(&watch));
        move || {
            loop {
                match watch.wait(&pty, None) {
                    Ok(transitions) => {
                        report(&transitions, &pty);
                        // The device counts as open from now on, and the
                        // writer no longer asks.
                        if transitions.contains(&Transition::Lost) {
                            return;
                        }
                    }
                    Err(e) => return lost(&e),
                }
            }
        }
    });
    match spawned {
        Ok(_) => (watch.presence(), Some(watch)),
        // Nothing would count the opens: the presence would stay closed.
        Err(e) => {
            ui::warn(format_args!("cannot watch {}: {e}", pty.path().display()));
            (Presence::unknown(), None)
        }
    }
}

fn lost(e: &io::Error) {
    ui::warn(format_args!(
        "{e}: the port's data is kept for the next program from now on"
    ));
}

fn report(transitions: &[Transition], pty: &Pty) {
    let path = pty.path().display();
    for transition in transitions {
        report_one(*transition, &path);
    }
}

fn report_one(transition: Transition, path: &impl std::fmt::Display) {
    match transition {
        Transition::Opened => ui::verbose(format_args!("a program opened {path}")),
        Transition::Closed => ui::verbose(format_args!(
            "the last program closed {path}: the port's data is dropped until the next one opens it"
        )),
        Transition::Lost => ui::warn(format_args!(
            "lost count of the programs that have {path} open: the port's data is kept for the \
             next program from now on"
        )),
    }
}

/// Hands the link to the cleanup registry, which removes it however the
/// process ends ([`Link`]'s `Drop` removes it, and warns if another link has
/// taken its place by then). Returns its path, for messages.
fn remove_on_exit(link: Link) -> PathBuf {
    if let Some(old) = link.replaced() {
        ui::notice(format_args!(
            "replaced {}, a stale link to {}",
            link.path().display(),
            old.display()
        ));
    }
    let path = link.path().to_owned();
    shutdown::register(move || drop(link));
    path
}

/// The master, read by the engine's local reader: what the program writes.
#[derive(Debug)]
struct Master(Arc<Pty>);

impl Read for Master {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

/// The master, written by the engine's local writer: the port's data for
/// the program. While no program has the device open the data is dropped:
/// by the time one opens it, it would be stale (see [`presence`]).
#[derive(Debug)]
struct Output {
    pty: Arc<Pty>,
    presence: Presence,
    opens: Option<Arc<OpenWatch>>,
}

impl Output {
    /// Whether a program has the device open after all: one that opened it
    /// just now may not be counted yet, and its open is queued before
    /// anything it wrote, so before the answer in `buf` (see [`OpenWatch`]).
    fn opened_just_now(&self) -> bool {
        if let Some(opens) = &self.opens {
            match opens.update(&self.pty) {
                Ok(transitions) => report(&transitions, &self.pty),
                Err(e) => lost(&e),
            }
        }
        self.presence.is_open()
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Checked for each write: when the program closes the device in the
        // middle of a chunk, the rest goes too.
        if self.presence.is_open() || self.opened_just_now() {
            (&*self.pty).write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The pty's side of the session: the program's settings changes go to the
/// bridge, and with `--baud keep` the bridge's rate goes into the pty.
#[derive(Debug)]
struct Bridge {
    pty: Arc<Pty>,
    watch: SettingsWatch,
    /// What the bridge was last asked for: the command line's settings, with
    /// the program's changes applied.
    requested: PortSettings,
    target: Target,
    link: Option<PathBuf>,
    /// When to read the pty's settings next, from the moment the negotiation
    /// has settled: until then the command line's settings are on their way.
    next_poll: Option<Instant>,
    /// The program's changes cannot reach a bridge that refused RFC 2217:
    /// said once.
    told_refused: bool,
}

impl Local for Bridge {
    /// The held slave keeps the master open: its read returns 0 only if the
    /// pty itself is gone.
    fn input_ended(&mut self) -> InputEnd {
        ui::error(format_args!("{} was closed", self.pty.path().display()));
        InputEnd::Exit(Exit::Io)
    }

    fn event(&mut self, event: &Event, ctl: &mut Ctl) {
        let _ = ctl;
        // With `keep`, the pty shows the rate the bridge runs at. A rate the
        // program (or the command line) chose stays as it is: a different
        // answer is a mismatch, which the engine reports.
        if let Event::BaudRate(rate) = *event {
            if self.requested.baud_rate == 0 {
                self.show_rate(rate);
            }
        }
    }

    fn settled(&mut self, ctl: &mut Ctl) {
        let client = ctl.client();
        if client.is_ready() {
            ui::notice(format_args!(
                "port settings: {}",
                Line::from(client.reported_settings())
            ));
        }
        let path = self.pty.path().display();
        // Only now, so that a program started on the path the moment it
        // appears sees the bridge's rate. stdout carries nothing else, and
        // `-q` does not hide it: scripts read the path from there.
        let mut stdout = io::stdout().lock();
        let _ = writeln!(stdout, "{path}").and_then(|()| stdout.flush());
        match &self.link {
            Some(link) => ui::notice(format_args!(
                "{path} ({}) is connected to {}",
                link.display(),
                self.target
            )),
            None => ui::notice(format_args!("{path} is connected to {}", self.target)),
        }
        self.next_poll = Some(Instant::now() + SettingsWatch::INTERVAL);
    }

    fn next_tick(&self) -> Option<Instant> {
        self.next_poll
    }

    fn tick(&mut self, now: Instant, ctl: &mut Ctl) -> Option<Exit> {
        self.next_poll = Some(now + SettingsWatch::INTERVAL);
        match self.watch.poll(&self.pty) {
            Ok(Some(update)) => self.changed(&update, ctl),
            Ok(None) => {}
            // The held slave is the pty: without it there is nothing left to
            // bridge to.
            Err(e) => {
                ui::error(e);
                return Some(Exit::Io);
            }
        }
        None
    }

    fn remote_closed(&self) -> Exit {
        Exit::Io
    }
}

impl Bridge {
    /// The program changed the pty's settings: what can be forwarded goes to
    /// the bridge.
    fn changed(&mut self, update: &Update, ctl: &mut Ctl) {
        let now = update.now;
        if let Some(input) = now.split_input_speed {
            ui::warn(format_args!(
                "the program set {input} baud for input and {} for output: RFC 2217 has one \
                 rate, the output's",
                now.baud_rate
            ));
        }
        if now.baud_rate == 0 {
            ui::warn(
                "the program set the speed to 0 (hang up), which a pty cannot pass on: the \
                 bridge keeps its rate",
            );
        }
        if !update.apply(&mut self.requested) {
            return;
        }
        if ctl.client().com_port_state() == ComPortState::Refused {
            if !mem::replace(&mut self.told_refused, true) {
                ui::warn(
                    "the program changed the line settings, but the bridge does not do RFC 2217: \
                     they stay as they are",
                );
            }
            return;
        }
        ui::verbose(format_args!(
            "the program asked for {}",
            Line::from(self.requested)
        ));
        ctl.set_port_settings(self.requested);
    }

    fn show_rate(&mut self, rate: u32) {
        match self.watch.set_speed(&self.pty, rate) {
            Ok(true) => ui::verbose(format_args!(
                "{} shows the bridge's {rate} baud",
                self.pty.path().display()
            )),
            // The program set a speed of its own since the last poll: the
            // next poll forwards it.
            Ok(false) => {}
            Err(e) => ui::warn(e),
        }
    }
}
