//! The session engine, shared by `connect` and `pty`: one owner thread holds
//! the [`Client`] and makes every decision; the threads around it only move
//! bytes, and tell the owner what happened through one channel.
//!
//! ```text
//!   socket reader ─┐                ┌─▶ local writer  (stdout; the pty master)
//!   local reader  ─┼─▶ owner thread ┤
//!   signal thread ─┘    (Client)    └─▶ socket writer
//! ```
//!
//! The owner never blocks on a write. The library's driving contract (its
//! README, "Driving model") is that a loop which writes everything before it
//! reads again deadlocks against a peer that does the same, and a quit key
//! or a signal must not wait on the peer either (review finding cn-3). So
//! the owner hands each chunk to a writer thread, and memory is bounded by
//! pausing the reader that feeds a full direction, never the owner (finding
//! vc-1): the socket reader waits while more than [`LIMIT`] bytes are on
//! their way to the local writer, and the local reader waits while more than
//! [`LIMIT`] bytes are on their way to the socket (input not yet handed to
//! the client included). The readers wait on a condvar and the owner on its
//! channel, with its next deadline as the timeout: nothing polls.
//!
//! The owner also applies the readiness policy. Remote output is shown at
//! once, also while negotiating; local input is held until the negotiation
//! has settled, and while the bridge has suspended the data flow (the
//! library's flow control is advisory: the CLI enforces it, finding cn-12).

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rfc2217::{
    Client, ComPortState, DataBits, Event, FlowControl, NotReady, Parity, PartialSettings,
    PortSettings, StopBits,
};

use crate::Exit;
use crate::cli::{LineArgs, SessionArgs};
use crate::shutdown;
use crate::target::Target;
use crate::ui;

/// Bytes one direction may hold before its reader pauses.
const LIMIT: usize = 256 * 1024;

/// What one read takes at most.
const CHUNK: usize = 16 * 1024;

/// Longer waits are forever: 136 years, short of overflowing an `Instant`
/// (which `--timeout 1e19` would).
const FOREVER: Duration = Duration::from_secs(1 << 32);

/// How long the socket writer may take, once the session has ended, to
/// write what is still queued (typed just before the quit key, say). A
/// bridge that stopped reading must not keep the process alive.
const NET_DRAIN: Duration = Duration::from_secs(1);

/// The same after a signal: short, well within the signal thread's grace
/// (see `shutdown`), but long enough for a last command such as the end of
/// a BREAK.
const SIGNAL_DRAIN: Duration = Duration::from_millis(250);

/// What may wait for the socket before the session gives up. Throttled
/// input stays near [`LIMIT`], so only a bridge that sends requests and
/// never reads the replies gets here; without a cap, those replies would
/// fill the memory.
const NET_CAP: usize = 16 * LIMIT;

/// A deadline found this late passed while the process was not running
/// (stopped, say): what the bridge sent meanwhile has not been read yet.
const LATE: Duration = Duration::from_millis(100);

/// How long such a deadline is put off, once, for that.
const LATE_GRACE: Duration = Duration::from_millis(200);

/// `from + wait`, for any wait the command line accepts.
fn after(from: Instant, wait: Duration) -> Instant {
    from + wait.min(FOREVER)
}

/// What a session starts from: where it is connected (for messages), and
/// the command line's line and session settings.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Setup<'a> {
    pub(crate) target: &'a Target,
    pub(crate) line: &'a LineArgs,
    pub(crate) args: &'a SessionArgs,
}

/// The local end's byte streams, each served by its own thread with
/// blocking calls.
pub(crate) struct LocalIo {
    pub(crate) reader: Box<dyn Read + Send>,
    pub(crate) writer: Box<dyn Write + Send>,
    /// For messages: "reading {input}: ...".
    pub(crate) input: &'static str,
    /// For messages: "writing {output}: ...".
    pub(crate) output: &'static str,
    /// Pause the reader while too much input waits for the socket. Off for a
    /// terminal: its volume is bounded by the typist, and a paused reader
    /// would also hold back the escape key while the bridge stalls.
    pub(crate) throttle: bool,
    /// How long the writer may take to write what is queued when the
    /// session ends. `None` waits until it is written: stdout's reader
    /// decides how fast it goes, as with `cat`.
    pub(crate) drain: Option<Duration>,
}

/// How a session goes on once its local input has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputEnd {
    /// Keep copying the port's output until it has been quiet this long and
    /// everything was written to the socket, and the readiness policy has
    /// let the session go on; then end with [`Exit::Ok`].
    Linger(Duration),
    /// End now.
    // The local ends that use it (the terminal, the pty) are Unix-only.
    #[cfg_attr(not(unix), allow(dead_code))]
    Exit(Exit),
}

/// The local end of a session: `connect`'s terminal or pipe, and in the
/// next stage the pty master. The engine calls these on the owner thread,
/// so they must not block; the client's output is written after each call.
/// The defaults are those of a byte-transparent end.
///
/// How `pty` plugs in: its [`LocalIo`] reads and writes the pty master
/// (blocking calls in the engine's threads, `throttle` on, so that a program
/// writing faster than the bridge takes is pushed back through the pty, and
/// a short `drain`, since nothing may be reading the slave); `input` and
/// `output` keep their defaults (no escape key, no mapping); `next_tick` and
/// `tick` poll the pty's termios every 50 ms and call
/// [`Ctl::set_port_settings`] on a change; `settled` and `event` write the
/// bridge's baud rate back into the pty; and `remote_closed` returns
/// [`Exit::Io`].
pub(crate) trait Local {
    /// Bytes the local reader read. The default sends them as they are.
    fn input(&mut self, bytes: &[u8], ctl: &mut Ctl) -> Option<Exit> {
        ctl.send(bytes);
        None
    }

    /// The local reader reached the end of its input.
    fn input_ended(&mut self) -> InputEnd;

    /// The port's data, on its way to the local writer. The default passes
    /// it on unchanged.
    fn output(&mut self, payload: Vec<u8>) -> Vec<u8> {
        payload
    }

    /// An event from the bridge, after the engine logged it and noted what
    /// it means for the readiness policy and the data flow.
    fn event(&mut self, event: &Event, ctl: &mut Ctl) {
        let _ = (event, ctl);
    }

    /// The negotiation has settled: held input goes out from now on (unless
    /// the policy failed the session, which then ends without this call).
    fn settled(&mut self, ctl: &mut Ctl) {
        let _ = ctl;
    }

    /// When [`tick`](Self::tick) is due next, if at all. It must lie in the
    /// future once `tick` has run.
    fn next_tick(&self) -> Option<Instant> {
        None
    }

    /// Called once `next_tick` has passed (and possibly a little later).
    fn tick(&mut self, now: Instant, ctl: &mut Ctl) -> Option<Exit> {
        let _ = (now, ctl);
        None
    }

    /// How the session ends when the bridge closes the connection: `connect`
    /// is done ([`Exit::Ok`]), while `pty` lost its port ([`Exit::Io`]).
    fn remote_closed(&self) -> Exit;

    /// The session is ending, however it ends (a signal included): the last
    /// chance to send something, such as the end of a BREAK.
    fn ending(&mut self, ctl: &mut Ctl) {
        let _ = ctl;
    }
}

/// What the local end may do with the session: the owner's state, lent to
/// the [`Local`] calls.
#[derive(Debug)]
pub(crate) struct Ctl {
    client: Client,
    /// Data for the port not yet handed to the client: held until the
    /// negotiation settles, and while the bridge has suspended the flow.
    pending: Vec<u8>,
    net: Queue,
    out: Queue,
    settled: bool,
    /// Report the outcome of a settings change made during the session by
    /// this deadline at the latest.
    recheck: Option<Instant>,
    /// The negotiation's deadline, for a recheck.
    #[cfg_attr(not(unix), allow(dead_code))]
    timeout: Duration,
}

// The local ends that change settings, echo or send commands (the
// terminal, the pty) are Unix-only.
#[cfg_attr(not(unix), allow(dead_code))]
impl Ctl {
    /// Data for the port, sent in order once the negotiation has settled
    /// and while the bridge has not suspended the flow.
    pub(crate) fn send(&mut self, data: &[u8]) {
        if !data.is_empty() {
            self.pending.extend_from_slice(data);
            self.net.gate.add(data.len());
            // To the client now, not after the `Local` call: a command
            // that follows in the same call (DTR, BREAK) goes out after
            // this data, as it was typed.
            self.flush();
        }
    }

    /// Bytes for the local writer (a local echo), in order with the port's
    /// data.
    pub(crate) fn show(&mut self, bytes: Vec<u8>) {
        self.out.push(bytes);
    }

    /// The client: its state, and the commands (DTR, RTS, BREAK, purge),
    /// which the alloc layer holds while negotiating. Settings changes go
    /// through [`set_port_settings`](Self::set_port_settings) instead.
    pub(crate) fn client(&mut self) -> &mut Client {
        &mut self.client
    }

    /// Whether the readiness policy has let the session go on: until then,
    /// data for the port is held.
    pub(crate) fn settled(&self) -> bool {
        self.settled
    }

    /// Hands what can go to the socket writer and waits up to `timeout` for
    /// it to be written: before the process stops (all its threads stop
    /// with it). Input not handled yet counts as unwritten.
    pub(crate) fn wait_sent(&mut self, timeout: Duration) {
        self.flush();
        self.net.gate.wait_empty(after(Instant::now(), timeout));
    }

    /// Asks the bridge for new line settings; what it answers is reported
    /// as at the start, but only ever as a warning.
    pub(crate) fn set_port_settings(&mut self, settings: PortSettings) {
        self.client.set_port_settings(settings);
        self.recheck = Some(after(Instant::now(), self.timeout));
    }

    /// Hands held input to the client if the policy and the bridge allow
    /// it, then the client's output to the socket writer.
    fn flush(&mut self) {
        let release = self.settled && !self.client.flow_suspended() && !self.pending.is_empty();
        let released = if release {
            self.client.write_data(&self.pending);
            let n = self.pending.len();
            self.pending.clear();
            n
        } else {
            0
        };
        self.net.push(self.client.take_output());
        // Only now, so that the gate does not dip below the limit while the
        // bytes move from `pending` to the queue.
        self.net.gate.sub(released);
    }

    /// Reports the bridge's answer to a settings change made during the
    /// session, once it is complete or the deadline has passed.
    fn recheck(&mut self, now: Instant) {
        let Some(deadline) = self.recheck else {
            return;
        };
        let late = now >= deadline;
        match self.client.readiness() {
            // Asked for during the session (a key, a program on the pty):
            // the answer is news.
            Ok(()) => ui::notice(format_args!(
                "the bridge confirmed {}",
                Line::from(self.client.reported_settings())
            )),
            Err(NotReady::Mismatched(settings))
                if late || self.client.pending_settings().is_empty() =>
            {
                ui::warn(mismatch(&self.client, settings));
            }
            Err(e @ NotReady::Refused) => ui::warn(format_args!("cannot change the settings: {e}")),
            Err(e) if late => ui::warn(e),
            Err(_) => return,
        }
        self.recheck = None;
    }
}

/// "the bridge reported different values for: data bits, parity (requested
/// ...; reported ...)".
fn mismatch(client: &Client, settings: rfc2217::SettingSet) -> String {
    format!(
        "{} (requested {}; reported {})",
        NotReady::Mismatched(settings),
        Line::from(client.requested_settings()),
        Line::from(client.reported_settings()),
    )
}

/// Line settings in messages: `115200 baud, 7E1, no flow control`, with `?`
/// for what the bridge has not reported, and `kept baud rate` for the
/// RFC 2217 query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Line(PartialSettings);

impl From<PortSettings> for Line {
    fn from(s: PortSettings) -> Self {
        Self(PartialSettings {
            baud_rate: Some(s.baud_rate),
            data_bits: Some(s.data_bits),
            parity: Some(s.parity),
            stop_bits: Some(s.stop_bits),
            flow_control: Some(s.flow_control),
        })
    }
}

impl From<PartialSettings> for Line {
    fn from(s: PartialSettings) -> Self {
        Self(s)
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.0;
        match s.baud_rate {
            Some(0) => f.write_str("kept baud rate")?,
            Some(rate) => write!(f, "{rate} baud")?,
            None => f.write_str("? baud")?,
        }
        let data_bits = s.data_bits.map_or('?', |bits| match bits {
            DataBits::Five => '5',
            DataBits::Six => '6',
            DataBits::Seven => '7',
            DataBits::Eight => '8',
        });
        let parity = s.parity.map_or('?', |parity| match parity {
            Parity::None => 'N',
            Parity::Odd => 'O',
            Parity::Even => 'E',
            Parity::Mark => 'M',
            Parity::Space => 'S',
        });
        let stop_bits = s.stop_bits.map_or("?", |bits| match bits {
            StopBits::One => "1",
            StopBits::OnePointFive => "1.5",
            StopBits::Two => "2",
        });
        let flow = match s.flow_control {
            None => "flow control ?",
            Some(FlowControl::None) => "no flow control",
            Some(FlowControl::XonXoff) => "XON/XOFF flow control",
            Some(FlowControl::RtsCts) => "RTS/CTS flow control",
            Some(FlowControl::Dcd) => "DCD flow control",
            Some(FlowControl::Dsr) => "DSR flow control",
        };
        write!(f, ", {data_bits}{parity}{stop_bits}, {flow}")
    }
}

/// Bytes on their way through the CLI in one direction: read by that
/// direction's reader and not yet written by its writer. The reader waits
/// while there are [`LIMIT`] or more; the owner only adds and subtracts.
#[derive(Debug, Default)]
struct Gate {
    bytes: Mutex<usize>,
    below_limit: Condvar,
}

impl Gate {
    /// A panic anywhere ends the process, so a poisoned lock only means
    /// that the thread holding it is gone; the count is still right.
    fn lock(&self) -> MutexGuard<'_, usize> {
        self.bytes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn add(&self, n: usize) {
        *self.lock() += n;
    }

    /// Returns how many bytes are left.
    fn sub(&self, n: usize) -> usize {
        let mut bytes = self.lock();
        debug_assert!(n <= *bytes, "{n} bytes leave a gate that holds {bytes}");
        *bytes = bytes.saturating_sub(n);
        if *bytes < LIMIT {
            self.below_limit.notify_all();
        }
        *bytes
    }

    fn get(&self) -> usize {
        *self.lock()
    }

    fn wait_below_limit(&self) {
        let mut bytes = self.lock();
        while *bytes >= LIMIT {
            bytes = self
                .below_limit
                .wait(bytes)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Waits until nothing is left, or until `deadline`.
    fn wait_empty(&self, deadline: Instant) {
        let mut bytes = self.lock();
        while *bytes > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            // `sub` wakes the waiters whenever the count drops below the
            // limit, so also when it reaches 0.
            bytes = self
                .below_limit
                .wait_timeout(bytes, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A writer thread's queue, and the gate that counts what is on its way
/// to that writer.
#[derive(Debug)]
struct Queue {
    tx: Chunks,
    gate: Arc<Gate>,
}

impl Queue {
    fn push(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.gate.add(bytes.len());
        // A writer that is gone failed, and its error is on its way to the
        // owner, which ends the session.
        let _ = self.tx.send(bytes);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The TCP connection to the bridge.
    Net,
    /// stdin and stdout, or the pty master.
    Local,
}

/// What the threads tell the owner.
#[derive(Debug)]
enum Msg {
    Data(Side, Vec<u8>),
    End(Side),
    ReadError(Side, io::Error),
    WriteError(Side, io::Error),
    /// That side's writer has written everything handed to it so far.
    Flushed,
    /// That side's writer has ended: its queue was closed and written, or
    /// it failed.
    WriterDone(Side),
    Signal(i32),
}

/// Runs a session over `stream` until it ends, and returns how. The
/// calling thread becomes the owner.
pub(crate) fn run(
    setup: Setup<'_>,
    stream: TcpStream,
    io: LocalIo,
    local: &mut impl Local,
) -> Exit {
    if !setup.line.framing_given() {
        ui::notice(
            "setting the port to 8N1 without flow control: only the baud rate can be kept as it is \
             (choose with --frame and --flow)",
        );
    }
    let mut client = Client::new(setup.line.settings());
    // The alloc layer holds these until COM-PORT-OPTION is decided.
    if let Some(dtr) = setup.args.dtr {
        client.set_dtr(dtr.is_on());
    }
    if let Some(rts) = setup.args.rts {
        client.set_rts(rts.is_on());
    }

    let LocalIo {
        reader,
        writer,
        input,
        output,
        throttle,
        drain,
    } = io;
    let (tx, rx) = mpsc::channel();
    let to_net = Arc::new(Gate::default());
    let to_local = Arc::new(Gate::default());
    let started = start_threads(&stream, (reader, writer, throttle), &tx, &to_net, &to_local);
    let (net, out) = match started {
        Ok(queues) => queues,
        Err(e) => {
            ui::error(format_args!("cannot start the session: {e}"));
            return Exit::Io;
        }
    };
    let signals = shutdown::on_signal(move |signal| {
        let _ = tx.send(Msg::Signal(signal));
    });
    let mut engine = Engine {
        ctl: Ctl {
            client,
            pending: Vec::new(),
            net: Queue {
                tx: net,
                gate: to_net,
            },
            out: Queue {
                tx: out,
                gate: to_local,
            },
            settled: false,
            recheck: None,
            timeout: setup.args.timeout,
        },
        local,
        rx,
        setup,
        deadline: after(Instant::now(), setup.args.timeout),
        regraced: false,
        echoing: false,
        linger: None,
        closing: None,
        names: (input, output),
        drain,
    };
    let exit = engine.run();
    engine.local.ending(&mut engine.ctl);
    engine.ctl.flush();
    let exit = engine.finish(exit);
    drop(signals);
    exit
}

/// The local reader and writer, and whether the reader is throttled.
type LocalStreams = (Box<dyn Read + Send>, Box<dyn Write + Send>, bool);

/// The owner's end of a writer thread's queue.
type Chunks = Sender<Vec<u8>>;

/// Starts the four byte movers; returns the socket writer's and the local
/// writer's queues.
fn start_threads(
    stream: &TcpStream,
    (reader, writer, throttle): LocalStreams,
    tx: &Sender<Msg>,
    to_net: &Arc<Gate>,
    to_local: &Arc<Gate>,
) -> io::Result<(Chunks, Chunks)> {
    let (net_tx, net_rx) = mpsc::channel();
    let (out_tx, out_rx) = mpsc::channel();
    let socket = stream.try_clone()?;
    spawn("socket reader", {
        let (tx, gate) = (tx.clone(), Arc::clone(to_local));
        move || read(Side::Net, socket, &gate, true, &tx)
    })?;
    let socket = stream.try_clone()?;
    spawn("socket writer", {
        let (tx, gate) = (tx.clone(), Arc::clone(to_net));
        move || write(Side::Net, socket, &net_rx, &gate, &tx)
    })?;
    spawn("local reader", {
        let (tx, gate) = (tx.clone(), Arc::clone(to_net));
        move || read(Side::Local, reader, &gate, throttle, &tx)
    })?;
    spawn("local writer", {
        let (tx, gate) = (tx.clone(), Arc::clone(to_local));
        move || write(Side::Local, writer, &out_rx, &gate, &tx)
    })?;
    Ok((net_tx, out_tx))
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new().name(name.into()).spawn(f).map(drop)
}

/// A reader thread: chunks from `source` to the owner, each counted in
/// `gate` until the other side's writer has written it.
fn read(side: Side, mut source: impl Read, gate: &Gate, throttle: bool, tx: &Sender<Msg>) {
    let mut buf = vec![0; CHUNK];
    loop {
        if throttle {
            gate.wait_below_limit();
        }
        let msg = match source.read(&mut buf) {
            Ok(0) => Msg::End(side),
            Ok(n) => {
                gate.add(n);
                Msg::Data(side, buf[..n].to_vec())
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Msg::ReadError(side, e),
        };
        let last = !matches!(msg, Msg::Data(..));
        if tx.send(msg).is_err() || last {
            return;
        }
    }
}

/// A writer thread: chunks from the owner to `sink`, until the owner closes
/// the queue or a write fails.
fn write(side: Side, mut sink: impl Write, rx: &Receiver<Vec<u8>>, gate: &Gate, tx: &Sender<Msg>) {
    for chunk in rx {
        if let Err(e) = sink.write_all(&chunk).and_then(|()| sink.flush()) {
            let _ = tx.send(Msg::WriteError(side, e));
            break;
        }
        if gate.sub(chunk.len()) == 0 {
            let _ = tx.send(Msg::Flushed);
        }
    }
    let _ = tx.send(Msg::WriterDone(side));
}

/// After the local input ended: the port's output is copied until it has
/// been quiet for `quiet` since `since`.
#[derive(Debug)]
struct Linger {
    quiet: Duration,
    /// The last time the port sent data, a writer emptied its queue, or the
    /// input ended. The local writer counts: while its queue is full, the
    /// socket reader is paused and the port's output waits unread in the
    /// socket, which is not the port being quiet.
    since: Instant,
}

impl Linger {
    fn end(&self) -> Instant {
        after(self.since, self.quiet)
    }
}

/// The owner's state.
struct Engine<'a, L> {
    ctl: Ctl,
    local: &'a mut L,
    rx: Receiver<Msg>,
    setup: Setup<'a>,
    /// Until when the negotiation may take.
    deadline: Instant,
    /// The deadline was put off once already, after it was found long past
    /// ([`LATE`]).
    regraced: bool,
    /// The peer echoed one of our requests back (review finding vc-2).
    echoing: bool,
    linger: Option<Linger>,
    /// The socket writer found the connection closed: the session ends when
    /// the socket reader has delivered the rest (it may have the bridge's
    /// last output), or at this time.
    closing: Option<Instant>,
    /// [`LocalIo::input`] and [`LocalIo::output`].
    names: (&'static str, &'static str),
    /// [`LocalIo::drain`].
    drain: Option<Duration>,
}

impl<L: Local> Engine<'_, L> {
    fn run(&mut self) -> Exit {
        loop {
            let now = Instant::now();
            if let Some(exit) = self.check(now) {
                return exit;
            }
            self.ctl.flush();
            let msg = match self.wake(now) {
                Some(at) => match self.rx.recv_timeout(at.saturating_duration_since(now)) {
                    Ok(msg) => msg,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return Exit::Io,
                },
                // The signal sink keeps a sender while the session runs,
                // so the channel cannot disconnect under the owner.
                None => match self.rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return Exit::Io,
                },
            };
            let exit = self.handle(msg);
            // Also on the way out: what was typed before the quit key goes.
            self.ctl.flush();
            if let Some(exit) = exit {
                return exit;
            }
            if self.ctl.net.gate.get() > NET_CAP {
                ui::error(format_args!(
                    "{} does not read what is sent to it ({} KiB waiting)",
                    self.setup.target,
                    NET_CAP / 1024
                ));
                return Exit::Io;
            }
        }
    }

    /// The deadlines: readiness, a settings change, the local tick, the end
    /// of the linger.
    fn check(&mut self, now: Instant) -> Option<Exit> {
        if !self.ctl.settled {
            if let Some(exit) = self.negotiate(now) {
                return Some(exit);
            }
        }
        self.ctl.recheck(now);
        if self.local.next_tick().is_some_and(|at| at <= now) {
            if let Some(exit) = self.local.tick(now, &mut self.ctl) {
                return Some(exit);
            }
        }
        if self.closing.is_some_and(|at| now >= at) {
            return Some(self.remote_closed(None));
        }
        // Not before the policy has settled: an input that ended at once
        // must not turn a port that never answers into a success, nor end
        // the session before the settings have gone out. And not while
        // anything is on its way: in either direction.
        match &self.linger {
            Some(linger)
                if self.ctl.settled
                    && now >= linger.end()
                    && self.ctl.net.gate.get() == 0
                    && self.ctl.out.gate.get() == 0 =>
            {
                ui::verbose("the input ended and the port is quiet");
                Some(Exit::Ok)
            }
            _ => None,
        }
    }

    /// When the owner must look again without a message, if ever.
    fn wake(&self, now: Instant) -> Option<Instant> {
        let linger = self
            .linger
            .as_ref()
            .map(Linger::end)
            // Once it is over, what is left is in the writers' queues, and
            // their Flushed messages wake the owner.
            .filter(|&at| at > now);
        [
            (!self.ctl.settled).then_some(self.deadline),
            self.ctl.recheck,
            self.local.next_tick(),
            linger,
            self.closing,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// The readiness policy: `None` while it is still open, or when it has
    /// settled the session (input flows from now on), `Some` when it failed.
    fn negotiate(&mut self, now: Instant) -> Option<Exit> {
        let target = self.setup.target;
        let args = self.setup.args;
        if self.echoing {
            ui::error(format_args!(
                "{target} echoes the RFC 2217 requests back: this looks like a raw TCP port in \
                 front of a device that echoes (or a loopback), not RFC 2217"
            ));
            return Some(Exit::Negotiation);
        }
        let late = now >= self.deadline;
        if late && !self.regraced && now.saturating_duration_since(self.deadline) > LATE {
            // The process was not running at the deadline (stopped by `z`
            // or from outside): the bridge may have answered meanwhile.
            self.regraced = true;
            self.deadline = now + LATE_GRACE;
            return None;
        }
        let client = &self.ctl.client;
        let timeout = Secs(args.timeout);
        match client.readiness() {
            Ok(()) => ui::verbose(format_args!(
                "the bridge confirmed {}",
                Line::from(client.reported_settings())
            )),
            Err(e @ NotReady::Refused) if args.allow_telnet => ui::warn(format_args!(
                "{e}: carrying on as a plain Telnet session (--allow-telnet)"
            )),
            Err(e @ NotReady::Refused) => {
                ui::error(format_args!(
                    "{e}; --allow-telnet carries on without RFC 2217"
                ));
                return Some(Exit::Negotiation);
            }
            Err(NotReady::Mismatched(settings)) => {
                // Wait for the other answers, so that the report is complete.
                if !late && !client.pending_settings().is_empty() {
                    return None;
                }
                if args.strict {
                    ui::error(mismatch(client, settings));
                    return Some(Exit::Negotiation);
                }
                ui::warn(mismatch(client, settings));
            }
            Err(NotReady::Negotiating) if late => {
                ui::error(format_args!(
                    "no answer to the RFC 2217 COM-PORT-OPTION offer in {timeout}: {target} \
                     probably is a raw TCP port, not RFC 2217"
                ));
                return Some(Exit::Negotiation);
            }
            // Pending, or a reason a later library version adds.
            Err(e) if late => {
                let reason = match e {
                    NotReady::Pending(settings) => {
                        format!("no answer from the bridge in {timeout} for: {settings}")
                    }
                    e => format!("{e} after {timeout}"),
                };
                if args.strict {
                    ui::error(format_args!("{reason} (--strict)"));
                    return Some(Exit::Negotiation);
                }
                ui::warn(format_args!(
                    "{reason}; carrying on (--strict makes this an error)"
                ));
            }
            Err(_) => return None,
        }
        self.ctl.settled = true;
        self.local.settled(&mut self.ctl);
        None
    }

    fn handle(&mut self, msg: Msg) -> Option<Exit> {
        let target = self.setup.target;
        match msg {
            Msg::Data(Side::Net, bytes) => {
                self.received(&bytes);
                None
            }
            Msg::Data(Side::Local, bytes) => {
                let exit = self.local.input(&bytes, &mut self.ctl);
                // After `input`, which moved them to `pending` or dropped
                // them: the gate never dips below what is really held.
                self.ctl.net.gate.sub(bytes.len());
                exit
            }
            Msg::End(Side::Net) => Some(self.remote_closed(None)),
            Msg::ReadError(Side::Net, e) if closed_by_peer(&e) => Some(self.remote_closed(Some(e))),
            // The socket reader may still have the bridge's last output: the
            // session ends when it reaches the end of the stream, or once the
            // local writer's drain time has passed.
            Msg::WriteError(Side::Net, e) if closed_by_peer(&e) => {
                ui::verbose(format_args!("writing to {target}: {e}"));
                if self.closing.is_none() {
                    self.closing = Some(after(Instant::now(), self.drain.unwrap_or(FOREVER)));
                }
                None
            }
            Msg::ReadError(Side::Net, e) => {
                ui::error(format_args!("reading from {target}: {e}"));
                Some(Exit::Io)
            }
            Msg::WriteError(Side::Net, e) => {
                ui::error(format_args!("writing to {target}: {e}"));
                Some(Exit::Io)
            }
            Msg::End(Side::Local) => match self.local.input_ended() {
                InputEnd::Linger(quiet) => {
                    ui::verbose(format_args!(
                        "the input ended: waiting until the port is quiet for {}",
                        Secs(quiet)
                    ));
                    self.linger = Some(Linger {
                        quiet,
                        since: Instant::now(),
                    });
                    None
                }
                InputEnd::Exit(exit) => Some(exit),
            },
            Msg::ReadError(Side::Local, e) => {
                ui::error(format_args!("reading {}: {e}", self.names.0));
                Some(Exit::Io)
            }
            // Whoever read the output is gone (`| head`): nothing is lost
            // that anyone would see.
            Msg::WriteError(Side::Local, e) if e.kind() == io::ErrorKind::BrokenPipe => {
                ui::verbose(format_args!("{} was closed", self.names.1));
                Some(Exit::Ok)
            }
            Msg::WriteError(Side::Local, e) => {
                ui::error(format_args!("writing {}: {e}", self.names.1));
                Some(Exit::Io)
            }
            Msg::Flushed => {
                if let Some(linger) = &mut self.linger {
                    linger.since = Instant::now();
                }
                None
            }
            Msg::WriterDone(_) => None,
            Msg::Signal(signal) => {
                tracing::debug!(target: ui::LOG, "caught signal {signal}");
                Some(Exit::Signal(signal))
            }
        }
    }

    fn received(&mut self, bytes: &[u8]) {
        let payload = self.ctl.client.receive(bytes);
        if !payload.is_empty() {
            if let Some(linger) = &mut self.linger {
                linger.since = Instant::now();
            }
            let shown = self.local.output(payload);
            self.ctl.out.push(shown);
        }
        // After the push, as for local input.
        self.ctl.out.gate.sub(bytes.len());
        while let Some(event) = self.ctl.client.poll_event() {
            self.event(&event);
        }
    }

    fn event(&mut self, event: &Event) {
        ui::verbose(format_args!("bridge: {event:?}"));
        match event {
            Event::FlowSuspended => {
                ui::notice("the bridge suspended the data flow: holding the input until it resumes")
            }
            Event::FlowResumed => ui::notice("the bridge resumed the data flow"),
            Event::ComPortRefused if self.ctl.settled => {
                ui::warn("the bridge withdrew RFC 2217: settings and control lines no longer work")
            }
            // Requests come back with their client-direction codes (below
            // 100): the peer echoes what it receives.
            Event::Unknown(body) if !self.ctl.settled => {
                if let [44, code, ..] = body.as_bytes() {
                    self.echoing |= *code < 100;
                }
            }
            _ => {}
        }
        self.local.event(event, &mut self.ctl);
    }

    fn remote_closed(&mut self, error: Option<io::Error>) -> Exit {
        if let Some(e) = error {
            ui::verbose(format_args!("the connection ended with: {e}"));
        }
        // Nothing more will come: the policy decides now, as at its deadline.
        if !self.ctl.settled {
            if !self.echoing && self.ctl.client.com_port_state() == ComPortState::Negotiating {
                ui::error(format_args!(
                    "{} closed the connection before answering the RFC 2217 offer",
                    self.setup.target
                ));
                return Exit::Negotiation;
            }
            let now = Instant::now();
            self.deadline = now;
            if let Some(exit) = self.negotiate(now) {
                return exit;
            }
        }
        let exit = self.local.remote_closed();
        match exit {
            Exit::Ok => ui::notice("the bridge closed the connection"),
            _ => ui::error("the bridge closed the connection"),
        }
        exit
    }

    /// Lets the writers finish: the socket writer for at most [`NET_DRAIN`],
    /// the local writer as long as [`LocalIo::drain`] allows. After a signal
    /// only the socket writer, for at most [`SIGNAL_DRAIN`]; a signal during
    /// the wait ends it at once.
    fn finish(self, exit: Exit) -> Exit {
        let signalled = matches!(exit, Exit::Signal(_));
        let Self { ctl, rx, drain, .. } = self;
        // Closing the queues ends the writers once they wrote them out.
        let Ctl { net, out, .. } = ctl;
        drop((net, out));
        let start = Instant::now();
        let net_drain = if signalled { SIGNAL_DRAIN } else { NET_DRAIN };
        // Nobody waits for the local writer after a signal.
        let (mut net_done, mut out_done) = (false, signalled);
        while !(net_done && out_done) {
            let limit = if net_done {
                drain.map(|drain| start + drain)
            } else {
                Some(start + net_drain)
            };
            let msg = match limit {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(Msg::WriterDone(Side::Net)) => net_done = true,
                Ok(Msg::WriterDone(Side::Local)) => out_done = true,
                Ok(Msg::Signal(signal)) if !signalled => return Exit::Signal(signal),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) if !net_done => net_done = true,
                Err(_) => break,
            }
        }
        exit
    }
}

/// The bridge closed the connection, abortively (it closed with our data
/// unread) or while we were writing.
fn closed_by_peer(e: &io::Error) -> bool {
    use io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    matches!(
        e.kind(),
        BrokenPipe | ConnectionAborted | ConnectionReset | UnexpectedEof
    )
}

/// A duration in messages: `3 s`, `0.25 s`.
#[derive(Debug, Clone, Copy)]
struct Secs(Duration);

impl fmt::Display for Secs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} s", self.0.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_settings_in_messages() {
        let requested = PortSettings {
            baud_rate: 0,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::OnePointFive,
            flow_control: FlowControl::RtsCts,
        };
        assert_eq!(
            Line::from(requested).to_string(),
            "kept baud rate, 7E1.5, RTS/CTS flow control"
        );
        let reported = PartialSettings {
            baud_rate: Some(115_200),
            data_bits: Some(DataBits::Eight),
            ..PartialSettings::default()
        };
        assert_eq!(
            Line::from(reported).to_string(),
            "115200 baud, 8??, flow control ?"
        );
    }

    #[test]
    fn durations_in_messages() {
        assert_eq!(Secs(Duration::from_secs(3)).to_string(), "3 s");
        assert_eq!(Secs(Duration::from_millis(250)).to_string(), "0.25 s");
    }

    #[test]
    fn a_gate_wakes_its_reader_below_the_limit() {
        let gate = Arc::new(Gate::default());
        gate.add(LIMIT);
        let reader = thread::spawn({
            let gate = Arc::clone(&gate);
            move || gate.wait_below_limit()
        });
        assert_eq!(gate.sub(1), LIMIT - 1);
        reader.join().unwrap();
        assert_eq!(gate.sub(LIMIT - 1), 0);
    }
}
