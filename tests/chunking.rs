//! Invariance (O1): payload, events with their positions, output, the
//! results of the caller's calls and state depend only on the input bytes
//! and on where the caller's own calls were made: not on how the input is
//! split, on the payload buffer size, on how many bytes each socket write
//! takes, or on when output is drained. The alloc layer produces the same
//! transcript as the core. Also here: a client and a server converge for
//! every setting, data is transparent both ways, and arbitrary input never
//! panics. The core parts run in every configuration.

mod common;

use std::fmt::Debug;

use common::{Wire, hex, port, pump, replies, requests, sb, ser2net_stream};
use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, InlineBytes, LineState,
    ModemState, Parity, PortSettings, Progress, Purge, Server, ServerEvent, StopBits,
};

/// A call the session makes at a fixed input position.
#[derive(Debug, Clone)]
enum Call {
    Data(Vec<u8>),
    Dtr(bool),
    Rts(bool),
    Break(bool),
    Purge(Purge),
    Signature,
    Settings(PortSettings),
    ModemState(ModemState),
    LineState(LineState),
    Suspend,
    Resume,
}

/// An input stream, and the calls made once exactly `position` input bytes
/// have been processed (sorted by position).
#[derive(Debug, Default)]
struct Session {
    input: Vec<u8>,
    calls: Vec<(usize, Call)>,
}

impl Session {
    /// Append input.
    fn rx(&mut self, bytes: impl AsRef<[u8]>) {
        self.input.extend_from_slice(bytes.as_ref());
    }

    /// Append input given as hex, one string per socket read.
    fn reads(&mut self, reads: &[&str]) {
        for read in reads {
            self.rx(hex(read));
        }
    }

    /// Make `call` once all input so far has been processed.
    fn call(&mut self, call: Call) {
        self.calls.push((self.input.len(), call));
    }
}

/// Everything a run observed.
#[derive(Debug, Clone, PartialEq)]
struct Transcript<E> {
    payload: Vec<u8>,
    /// Each event with its position: the input bytes processed and the
    /// payload length when it was polled.
    events: Vec<(usize, usize, E)>,
    /// What each of the session's calls returned in the end, after the
    /// retries of driving rule DL4. Alloc-layer runs record none: their
    /// calls return nothing.
    calls: Vec<Result<(), CommandError>>,
    /// Every byte written to the socket, the opening included.
    output: Vec<u8>,
    /// The observable state at the end.
    state: String,
}

impl<E: Clone> Transcript<E> {
    fn new() -> Self {
        Self {
            payload: Vec::new(),
            events: Vec::new(),
            calls: Vec::new(),
            output: Vec::new(),
            state: String::new(),
        }
    }

    /// How many events match.
    fn count(&self, matches: impl Fn(&E) -> bool) -> usize {
        self.events.iter().filter(|(_, _, e)| matches(e)).count()
    }

    /// How many calls returned `result`.
    fn results(&self, result: Result<(), CommandError>) -> usize {
        self.calls.iter().filter(|&&r| r == result).count()
    }
}

#[derive(Debug, Clone, Copy)]
enum Split {
    /// Chunks end only at call positions.
    Whole,
    /// One byte per chunk.
    Bytes,
    /// Random chunks of 1 to 97 bytes.
    Random,
    /// Chunks end at this input position and at call positions: every
    /// two-way split of a session without calls.
    At(usize),
}

impl Split {
    /// Length of the chunk that starts at input position `at`, before it is
    /// cut at `limit`, the next call position.
    fn next(self, at: usize, limit: usize, rng: &mut XorShift) -> usize {
        match self {
            Split::Whole => limit - at,
            Split::Bytes => 1,
            Split::Random => 1 + rng.below(97),
            Split::At(position) if at < position => position - at,
            Split::At(_) => limit - at,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Config {
    split: Split,
    /// Length of the payload buffer passed to `receive_into`.
    payload: usize,
    /// Most bytes one socket write takes.
    write_limit: usize,
    /// Write output only when the core pushes back (a stalled receive or a
    /// refused call), instead of draining after every call.
    lazy: bool,
    /// Seed of the random splits.
    seed: u64,
}

/// Whole input, a 4096-byte payload buffer, unlimited writes, full drains.
const REFERENCE: Config = Config {
    split: Split::Whole,
    payload: 4096,
    write_limit: usize::MAX,
    lazy: false,
    seed: 1,
};

/// A random configuration: random splits, payload buffers of
/// {1, 2, 3, 7, 64, 4096} bytes, write limits of {1, 2, 5, 13, 64, 300,
/// unlimited} bytes, full or lazy drains.
fn random_config(rng: &mut XorShift) -> Config {
    Config {
        split: Split::Random,
        payload: [1, 2, 3, 7, 64, 4096][rng.below(6)],
        write_limit: [1, 2, 5, 13, 64, 300, usize::MAX][rng.below(7)],
        lazy: rng.below(2) == 0,
        seed: rng.next() | 1,
    }
}

/// How often a run had to push back.
#[derive(Debug, Default)]
struct Counts {
    /// `receive_into` calls that consumed nothing.
    stalls: usize,
    /// Calls refused with `OutputFull`, or data taken only in part.
    retries: usize,
}

/// xorshift64: a small, seeded, dependency-free PRNG.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The core API of a half, as the harness drives it.
trait Driven {
    type Event: Debug + Clone + PartialEq;
    fn step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress;
    fn pending(&self) -> &[u8];
    fn written(&mut self, n: usize);
    fn next_event(&mut self) -> Option<Self::Event>;
    /// One attempt at `call`, whose data's first `done` bytes were taken
    /// already. Returns how many more data bytes were taken.
    fn attempt(&mut self, call: &Call, done: usize) -> Result<usize, CommandError>;
    fn state(&self) -> String;
}

impl Driven for Client {
    type Event = Event;

    fn step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress {
        self.receive_into(input, payload)
    }

    fn pending(&self) -> &[u8] {
        self.output()
    }

    fn written(&mut self, n: usize) {
        self.consume_output(n);
    }

    fn next_event(&mut self) -> Option<Event> {
        self.poll_event()
    }

    fn attempt(&mut self, call: &Call, done: usize) -> Result<usize, CommandError> {
        match call {
            Call::Data(data) => self.try_write_data(&data[done..]),
            &Call::Dtr(on) => self.try_set_dtr(on).map(|()| 0),
            &Call::Rts(on) => self.try_set_rts(on).map(|()| 0),
            &Call::Break(on) => self.try_set_break(on).map(|()| 0),
            &Call::Purge(purge) => self.try_purge(purge).map(|()| 0),
            Call::Signature => self.try_request_signature().map(|()| 0),
            &Call::Settings(settings) => self.try_set_port_settings(settings).map(|()| 0),
            other => panic!("not a client call: {other:?}"),
        }
    }

    fn state(&self) -> String {
        format!(
            "{:?} {:?} suspended={} modem={:?} requested={:?} reported={:?}",
            self.com_port_state(),
            self.readiness(),
            self.flow_suspended(),
            self.modem_state(),
            self.requested_settings(),
            self.reported_settings()
        )
    }
}

impl Driven for Server {
    type Event = ServerEvent;

    fn step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress {
        self.receive_into(input, payload)
    }

    fn pending(&self) -> &[u8] {
        self.output()
    }

    fn written(&mut self, n: usize) {
        self.consume_output(n);
    }

    fn next_event(&mut self) -> Option<ServerEvent> {
        self.poll_event()
    }

    fn attempt(&mut self, call: &Call, done: usize) -> Result<usize, CommandError> {
        match call {
            Call::Data(data) => self.try_write_data(&data[done..]),
            &Call::ModemState(state) => self.try_set_modem_state(state).map(|()| 0),
            &Call::LineState(state) => self.try_notify_line_state(state).map(|()| 0),
            Call::Suspend => self.try_suspend_flow().map(|()| 0),
            Call::Resume => self.try_resume_flow().map(|()| 0),
            other => panic!("not a server call: {other:?}"),
        }
    }

    fn state(&self) -> String {
        format!("{:?} active={}", self.settings(), self.com_port_active())
    }
}

/// One socket write of at most `limit` bytes; returns how many were written.
fn write_once<H: Driven>(half: &mut H, limit: usize, wire: &mut Vec<u8>) -> usize {
    let pending = half.pending();
    let n = pending.len().min(limit);
    wire.extend_from_slice(&pending[..n]);
    half.written(n);
    n
}

/// Socket writes of at most `limit` bytes until nothing is pending.
fn drain<H: Driven>(half: &mut H, limit: usize, wire: &mut Vec<u8>) {
    while write_once(half, limit, wire) > 0 {}
}

/// Make `call`, writing out output and retrying while the core pushes back
/// (driving rule DL4); returns what the call returned in the end.
fn issue<H: Driven>(
    half: &mut H,
    call: &Call,
    cfg: Config,
    wire: &mut Vec<u8>,
    n: &mut Counts,
) -> Result<(), CommandError> {
    let len = match call {
        Call::Data(data) => data.len(),
        _ => 0,
    };
    let mut done = 0;
    let result = loop {
        match half.attempt(call, done) {
            Ok(taken) => {
                done += taken;
                if done == len {
                    break Ok(());
                }
            }
            Err(CommandError::OutputFull) => {}
            Err(error) => break Err(error),
        }
        n.retries += 1;
        assert!(
            write_once(half, cfg.write_limit, wire) > 0,
            "{call:?} pushed back with nothing to write"
        );
    };
    if !cfg.lazy {
        drain(half, cfg.write_limit, wire);
    }
    result
}

/// Run `session` through the core API of `half` as `cfg` says.
fn run<H: Driven>(
    mut half: H,
    session: &Session,
    cfg: Config,
    n: &mut Counts,
) -> Transcript<H::Event> {
    let mut t = Transcript::new();
    let mut rng = XorShift(cfg.seed);
    let mut buf = vec![0; cfg.payload];
    let input = &session.input[..];
    let mut calls = session.calls.iter().peekable();
    let mut at = 0;
    loop {
        while let Some((_, call)) = calls.next_if(|&&(position, _)| position == at) {
            let result = issue(&mut half, call, cfg, &mut t.output, n);
            t.calls.push(result);
        }
        if at == input.len() {
            break;
        }
        let limit = calls.peek().map_or(input.len(), |&&(position, _)| position);
        let end = (at + cfg.split.next(at, limit, &mut rng)).min(limit);
        while at < end {
            let p = half.step(&input[at..end], &mut buf);
            at += p.consumed;
            t.payload.extend_from_slice(&buf[..p.payload_len]);
            while let Some(event) = half.next_event() {
                t.events.push((at, t.payload.len(), event));
            }
            if p.consumed == 0 {
                n.stalls += 1;
                assert!(
                    write_once(&mut half, cfg.write_limit, &mut t.output) > 0,
                    "stalled with nothing to write"
                );
            } else if !cfg.lazy {
                drain(&mut half, cfg.write_limit, &mut t.output);
            }
        }
    }
    drain(&mut half, cfg.write_limit, &mut t.output);
    t.state = half.state();
    t
}

/// Feed all of `input`, polling events and writing out output whenever a
/// call consumed nothing; returns the payload.
fn drive<H: Driven>(half: &mut H, input: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut buf = [0; 64];
    let mut at = 0;
    while at < input.len() {
        let p = half.step(&input[at..], &mut buf);
        at += p.consumed;
        payload.extend_from_slice(&buf[..p.payload_len]);
        while half.next_event().is_some() {}
        if p.consumed == 0 {
            drain(half, usize::MAX, &mut Vec::new());
        }
    }
    payload
}

/// `data` as it appears on the wire: every 0xFF doubled.
fn escape(data: &[u8]) -> Vec<u8> {
    data.iter()
        .flat_map(|&b| if b == 0xFF { vec![0xFF; 2] } else { vec![b] })
        .collect()
}

/// Where `needle` first occurs in `haystack`.
fn position(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("present")
}

// ---- sessions ---------------------------------------------------------------

/// The ser2net 4.6.4 capture toward a client, with the calls the capture
/// made: once the settings are confirmed a purge and a signature request
/// (which ser2net answers with an empty signature), then data.
fn ser2net_session() -> Session {
    let stream = ser2net_stream();
    let purged = position(&stream, &sb(112, &[1]));
    let data = position(&stream, b"JULABO");
    let mut s = Session::default();
    s.rx(&stream[..purged]);
    s.call(Call::Purge(Purge::Receive));
    s.call(Call::Signature);
    s.rx(&stream[purged..data]);
    s.call(Call::Data(b"version\r".to_vec()));
    s.rx(&stream[data..]);
    s
}

/// What the sredird capture wrote to the port: every byte value, two more
/// 0xFF and line ends.
fn sredird_data() -> Vec<u8> {
    (0..=255)
        .chain([0xFF, 0xFF, 0x0D, 0x0A, 0x0D, 0x00])
        .collect()
}

/// sredird 2.2.2 (Debian, LP64, socat + pty) toward a client, copied from
/// the release audit's capture (`t6_sredird.log`) one string per read, with
/// the calls the capture made between them: 8-byte SET-BAUDRATE replies,
/// NOTIFY-MODEMSTATE bursts after every exchange, 8N1 acknowledged for every
/// request on the pty, DTR and RTS answered with the flow value, a
/// signature, and the device's echo of the data written last.
fn sredird_session() -> Session {
    let mut s = Session::default();
    s.reads(&[
        "FF FB 00 FF FD 00 FF FB 01 FF FB 03 FF FD 03 FF FD 2C",
        "FF FB 2C FF FA 2C 6B 88 FF F0 FF FA 2C 6B F7 FF F0",
        "FF FA 2C 6B F0 FF F0",
        "FF FA 2C 65 00 00 25 80 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0",
        "FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        19_200,
        DataBits::Seven,
        Parity::Even,
        StopBits::Two,
        FlowControl::None,
    )));
    s.reads(&[
        "FF FA 2C 65",
        "00 00 4B 00 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 02 FF F0 FF",
        "FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0",
        "FF FA 2C",
        "6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        1200,
        DataBits::Five,
        Parity::Odd,
        StopBits::OnePointFive,
        FlowControl::None,
    )));
    s.reads(&[
        "FF FA 2C 65 00 00 04 B0 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA",
        "2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        57_600,
        DataBits::Six,
        Parity::Mark,
        StopBits::One,
        FlowControl::XonXoff,
    )));
    s.reads(&[
        "FF FA 2C 65",
        "00 00 E1 00 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA",
        "2C 69 02 FF F0 FF FA 2C 6B 1E FF F0",
        "FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        115_200,
        DataBits::Eight,
        Parity::Space,
        StopBits::Two,
        FlowControl::RtsCts,
    )));
    s.reads(&[
        "FF FA 2C 65 00 01 C2 00",
        "00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 02 FF F0 FF FA 2C 69 03 FF F0 FF FA 2C 6B 1E FF",
        "F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0",
        "FF F0",
    ]);
    s.call(Call::Settings(port(
        38_400,
        DataBits::Eight,
        Parity::None,
        StopBits::One,
        FlowControl::None,
    )));
    s.reads(&[
        "FF FA 2C 65",
        "00 00 96 00 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0",
        "FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0",
        "FF FA 2C 6B",
        "FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        250_000,
        DataBits::Eight,
        Parity::None,
        StopBits::One,
        FlowControl::None,
    )));
    s.reads(&[
        "FF FA 2C 65 00 00 25 80 00 00",
        "00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E",
        "FF F0 FF FA",
        "2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        9600,
        DataBits::Eight,
        Parity::None,
        StopBits::One,
        FlowControl::Dcd,
    )));
    s.reads(&[
        "FF FA 2C 65 00 00 25 80 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        9600,
        DataBits::Eight,
        Parity::None,
        StopBits::One,
        FlowControl::Dsr,
    )));
    s.reads(&[
        "FF FA 2C 65 00 00 25 80 00",
        "00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF",
        "FA 2C 6B 1E FF F0",
        "FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Settings(port(
        9600,
        DataBits::Eight,
        Parity::None,
        StopBits::One,
        FlowControl::None,
    )));
    s.reads(&[
        "FF FA 2C 65 00 00 25 80 00 00",
        "00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF",
        "FA 2C 6B 1E FF F0",
        "FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Dtr(false));
    s.reads(&[
        "FF FA 2C 69 01 FF F0 FF FA",
        "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Rts(false));
    s.reads(&[
        "FF FA 2C 69 01 FF F0 FF FA",
        "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Break(true));
    s.reads(&[
        "FF FA 2C 69 05 FF F0 FF FA",
        "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Break(false));
    s.reads(&[
        "FF FA 2C 69 06 FF F0 FF FA",
        "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0",
        "FF F0",
    ]);
    s.call(Call::Dtr(true));
    s.reads(&[
        "FF FA 2C 69 01 FF F0",
        "FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Rts(true));
    s.reads(&[
        "FF FA 2C 69 01 FF F0 FF FA",
        "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Purge(Purge::Receive));
    s.reads(&[
        "FF FA 2C 70 01 FF F0 FF FA",
        "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Purge(Purge::Transmit));
    s.reads(&[
        "FF FA 2C 70 02 FF F0 FF FA",
        "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C",
        "6B F0 FF F0",
    ]);
    s.call(Call::Purge(Purge::Both));
    s.reads(&[
        "FF FA 2C 70 03 FF F0 FF FA 2C 6B",
        "1E FF F0 FF FA 2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    s.call(Call::Signature);
    s.reads(&[
        "FF FA 2C 64 53 52 65 64",
        "69 72 64 20 32 2E 32 2E 32 20 2F 64 65 76 2F 74 74 79 56 30 FF F0 FF FA 2C 6B 1E FF F0",
        "FF FA",
        "2C 6B FE FF F0",
        "FF FA 2C 6B F0 FF F0",
    ]);
    let data = sredird_data();
    s.call(Call::Data(data.clone()));
    // The device echoed it. The capture logs only protocol reads, so the
    // echo is the data as sredird sends it: escaped.
    s.rx(escape(&data));
    s
}

/// The ser2net session, then a hostile tail: refused ECHO offers
/// interleaved with data and a DTR pulse train, data containing 0xFF, a
/// reconfiguration with a 0xFF baud byte and its replies, a volunteered
/// 100-byte signature, a 72-byte unknown body, refusals with data that fill
/// the output buffer, and a subnegotiation given up after 1024 bytes with
/// data before and after it. Then the lifecycle: a signature request
/// answered with text and followed by the server's own request,
/// edge-triggered flow control, a withdrawal with calls and replies while
/// Refused, and a re-activation whose fresh round carries the settings
/// requested meanwhile.
fn client_session() -> Session {
    let mut s = ser2net_session();
    for i in 0..40 {
        s.rx(hex("FF FB 01"));
        s.rx([b'd', i]);
        if i % 2 == 0 {
            s.call(Call::Dtr(i % 4 == 0));
        }
    }
    s.rx(hex("41 FF FF 42 FF FF FF FF 43"));
    s.call(Call::Data(vec![0xFF; 150]));
    s.rx(sb(105, &[8]));
    s.rx(sb(105, &[9]));
    let baud_with_ff = 0x0001_C2FF;
    s.call(Call::Settings(port(
        baud_with_ff,
        DataBits::Seven,
        Parity::Even,
        StopBits::Two,
        FlowControl::RtsCts,
    )));
    s.rx(replies(baud_with_ff, [7, 3, 2, 3]));
    s.rx(sb(100, &[b's'; 100]));
    s.rx(sb(113, &[0xFF; 70]));
    for i in 0..100 {
        s.rx(hex("FF FD 18"));
        s.rx([b'z', i]);
    }
    s.rx(b"<");
    s.rx(hex("FF FA 2C 64"));
    s.rx([b'g'; 1100]);
    s.rx(hex("FF F0"));
    s.rx(b">");

    s.call(Call::Signature);
    s.rx(sb(100, b"ser2net 4.6.4"));
    s.rx(sb(100, &[]));
    for code in [108, 108, 109, 109] {
        s.rx(sb(code, &[]));
    }
    s.rx(sb(107, &[0x31]));
    s.call(Call::Rts(false));
    s.call(Call::Break(true));
    s.rx(sb(105, &[12]));
    s.rx(sb(105, &[5]));

    // Withdrawn: commands are refused, data flows, replies are still
    // decoded, a repeated refusal and the peer's own WILL/WONT 44 change
    // nothing, and new settings wait for the next round.
    s.rx(hex("FF FE 2C"));
    s.call(Call::Dtr(false));
    s.call(Call::Purge(Purge::Both));
    s.call(Call::Signature);
    s.call(Call::Data(b"plain \xFF telnet".to_vec()));
    s.call(Call::Settings(port(
        57_600,
        DataBits::Six,
        Parity::Odd,
        StopBits::OnePointFive,
        FlowControl::XonXoff,
    )));
    s.rx(sb(101, &57_600u32.to_be_bytes()));
    s.rx(sb(107, &[0x20]));
    s.rx(hex("FF FE 2C FF FB 2C FF FC 2C"));
    s.rx(b"refused\r\n");

    // Re-enabled: WILL 44 and a fresh round, answered in full.
    s.rx(hex("FF FD 2C"));
    s.rx(replies(57_600, [6, 2, 3, 2]));
    s.rx(b"end\r\n");
    s.call(Call::Data(b"bye".to_vec()));
    s
}

/// pyserial 3.5's client toward a server, copied from the audit's capture
/// (`server.log`, its `<-` lines) one array per read, with the calls of
/// the replay: the port's modem state, supplied before the client
/// connects, and the echo of the client's data.
fn pyserial_session() -> Session {
    let mut s = Session::default();
    s.call(Call::ModemState(ModemState::CTS | ModemState::DSR));
    s.rx([255, 253, 1]);
    s.rx([255, 251, 3, 255, 253, 3, 255, 253, 44, 255, 251, 44]);
    s.rx([255, 250, 44, 1, 0, 0, 75, 0, 255, 240]);
    s.rx([
        255, 250, 44, 2, 7, 255, 240, 255, 250, 44, 3, 3, 255, 240, 255, 250, 44, 4, 2, 255, 240,
    ]);
    s.rx([255, 250, 44, 5, 3, 255, 240]);
    s.rx([255, 250, 44, 5, 8, 255, 240]);
    s.rx([255, 250, 44, 12, 1, 255, 240]);
    s.rx([255, 250, 44, 12, 2, 255, 240]);
    s.rx([104, 101, 108, 108, 111, 255, 255, 13, 10]);
    s.call(Call::Data(b"hello\xFF\r\n".to_vec()));
    s.rx([255, 250, 44, 1, 0, 1, 194, 0, 255, 240]);
    s.rx([255, 250, 44, 2, 7, 255, 240, 255, 250, 44, 3, 3, 255, 240]);
    s.rx([255, 250, 44, 4, 2, 255, 240]);
    s.rx([255, 250, 44, 5, 3, 255, 240]);
    s.rx([255, 250, 44, 5, 9, 255, 240]);
    s.rx([255, 250, 44, 5, 12, 255, 240]);
    s.rx([255, 250, 44, 5, 5, 255, 240]);
    s.rx([255, 250, 44, 5, 6, 255, 240]);
    s.rx([255, 250, 44, 12, 1, 255, 240]);
    s
}

/// pyserial 3.5's client with `?poll_modem`, from `server_poll.log`: the
/// default settings, DTR, RTS, two purges and a modem poll. The port's CTS
/// is on from the start and DSR rises before the poll.
fn pyserial_poll_session() -> Session {
    let mut s = Session::default();
    s.call(Call::ModemState(ModemState::CTS));
    s.rx([255, 253, 1]);
    s.rx([255, 251, 3, 255, 253, 3, 255, 253, 44, 255, 251, 44]);
    s.rx([255, 250, 44, 1, 0, 0, 37, 128, 255, 240]);
    s.rx([
        255, 250, 44, 2, 8, 255, 240, 255, 250, 44, 3, 1, 255, 240, 255, 250, 44, 4, 1, 255, 240,
    ]);
    s.rx([255, 250, 44, 5, 1, 255, 240]);
    s.rx([255, 250, 44, 5, 8, 255, 240]);
    s.rx([255, 250, 44, 5, 11, 255, 240]);
    s.rx([255, 250, 44, 12, 1, 255, 240]);
    s.rx([255, 250, 44, 12, 2, 255, 240]);
    s.call(Call::ModemState(ModemState::CTS | ModemState::DSR));
    s.rx([255, 250, 44, 7, 255, 240]);
    s
}

/// A client toward a server. Before the client enables COM-PORT-OPTION: a
/// modem state is stored, line states and flow control are refused, its
/// `DO 44` is refused, requests are ignored and data flows. Then the
/// opening, a settings round, every command, a 100-byte client signature,
/// 0xFF masks, a refused option, data with an escaped IAC, and 30 queries
/// and modem polls while the server sends 20 notifications; sredird-style
/// 8-byte rates, inbound flow and unassigned SET-CONTROL values, and
/// requests that get no answer (wrong lengths, a truncated body,
/// notifications, the client's flow control, unknown codes, subnegotiations
/// of other options), refusals with data that fill the output buffer, and
/// a subnegotiation given up after 1024 bytes at an escaped IAC, with data
/// before and after it. Finally a withdrawal with calls and ignored
/// requests, and a re-enable that reports the modem state stored meanwhile.
fn server_session() -> Session {
    let mut s = Session::default();
    s.call(Call::ModemState(ModemState::DSR));
    s.call(Call::LineState(LineState::BREAK_DETECT));
    s.call(Call::Suspend);
    s.rx(hex("FF FD 2C"));
    for request in [
        sb(1, &115_200u32.to_be_bytes()),
        sb(10, &[0xFF]),
        sb(7, &[]),
        sb(0, &[]),
        sb(5, &[8]),
    ] {
        s.rx(request);
    }
    s.rx(b"early");
    s.call(Call::Resume);

    s.rx(hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C"));
    s.rx(requests(115_200, [7, 3, 2, 3]));
    s.call(Call::ModemState(ModemState::DSR | ModemState::CTS));
    for value in [8, 9, 11, 12, 5, 6] {
        s.rx(sb(5, &[value]));
    }
    for purge in [1, 2, 3] {
        s.rx(sb(12, &[purge]));
    }
    s.rx(sb(0, &[]));
    s.rx(sb(0, &[b'c'; 100]));
    s.rx(sb(11, &[0xFF]));
    s.rx(sb(10, &[0xFF]));
    s.rx(hex("FF FB 18"));
    s.rx(hex("61 FF FF 62"));
    s.call(Call::Data(vec![0xFF; 100]));
    let queries = [
        sb(1, &[0; 4]),
        sb(2, &[0]),
        sb(3, &[0]),
        sb(4, &[0]),
        sb(5, &[0]),
        sb(5, &[4]),
        sb(5, &[7]),
        sb(5, &[10]),
        // pyserial's modem state poll.
        sb(7, &[]),
    ];
    for i in 0..30 {
        s.rx(&queries[i % queries.len()]);
        if i < 20 {
            s.call(match i % 4 {
                0 => Call::ModemState(ModemState::from_bits(0x10 | i as u8)),
                1 => Call::LineState(LineState::from_bits(0xF0 | i as u8)),
                2 => Call::Suspend,
                _ => Call::Resume,
            });
        }
    }
    s.rx(sb(1, &hex("00 00 4B 00 00 00 00 00")));
    s.rx(sb(1, &hex("00 00 00 00 00 01 C2 00")));
    for value in [13, 14, 15, 16, 18, 20, 0xFF] {
        s.rx(sb(5, &[value]));
    }
    for request in [
        sb(1, &[1, 2, 3]),
        sb(2, &[7, 7]),
        sb(5, &[]),
        sb(10, &[]),
        sb(11, &[1, 2]),
        sb(12, &[]),
        sb(3, &[1; 70]),
        sb(6, &[1]),
        sb(7, &[1]),
        sb(8, &[]),
        sb(9, &[]),
        sb(13, &[1]),
        sb(99, &[]),
        sb(200, &[5]),
    ] {
        s.rx(request);
    }
    s.rx(hex("FF FA 18 01 FF F0 FF FA FF F0 FF FA 2C FF F0 FF F1"));
    for i in 0..100 {
        s.rx(hex("FF FD 18"));
        s.rx([b'z', i]);
    }
    s.rx(b"<");
    s.rx(hex("FF FA 2C 00"));
    s.rx([b'g'; 1022]);
    s.rx(hex("FF FF 68 FF F0"));
    s.rx(b">");

    s.rx(hex("FF FC 2C"));
    s.call(Call::LineState(LineState::FRAMING_ERROR));
    s.call(Call::Suspend);
    s.call(Call::ModemState(ModemState::CD | ModemState::CTS));
    for request in [sb(5, &[9]), sb(12, &[3]), sb(7, &[])] {
        s.rx(request);
    }
    s.rx(b"between");

    s.rx(hex("FF FB 2C"));
    s.rx(sb(5, &[9]));
    s.rx(sb(7, &[]));
    s.call(Call::LineState(LineState::FRAMING_ERROR));
    s.rx(b"tail");
    s.call(Call::Data(b"bye".to_vec()));
    s
}

fn client() -> Client {
    Client::new(PortSettings::default())
}

fn server() -> Server {
    Server::new(PortSettings::default())
}

// ---- cases: sessions with their checked references --------------------------

/// A session and its reference transcript (whole input, a 4096-byte
/// payload buffer, unlimited writes, full drains).
struct Case<E> {
    name: &'static str,
    session: Session,
    reference: Transcript<E>,
}

fn case<H: Driven>(name: &'static str, new: fn() -> H, session: Session) -> Case<H::Event> {
    let reference = run(new(), &session, REFERENCE, &mut Counts::default());
    Case {
        name,
        session,
        reference,
    }
}

/// The client replays, their references checked against the captures.
fn client_replays() -> Vec<Case<Event>> {
    let ser2net = case("ser2net", client, ser2net_session());
    let r = &ser2net.reference;
    assert!(r.state.starts_with("Active Ok(())"), "{}", r.state);
    assert_eq!(r.payload, b"JULABO HT30-M1 VERSION 4.00\r\n");
    assert_eq!(r.count(|e| *e == Event::Signature(InlineBytes::EMPTY)), 1);
    assert_eq!(r.count(|e| *e == Event::Purged(Purge::Receive)), 1);
    assert_eq!(r.results(Ok(())), 3);

    let sredird = case("sredird", client, sredird_session());
    let r = &sredird.reference;
    assert!(r.state.starts_with("Active Ok(())"), "{}", r.state);
    assert_eq!(r.payload, sredird_data());
    let signature = Event::Signature(InlineBytes::from("SRedird 2.2.2 /dev/ttyV0"));
    assert_eq!(r.count(|e| *e == signature), 1);
    assert_eq!(r.count(|e| *e == Event::Break(true)), 1);
    assert_eq!(r.results(Ok(())), r.calls.len());
    vec![ser2net, sredird]
}

/// The hostile client session, its reference checked for what it is meant
/// to exercise.
fn hostile_client() -> Case<Event> {
    let hostile = case("hostile client", client, client_session());
    let r = &hostile.reference;
    assert!(r.state.starts_with("Active Ok(())"), "{}", r.state);
    assert!(r.output.len() > 2 * Client::OUTPUT_CAPACITY);
    assert!(r.payload.ends_with(b"refused\r\nend\r\n"));
    // The body kept `2C 64` and 1022 bytes; the other 78 are data.
    let given_up = [&b"<"[..], &[b'g'; 78], b">"].concat();
    assert!(r.payload.windows(given_up.len()).any(|w| w == given_up));
    let truncated =
        |e: &Event| matches!(e, Event::Signature(s) | Event::Unknown(s) if s.is_truncated());
    assert_eq!(r.count(truncated), 2);
    assert_eq!(r.count(|e| *e == Event::ComPortActive), 2);
    assert_eq!(r.count(|e| *e == Event::ComPortRefused), 1);
    // ser2net's empty answer, the volunteered one and the text answer; the
    // server's own request is not an event.
    assert_eq!(r.count(|e| matches!(e, Event::Signature(_))), 3);
    assert_eq!(r.count(|e| *e == Event::FlowSuspended), 1);
    assert_eq!(r.count(|e| *e == Event::FlowResumed), 1);
    assert_eq!(r.results(Err(CommandError::Inactive)), 3);
    hostile
}

/// The server replays, their references checked against the captures.
fn server_replays() -> Vec<Case<ServerEvent>> {
    let pyserial = case("pyserial", server, pyserial_session());
    let r = &pyserial.reference;
    let configured = port(
        115_200,
        DataBits::Seven,
        Parity::Even,
        StopBits::Two,
        FlowControl::RtsCts,
    );
    assert_eq!(r.state, format!("{configured:?} active=true"));
    assert_eq!(r.payload, b"hello\xFF\r\n");
    assert!(r.output.ends_with(&sb(112, &[1])));
    assert_eq!(r.count(|e| *e == ServerEvent::ComPortActive), 1);
    assert_eq!(r.count(|e| matches!(e, ServerEvent::SettingsChanged(_))), 6);
    assert_eq!(r.count(|e| *e == ServerEvent::Break(false)), 1);

    let poll = case("pyserial poll", server, pyserial_poll_session());
    let r = &poll.reference;
    assert_eq!(
        r.state,
        format!("{:?} active=true", PortSettings::default())
    );
    assert!(
        r.output
            .ends_with(&[sb(107, &[0x32]), sb(107, &[0x30])].concat())
    );
    vec![pyserial, poll]
}

/// The hostile server session, its reference checked for what it is meant
/// to exercise.
fn hostile_server() -> Case<ServerEvent> {
    let hostile = case("hostile server", server, server_session());
    let r = &hostile.reference;
    let configured = port(
        115_200,
        DataBits::Seven,
        Parity::Even,
        StopBits::Two,
        FlowControl::RtsCts,
    );
    assert_eq!(r.state, format!("{configured:?} active=true"));
    assert!(r.output.len() > 2 * Server::OUTPUT_CAPACITY);
    assert!(r.payload.starts_with(b"early"));
    assert!(r.payload.ends_with(b"betweentail"));
    // The body kept `2C 00` and 1022 bytes; the escaped IAC after them is
    // data.
    assert!(r.payload.windows(4).any(|w| w == b"<\xFFh>"));
    assert_eq!(r.count(|e| *e == ServerEvent::ComPortActive), 2);
    let long =
        |e: &ServerEvent| matches!(e, ServerEvent::ClientSignature(s) if s.original_len() == 100);
    assert_eq!(r.count(long), 1);
    assert_eq!(r.results(Err(CommandError::Inactive)), 5);
    // The re-enable reports the levels stored while withdrawn.
    let reenabled = position(&r.output, &hex("FF FD 2C FF FA 2C 6B 90 FF F0"));
    assert!(reenabled > position(&r.output, &hex("FF FE 2C")));
    hostile
}

// ---- core invariance -------------------------------------------------------

/// Every two-way split of the input, and one byte at a time, against the
/// reference.
fn every_split<H: Driven>(new: fn() -> H, case: &Case<H::Event>) {
    let splits = (0..=case.session.input.len()).map(Split::At);
    for split in splits.chain([Split::Bytes]) {
        let cfg = Config { split, ..REFERENCE };
        let t = run(new(), &case.session, cfg, &mut Counts::default());
        assert!(t == case.reference, "{}: {split:?}", case.name);
    }
}

// The captures never have payload right after an event byte (a call sits
// between them), so the hostile sessions, which do, are split too.

#[test]
fn client_chunk_split_invariance() {
    for case in client_replays().iter().chain([&hostile_client()]) {
        every_split(client, case);
    }
}

#[test]
fn server_chunk_split_invariance() {
    for case in server_replays().iter().chain([&hostile_server()]) {
        every_split(server, case);
    }
}

#[test]
fn random_multi_splits_are_invariant() {
    let (c, s) = (hostile_client(), hostile_server());
    let mut rng = XorShift(0x2217_2217_2217_2217);
    let (mut client_counts, mut server_counts) = (Counts::default(), Counts::default());
    for round in 0..500 {
        let cfg = random_config(&mut rng);
        let t = run(client(), &c.session, cfg, &mut client_counts);
        assert!(t == c.reference, "client, round {round}, {cfg:?}");
        let cfg = random_config(&mut rng);
        let t = run(server(), &s.session, cfg, &mut server_counts);
        assert!(t == s.reference, "server, round {round}, {cfg:?}");
    }
    for counts in [client_counts, server_counts] {
        assert!(counts.stalls > 0 && counts.retries > 0, "{counts:?}");
    }
}

/// A 1-byte payload buffer and 1-byte writes for the whole session, whole
/// or byte-split input, full or lazy drains. With `pushback`, the lazy runs
/// must have stalled and retried.
fn one_byte_buffers<H: Driven>(new: fn() -> H, case: &Case<H::Event>, pushback: bool) {
    for split in [Split::Whole, Split::Bytes] {
        for lazy in [false, true] {
            let cfg = Config {
                split,
                payload: 1,
                write_limit: 1,
                lazy,
                seed: 1,
            };
            let mut counts = Counts::default();
            let t = run(new(), &case.session, cfg, &mut counts);
            assert!(t == case.reference, "{}, {cfg:?}", case.name);
            if lazy && pushback {
                assert!(counts.stalls > 0 && counts.retries > 0, "{counts:?}");
            }
        }
    }
}

#[test]
fn fixed_one_byte_buffers_are_invariant() {
    for case in client_replays() {
        one_byte_buffers(client, &case, false);
    }
    one_byte_buffers(client, &hostile_client(), true);
    for case in server_replays() {
        one_byte_buffers(server, &case, false);
    }
    one_byte_buffers(server, &hostile_server(), true);
}

// ---- convergence, transparency, arbitrary input -----------------------------

#[test]
fn client_server_converge_for_every_setting() {
    use DataBits::*;
    use StopBits::*;
    let mut combinations = 0;
    for baud_rate in [300, 9600, 115_200, 921_600] {
        for data_bits in [Five, Six, Seven, Eight] {
            for parity in [
                Parity::None,
                Parity::Odd,
                Parity::Even,
                Parity::Mark,
                Parity::Space,
            ] {
                for stop_bits in [One, Two, OnePointFive] {
                    for flow_control in [
                        FlowControl::None,
                        FlowControl::XonXoff,
                        FlowControl::RtsCts,
                        FlowControl::Dcd,
                        FlowControl::Dsr,
                    ] {
                        let wanted = port(baud_rate, data_bits, parity, stop_bits, flow_control);
                        let mut c = Wire::new(Client::new(wanted));
                        let mut s = Wire::new(server());
                        let rounds = pump(&mut c, &mut s, 4);
                        assert!(rounds <= 4, "{wanted:?}: {rounds} rounds");
                        assert_eq!(c.readiness(), Ok(()), "{wanted:?}");
                        assert!(c.mismatched_settings().is_empty(), "{wanted:?}");
                        assert_eq!(c.reported_settings().complete(), Some(wanted));
                        assert_eq!(s.settings(), wanted);
                        combinations += 1;
                    }
                }
            }
        }
    }
    assert_eq!(combinations, 4 * 300);
}

/// Write `data` from one half to the other through the core: the wire
/// carries `escape(data)`, and the receiver delivers `data` whether it
/// reads the wire whole or one byte at a time, with no event or reply.
fn transfer<A, B>(from: &mut Wire<A>, to: &mut Wire<B>, data: &[u8])
where
    A: common::Half + Driven,
    B: common::Half + Driven,
{
    let call = Call::Data(data.to_vec());
    let mut wire = Vec::new();
    let mut done = 0;
    while done < data.len() {
        done += from.attempt(&call, done).expect("data is accepted");
        wire.extend(from.out());
    }
    assert_eq!(wire, escape(data));
    assert_eq!(to.feed(&wire), data);
    let bytewise: Vec<u8> = wire.iter().flat_map(|&b| to.feed(&[b])).collect();
    assert_eq!(bytewise, data);
    assert!(to.events().is_empty());
    assert!(to.out().is_empty());
}

#[test]
fn data_is_transparent_both_ways() {
    // Every byte value, runs of one to five 0xFF, and a run longer than
    // the output buffer holds.
    let mut data: Vec<u8> = (0..=255).collect();
    for run in 1..=5 {
        data.extend(std::iter::repeat_n(0xFF, run));
        data.push(run as u8);
    }
    data.extend([0xFF; 300]);

    // BINARY enabled both ways: a client and a server after negotiation.
    let mut c = Wire::new(client());
    let mut s = Wire::new(server());
    let _ = pump(&mut c, &mut s, 4);
    assert_eq!(c.readiness(), Ok(()));
    let _ = (c.events(), s.events());
    transfer(&mut c, &mut s, &data);
    transfer(&mut s, &mut c, &data);

    // No BINARY: a client whose peer refused BINARY and COM-PORT-OPTION
    // (a plain Telnet data channel), and a server that was never answered.
    let mut c = Wire::new(client());
    let _ = c.feed(&hex("FF FE 00 FF FC 00 FF FE 2C FF FC 2C"));
    assert_eq!(c.com_port_state(), ComPortState::Refused);
    let _ = (c.out(), c.events());
    let mut s = Wire::new(server());
    let _ = s.out();
    transfer(&mut c, &mut s, &data);
    transfer(&mut s, &mut c, &data);
}

/// `len` random bytes, built from pieces that reach deep into both halves:
/// random bytes, Telnet command bytes, negotiations of the options the
/// halves care about, and COM-PORT-OPTION subnegotiations of both
/// directions with payloads of the lengths that matter, sometimes cut
/// short.
fn arbitrary(rng: &mut XorShift, len: usize) -> Vec<u8> {
    const TELNET: [u8; 9] = [0xFF, 0xFF, 0xFA, 0xF0, 0xFB, 0xFC, 0xFD, 0xFE, 0x2C];
    const OPTIONS: [u8; 5] = [0, 1, 3, 24, 44];
    let mut input = Vec::new();
    while input.len() < len {
        match rng.below(8) {
            0 | 1 => input.extend((0..1 + rng.below(8)).map(|_| rng.next() as u8)),
            2 => input.extend((0..1 + rng.below(4)).map(|_| TELNET[rng.below(TELNET.len())])),
            3 | 4 => {
                let verb = 0xFB + rng.below(4) as u8;
                input.extend([0xFF, verb, OPTIONS[rng.below(OPTIONS.len())]]);
            }
            _ => {
                let code = match rng.below(4) {
                    0 => rng.below(14) as u8,
                    1 | 2 => 100 + rng.below(14) as u8,
                    _ => rng.next() as u8,
                };
                let payload_len = [0, 1, 1, 4, 8, rng.below(80)][rng.below(6)];
                let payload: Vec<u8> = (0..payload_len).map(|_| rng.next() as u8).collect();
                let mut piece = sb(code, &payload);
                if rng.below(8) == 0 {
                    piece.truncate(rng.below(piece.len()));
                }
                input.extend(piece);
            }
        }
    }
    input.truncate(len);
    input
}

/// `IAC SB COM-PORT-OPTION`, 5000 bytes that never end it, then `"ok"`.
fn unterminated() -> Vec<u8> {
    [hex("FF FA 2C"), vec![b'x'; 5000], b"ok".to_vec()].concat()
}

/// The client's largest receive reserve (BP1): `WILL 44` and a settings
/// round of at most 42 bytes, for a negotiation byte.
const CLIENT_RESERVE: usize = 45;

/// The server's largest receive reserve with `signature` (BP1): the larger of
/// 15 and its signature reply (6 bytes plus the signature, 0xFF doubled), for
/// a subnegotiation byte; an empty signature keeps the default one.
fn server_reserve(signature: &[u8]) -> usize {
    let default = concat!("rfc2217 ", env!("CARGO_PKG_VERSION")).as_bytes();
    let signature = if signature.is_empty() {
        default
    } else {
        signature
    };
    let escaped = signature.len() + signature.iter().filter(|&&b| b == 0xFF).count();
    (6 + escaped).max(15)
}

/// Less room than the replies to a negotiation or subnegotiation byte may
/// need: below the half's largest receive reserve.
fn output_full<H: Driven>(half: &H, reserve: usize) -> bool {
    Client::OUTPUT_CAPACITY - half.pending().len() < reserve
}

/// Feed arbitrary `input` to `half`: first never writing its output nor
/// polling its events, then polling but not writing, then driving it as
/// documented. The first two phases may stop only for the reasons of BP1,
/// and the last processes everything. Then an unterminated subnegotiation
/// is given up and the data after it is delivered. `reserve` is the half's
/// largest receive reserve.
fn hammer<H: Driven>(mut half: H, reserve: usize, input: &[u8]) {
    let mut buf = [0; 16];
    let mut at = 0;
    loop {
        let p = half.step(&input[at..], &mut buf);
        at += p.consumed;
        if p.consumed == 0 {
            break;
        }
    }
    let event = half.next_event().is_some();
    assert!(at == input.len() || event || output_full(&half, reserve));
    loop {
        let p = half.step(&input[at..], &mut buf);
        at += p.consumed;
        while half.next_event().is_some() {}
        if p.consumed == 0 {
            break;
        }
    }
    assert!(at == input.len() || output_full(&half, reserve));
    let _ = drive(&mut half, &input[at..]);

    // Whatever state the input left the parser in, at most 1024 body bytes
    // and the 3 bytes that open the subnegotiation are not data.
    let payload = drive(&mut half, &unterminated());
    assert!(payload.ends_with(b"ok"));
    assert!(payload.len() >= 5000 - 1023 + 2, "{}", payload.len());
}

#[test]
fn arbitrary_input_never_panics_and_sb_memory_is_bounded() {
    let mut rng = XorShift(0xF022_0F02_2F02_20F0);
    for round in 0..200 {
        let input = arbitrary(&mut rng, 4096);
        hammer(client(), CLIENT_RESERVE, &input);
        let signature: &[u8] = if round % 2 == 0 { b"" } else { &[0xFF; 64] };
        let reserve = server_reserve(signature);
        hammer(server().with_signature(signature), reserve, &input);
    }

    // Subnegotiation memory is a fixed array: an unterminated one is given
    // up once its body holds 1024 bytes (option byte included), and every
    // later byte is data.
    let expected = [vec![b'x'; 5000 - 1023], b"ok".to_vec()].concat();
    assert_eq!(drive(&mut client(), &unterminated()), expected);
    assert_eq!(drive(&mut server(), &unterminated()), expected);
}

// ---- the alloc layer --------------------------------------------------------

#[cfg(feature = "alloc")]
impl<E: Clone> Transcript<E> {
    /// The transcript as an alloc-layer run records it: without call
    /// results.
    fn without_calls(&self) -> Self {
        Self {
            calls: Vec::new(),
            ..self.clone()
        }
    }

    /// The transcript with every event at position (0, 0): what remains
    /// comparable when events are polled only after a whole chunk.
    fn without_positions(mut self) -> Self {
        for event in &mut self.events {
            (event.0, event.1) = (0, 0);
        }
        self
    }
}

/// How an alloc-layer run feeds its input.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, Copy)]
enum Receive {
    /// `receive` on every chunk: events are polled after the whole chunk.
    Whole,
    /// `receive_until_event` until the chunk is consumed: events are polled
    /// right after the byte that produced them.
    UntilEvent,
}

/// The alloc layer of a half, as the harness drives it.
#[cfg(feature = "alloc")]
trait Buffered: Driven {
    fn receive_all(&mut self, input: &[u8]) -> Vec<u8>;
    fn until_event(&mut self, input: &[u8], payload: &mut Vec<u8>) -> usize;
    fn take_all(&mut self) -> Vec<u8>;
    fn call(&mut self, call: &Call);
}

#[cfg(feature = "alloc")]
impl Buffered for Client {
    fn receive_all(&mut self, input: &[u8]) -> Vec<u8> {
        self.receive(input)
    }

    fn until_event(&mut self, input: &[u8], payload: &mut Vec<u8>) -> usize {
        self.receive_until_event(input, payload)
    }

    fn take_all(&mut self) -> Vec<u8> {
        self.take_output()
    }

    fn call(&mut self, call: &Call) {
        match call {
            Call::Data(data) => self.write_data(data),
            &Call::Dtr(on) => self.set_dtr(on),
            &Call::Rts(on) => self.set_rts(on),
            &Call::Break(on) => self.set_break(on),
            &Call::Purge(purge) => self.purge(purge),
            Call::Signature => self.request_signature(),
            &Call::Settings(settings) => self.set_port_settings(settings),
            other => panic!("not a client call: {other:?}"),
        }
    }
}

#[cfg(feature = "alloc")]
impl Buffered for Server {
    fn receive_all(&mut self, input: &[u8]) -> Vec<u8> {
        self.receive(input)
    }

    fn until_event(&mut self, input: &[u8], payload: &mut Vec<u8>) -> usize {
        self.receive_until_event(input, payload)
    }

    fn take_all(&mut self) -> Vec<u8> {
        self.take_output()
    }

    fn call(&mut self, call: &Call) {
        match call {
            Call::Data(data) => self.write_data(data),
            &Call::ModemState(state) => self.set_modem_state(state),
            &Call::LineState(state) => self.notify_line_state(state),
            Call::Suspend => self.suspend_flow(),
            Call::Resume => self.resume_flow(),
            other => panic!("not a server call: {other:?}"),
        }
    }
}

/// Run `session` through the alloc layer: `receive` or
/// `receive_until_event` for the input, the infallible calls, and
/// `take_output` after every step, or only at the end when `lazy`. The
/// calls the core refuses are held or dropped here, and the sessions make
/// none while the client negotiates, so the output is the core's.
#[cfg(feature = "alloc")]
fn run_alloc<H: Buffered>(
    mut half: H,
    session: &Session,
    split: Split,
    lazy: bool,
    seed: u64,
    receive: Receive,
) -> Transcript<H::Event> {
    let mut t = Transcript::new();
    let mut rng = XorShift(seed);
    let input = &session.input[..];
    let mut calls = session.calls.iter().peekable();
    let mut at = 0;
    loop {
        while let Some((_, call)) = calls.next_if(|&&(position, _)| position == at) {
            half.call(call);
            if !lazy {
                t.output.extend(half.take_all());
            }
        }
        if at == input.len() {
            break;
        }
        let limit = calls.peek().map_or(input.len(), |&&(position, _)| position);
        let end = (at + split.next(at, limit, &mut rng)).min(limit);
        while at < end {
            match receive {
                Receive::Whole => {
                    t.payload.extend(half.receive_all(&input[at..end]));
                    at = end;
                }
                Receive::UntilEvent => {
                    let n = half.until_event(&input[at..end], &mut t.payload);
                    assert!(n > 0, "receive_until_event consumed nothing");
                    at += n;
                }
            }
            let before = t.events.len();
            while let Some(event) = half.next_event() {
                t.events.push((at, t.payload.len(), event));
            }
            if let Receive::UntilEvent = receive {
                // At most one event, and the rest is consumed only if there
                // was none.
                assert!(t.events.len() <= before + 1);
                assert!(t.events.len() > before || at == end);
            }
            if !lazy {
                t.output.extend(half.take_all());
            }
        }
    }
    t.output.extend(half.take_all());
    t.state = half.state();
    t
}

#[cfg(feature = "alloc")]
fn alloc_matches_core<H: Buffered>(new: fn() -> H, case: &Case<H::Event>, rng: &mut XorShift) {
    let reference = case.reference.without_calls();
    for round in 0..40 {
        let split = [Split::Whole, Split::Bytes, Split::Random][round % 3];
        let lazy = round % 2 == 0;
        let seed = rng.next() | 1;
        let t = run_alloc(new(), &case.session, split, lazy, seed, Receive::UntilEvent);
        assert!(
            t == reference,
            "{}, round {round}, {split:?}, lazy {lazy}",
            case.name
        );
    }
}

#[cfg(feature = "alloc")]
#[test]
fn alloc_layer_matches_core_transcript() {
    let mut rng = XorShift(0x0A11_0C0A_110C);
    for case in client_replays().iter().chain([&hostile_client()]) {
        alloc_matches_core(client, case, &mut rng);
    }
    for case in server_replays().iter().chain([&hostile_server()]) {
        alloc_matches_core(server, case, &mut rng);
    }
}

/// `receive_until_event` stops right after every byte that produced an
/// event, exactly where the core's `receive_into` stops, and a loop over it
/// yields what `receive` yields for the same chunks.
#[cfg(feature = "alloc")]
fn until_event_equals_receive<H: Buffered>(new: fn() -> H, case: &Case<H::Event>) {
    let reference = case.reference.without_calls();
    let mut rng = XorShift(0x5EED_0FE7_E7E7);
    for split in [Split::Whole, Split::Bytes, Split::Random] {
        for lazy in [false, true] {
            let seed = rng.next() | 1;
            let stepped = run_alloc(new(), &case.session, split, lazy, seed, Receive::UntilEvent);
            assert!(stepped == reference, "{}, {split:?}", case.name);
            let whole = run_alloc(new(), &case.session, split, lazy, seed, Receive::Whole);
            assert!(
                whole.without_positions() == stepped.without_positions(),
                "{}, {split:?}, lazy {lazy}",
                case.name
            );
        }
    }
}

#[cfg(feature = "alloc")]
#[test]
fn receive_until_event_equals_receive() {
    for case in client_replays().iter().chain([&hostile_client()]) {
        until_event_equals_receive(client, case);
    }
    for case in server_replays().iter().chain([&hostile_server()]) {
        until_event_equals_receive(server, case);
    }
}
