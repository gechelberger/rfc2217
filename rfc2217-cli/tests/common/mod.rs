//! Peers for the CLI's tests, each on 127.0.0.1:0 in a thread of the test,
//! and a runner for the binary.
//!
//! - [`Bridge`]: an RFC 2217 bridge built on the library's own `Server`,
//!   which acknowledges every request; its "UART" echoes the data back, and
//!   it records what it saw for the test's assertions.
//! - [`Script`]: scripted peers, after the byte sequences of the library's
//!   `tests/*_trace.rs`: a Telnet server that refuses COM-PORT-OPTION, raw
//!   ports that never answer or echo everything, and bridges that answer
//!   with other values or not at all.
//!
//! Every wait has a deadline ([`DEADLINE`]): a broken CLI fails a test, it
//! never hangs the suite.

// Each test crate uses its own part of this module.
#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rfc2217::{PortSettings, Server, ServerEvent};
use socket2::SockRef;

/// No test waits longer than this for anything.
pub(crate) const DEADLINE: Duration = Duration::from_secs(20);

/// The binary under test.
pub(crate) const BIN: &str = env!("CARGO_BIN_EXE_rfc2217");

const IAC: u8 = 0xFF;
const SB: u8 = 0xFA;
const SE: u8 = 0xF0;
const WILL: u8 = 0xFB;
const DO: u8 = 0xFD;
const DONT: u8 = 0xFE;
/// COM-PORT-OPTION.
const COM: u8 = 44;

/// What a [`Bridge`] saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seen {
    Event(ServerEvent),
    /// Serial data from the client (what the UART transmits).
    Data(Vec<u8>),
    /// The bridge sent FLOWCONTROL-RESUME.
    Resumed,
}

/// How a [`Bridge`] behaves.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// The port's settings before the client changes them.
    pub(crate) settings: PortSettings,
    /// The UART's echo comes back this late.
    pub(crate) delay: Duration,
    /// Close the connection once the data received contains this.
    pub(crate) close_on: Option<Vec<u8>>,
    /// Socket buffers of a few KiB, so that a transfer outgrows them many
    /// times over (review finding vc-1).
    pub(crate) small_buffers: bool,
    /// Send FLOWCONTROL-SUSPEND as COM-PORT-OPTION goes active, and
    /// FLOWCONTROL-RESUME this much later.
    pub(crate) suspend_for: Option<Duration>,
    /// Answer the client's opening this late: the client is negotiating
    /// until then.
    pub(crate) answer_after: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            settings: PortSettings {
                baud_rate: 115_200,
                ..PortSettings::default()
            },
            delay: Duration::ZERO,
            close_on: None,
            small_buffers: false,
            suspend_for: None,
            answer_after: Duration::ZERO,
        }
    }
}

/// An RFC 2217 bridge for one connection, with a UART that echoes.
#[derive(Debug)]
pub(crate) struct Bridge {
    pub(crate) addr: SocketAddr,
    rx: Receiver<Note>,
    seen: Vec<(Instant, Seen)>,
    /// The port's settings when the connection ended.
    closed: Option<PortSettings>,
}

#[derive(Debug)]
enum Note {
    Seen(Instant, Seen),
    Closed(PortSettings),
}

/// Everything a [`Bridge`] saw on its connection.
#[derive(Debug)]
pub(crate) struct Report {
    /// The port's settings when the connection ended.
    pub(crate) settings: PortSettings,
    pub(crate) events: Vec<ServerEvent>,
    /// All the serial data received.
    pub(crate) data: Vec<u8>,
    pub(crate) seen: Vec<(Instant, Seen)>,
}

impl Bridge {
    pub(crate) fn echo() -> Self {
        Self::start(Options::default())
    }

    pub(crate) fn start(options: Options) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        if options.small_buffers {
            // Inherited by the accepted socket.
            let socket = SockRef::from(&listener);
            socket.set_recv_buffer_size(4096).expect("SO_RCVBUF");
            socket.set_send_buffer_size(4096).expect("SO_SNDBUF");
        }
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut server = Server::new(options.settings);
            // The connection ends with an error when the CLI resets it.
            let _ = serve(&mut stream, &mut server, &options, &tx);
            let _ = tx.send(Note::Closed(server.settings()));
        });
        Self {
            addr,
            rx,
            seen: Vec::new(),
            closed: None,
        }
    }

    /// The bridge's address, as the CLI takes it.
    pub(crate) fn target(&self) -> String {
        self.addr.to_string()
    }

    /// Waits until the bridge has seen something that `matches`, and
    /// returns when it saw it.
    pub(crate) fn wait_for(&mut self, what: &str, matches: impl Fn(&Seen) -> bool) -> Instant {
        let end = Instant::now() + DEADLINE;
        let mut checked = 0;
        loop {
            if let Some((at, _)) = self.seen[checked..].iter().find(|(_, seen)| matches(seen)) {
                return *at;
            }
            checked = self.seen.len();
            assert!(
                self.closed.is_none(),
                "the connection ended before the bridge saw {what}: {:?}",
                self.seen
            );
            match self
                .rx
                .recv_timeout(end.saturating_duration_since(Instant::now()))
            {
                Ok(note) => self.note(note),
                Err(_) => panic!("the bridge never saw {what}: {:?}", self.seen),
            }
        }
    }

    /// Waits until the serial data received so far contains `want`,
    /// however it was split.
    pub(crate) fn wait_for_data(&mut self, want: &[u8]) {
        let end = Instant::now() + DEADLINE;
        loop {
            let data = self.data();
            if data.windows(want.len()).any(|w| w == want) {
                return;
            }
            assert!(
                self.closed.is_none(),
                "the connection ended before the bridge got {want:?}: {data:?}"
            );
            match self
                .rx
                .recv_timeout(end.saturating_duration_since(Instant::now()))
            {
                Ok(note) => self.note(note),
                Err(_) => panic!("the bridge never got {want:?}: {data:?}"),
            }
        }
    }

    fn data(&self) -> Vec<u8> {
        self.seen
            .iter()
            .filter_map(|(_, seen)| match seen {
                Seen::Data(data) => Some(data.as_slice()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .concat()
    }

    /// Waits until the connection has ended; returns what the bridge saw.
    pub(crate) fn finish(mut self) -> Report {
        let end = Instant::now() + DEADLINE;
        while self.closed.is_none() {
            match self
                .rx
                .recv_timeout(end.saturating_duration_since(Instant::now()))
            {
                Ok(note) => self.note(note),
                Err(_) => panic!("the bridge's connection never ended: {:?}", self.seen),
            }
        }
        let events = self
            .seen
            .iter()
            .filter_map(|(_, seen)| match seen {
                Seen::Event(event) => Some(event.clone()),
                _ => None,
            })
            .collect();
        let data = self.data();
        Report {
            settings: self.closed.unwrap_or_default(),
            events,
            data,
            seen: self.seen,
        }
    }

    fn note(&mut self, note: Note) {
        match note {
            Note::Seen(at, seen) => self.seen.push((at, seen)),
            Note::Closed(settings) => self.closed = Some(settings),
        }
    }
}

/// One connection: a blocking loop, as simple bridges are. It writes all
/// it has before it reads again, so a client that writes before it reads
/// would deadlock against it on a large transfer.
fn serve(
    stream: &mut TcpStream,
    server: &mut Server,
    options: &Options,
    tx: &Sender<Note>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let seen = |seen| {
        let _ = tx.send(Note::Seen(Instant::now(), seen));
    };
    let mut resume_at = None;
    if options.suspend_for.is_some() {
        // Wake up to send the RESUME.
        stream.set_read_timeout(Some(Duration::from_millis(10)))?;
    }
    let mut received = Vec::new();
    let mut buf = vec![0; 16 * 1024];
    thread::sleep(options.answer_after);
    loop {
        stream.write_all(&server.take_output())?;
        if resume_at.is_some_and(|at| Instant::now() >= at) {
            resume_at = None;
            server.resume_flow();
            seen(Seen::Resumed);
            continue;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        // Event by event, so that `seen` keeps the order of the wire: data
        // and commands from one read stay in the order they were sent.
        let mut payload = Vec::new();
        let mut rest = &buf[..n];
        while !rest.is_empty() {
            let start = payload.len();
            let used = server.receive_until_event(rest, &mut payload);
            rest = &rest[used..];
            if payload.len() > start {
                seen(Seen::Data(payload[start..].to_vec()));
            }
            while let Some(event) = server.poll_event() {
                if let (ServerEvent::ComPortActive, Some(after)) = (&event, options.suspend_for) {
                    // Ahead of the replies to the settings, which the client
                    // sends only once it has read this.
                    server.suspend_flow();
                    resume_at = Some(Instant::now() + after);
                }
                seen(Seen::Event(event));
            }
        }
        if payload.is_empty() {
            continue;
        }
        received.extend_from_slice(&payload);
        thread::sleep(options.delay);
        server.write_data(&payload);
        if let Some(close_on) = &options.close_on {
            if received.windows(close_on.len()).any(|w| w == close_on) {
                return stream.write_all(&server.take_output());
            }
        }
    }
}

/// Scripted peers that are not RFC 2217 bridges, or not good ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Script {
    /// A Telnet server without RFC 2217: it sends `login: `, answers the
    /// client's WILL COM-PORT-OPTION with DONT, and echoes the data.
    Refuse,
    /// A raw TCP port in front of a device that never says anything.
    Silent,
    /// A raw TCP port in front of a device that echoes: the client's own
    /// requests come back (review finding vc-2).
    Echo,
    /// A bridge on a port that takes nothing but 8N1, as sredird on a pty:
    /// it answers every setting, data bits with 8 and parity with none, and
    /// echoes the data.
    Mismatch,
    /// A bridge that never answers the stop bits, as ser2net for 1.5; it
    /// echoes the data.
    NoStopBits,
    /// A bridge that answers every setting, then suspends and resumes the
    /// data flow over and over: a notice each time, which soon fills a
    /// stderr that nobody reads.
    FlowFlood,
    /// A server that says why and closes before answering anything, as a
    /// ser2net port another client has.
    Hangup,
}

/// Starts a scripted peer for one connection.
pub(crate) fn scripted(script: Script) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = play(stream, script);
        }
    });
    addr
}

fn play(mut stream: TcpStream, script: Script) -> io::Result<()> {
    match script {
        Script::Refuse => stream.write_all(b"login: ")?,
        Script::Hangup => return stream.write_all(b"Port already in use\r\n"),
        _ => {}
    }
    let mut telnet = Telnet::default();
    let mut buf = [0; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let input = &buf[..n];
        match script {
            Script::Silent => continue,
            Script::Echo => {
                stream.write_all(input)?;
                continue;
            }
            Script::Refuse
            | Script::Mismatch
            | Script::NoStopBits
            | Script::FlowFlood
            | Script::Hangup => {}
        }
        let mut out = Vec::new();
        for &byte in input {
            match telnet.push(byte) {
                Some(Token::Data(data)) => escape_into(&mut out, &[data]),
                Some(Token::Option(WILL, COM)) => {
                    let answer = if script == Script::Refuse { DONT } else { DO };
                    out.extend_from_slice(&[IAC, answer, COM]);
                }
                Some(Token::Sb(body)) => {
                    out.extend(answer(script, &body).unwrap_or_default());
                    // The flow control comes last in the settings round.
                    if script == Script::FlowFlood && body.starts_with(&[COM, 5]) {
                        stream.write_all(&out)?;
                        return flood(&mut stream);
                    }
                }
                _ => {}
            }
        }
        stream.write_all(&out)?;
    }
}

/// FLOWCONTROL-SUSPEND and FLOWCONTROL-RESUME until the client is gone.
fn flood(stream: &mut TcpStream) -> io::Result<()> {
    let toggle = [IAC, SB, COM, 108, IAC, SE, IAC, SB, COM, 109, IAC, SE];
    let toggles = toggle.repeat(256);
    loop {
        stream.write_all(&toggles)?;
    }
}

/// A scripted bridge's reply to one of the five settings.
fn answer(script: Script, body: &[u8]) -> Option<Vec<u8>> {
    let [COM, code @ 1..=5, value @ ..] = body else {
        return None;
    };
    let value = match (script, *code) {
        // The baud rate query: the port runs at 9600.
        (_, 1) if value == [0; 4] => 9600u32.to_be_bytes().to_vec(),
        (Script::Mismatch, 2) => vec![8],
        (Script::Mismatch, 3) => vec![1],
        (Script::NoStopBits, 4) => return None,
        _ => value.to_vec(),
    };
    let mut reply = vec![IAC, SB, COM, code + 100];
    escape_into(&mut reply, &value);
    reply.extend_from_slice(&[IAC, SE]);
    Some(reply)
}

fn escape_into(out: &mut Vec<u8>, bytes: &[u8]) {
    for &byte in bytes {
        if byte == IAC {
            out.push(IAC);
        }
        out.push(byte);
    }
}

/// Splits a Telnet stream into what the scripted peers react to.
#[derive(Debug, Default)]
struct Telnet {
    state: State,
    body: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy)]
enum State {
    #[default]
    Data,
    Iac,
    Option(u8),
    Sb,
    SbIac,
}

#[derive(Debug)]
enum Token {
    Data(u8),
    Option(u8, u8),
    /// A subnegotiation's body, unescaped.
    Sb(Vec<u8>),
}

impl Telnet {
    fn push(&mut self, byte: u8) -> Option<Token> {
        let (state, token) = match (self.state, byte) {
            (State::Data, IAC) => (State::Iac, None),
            (State::Data, _) => (State::Data, Some(Token::Data(byte))),
            (State::Iac, IAC) => (State::Data, Some(Token::Data(IAC))),
            (State::Iac, WILL..=DONT) => (State::Option(byte), None),
            (State::Iac, SB) => {
                self.body.clear();
                (State::Sb, None)
            }
            (State::Iac, _) => (State::Data, None),
            (State::Option(verb), _) => (State::Data, Some(Token::Option(verb, byte))),
            (State::Sb, IAC) => (State::SbIac, None),
            (State::Sb, _) | (State::SbIac, IAC) => {
                self.body.push(byte);
                (State::Sb, None)
            }
            (State::SbIac, _) => (State::Data, Some(Token::Sb(std::mem::take(&mut self.body)))),
        };
        self.state = state;
        token
    }
}

/// An address nobody listens on.
pub(crate) fn closed_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    listener.local_addr().expect("local_addr")
}

/// How a run of the CLI ended, and what it wrote.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
    pub(crate) took: Duration,
}

impl Output {
    pub(crate) fn code(&self) -> Option<i32> {
        self.status.code()
    }
}

/// Runs `rfc2217 args` with `input` on stdin, which is closed after it.
pub(crate) fn run(args: &[&str], input: &[u8]) -> Output {
    run_with(args, input, false)
}

/// Runs `rfc2217 args` with `input` on stdin, which stays open until the
/// CLI has exited.
pub(crate) fn run_open(args: &[&str], input: &[u8]) -> Output {
    run_with(args, input, true)
}

fn run_with(args: &[&str], input: &[u8], keep_open: bool) -> Output {
    let start = Instant::now();
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the CLI");
    // Written from a thread, so that a large input and its echo cross.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.to_vec();
    let writer = thread::spawn(move || {
        // The CLI may exit before it reads it all.
        let _ = stdin.write_all(&input);
        keep_open.then_some(stdin)
    });
    let stdout = read_all(child.stdout.take().expect("piped stdout"));
    let stderr = read_all(child.stderr.take().expect("piped stderr"));
    let status = wait(&mut child);
    drop(writer.join());
    Output {
        status,
        stdout: stdout.join().expect("stdout reader"),
        stderr: String::from_utf8_lossy(&stderr.join().expect("stderr reader")).into_owned(),
        took: start.elapsed(),
    }
}

/// Reads `source` to its end in a thread.
pub(crate) fn read_all(mut source: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = source.read_to_end(&mut bytes);
        bytes
    })
}

/// Waits for `child` to exit; kills it and fails the test after
/// [`DEADLINE`].
pub(crate) fn wait(child: &mut Child) -> ExitStatus {
    let end = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the CLI did not exit within {DEADLINE:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}
