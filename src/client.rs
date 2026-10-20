use core::fmt;

use crate::bytes::InlineBytes;
use crate::io::{CommandError, Io, OUTPUT_CAPACITY, Progress};
use crate::lines::{LineState, ModemState};
use crate::log::{debug, trace};
use crate::negotiation::{ACCEPT_BINARY, ACCEPT_COM_PORT, ACCEPT_SGA, Negotiator, Q, Reply};
use crate::settings::{
    DataBits, FlowControl, Parity, PartialSettings, PortSettings, Purge, Setting, SettingSet,
    StopBits,
};
use crate::telnet::{
    self, Effect, OPT_BINARY, OPT_COM_PORT, OPT_SGA, Parser, SB_KEEP, TelnetEvent, com,
};

/// Where COM-PORT-OPTION stands: the answer to our `WILL COM-PORT-OPTION`
/// ([`Client::com_port_state`]). A closed three-state lifecycle that starts
/// at `Negotiating` and never returns to it, so the enum is exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComPortState {
    /// Our `WILL COM-PORT-OPTION` is unanswered. A bridge's raw TCP port (or
    /// ESP-Link, which 0.1 does not support) stays here forever: give up at
    /// your deadline. Until the settings round has gone out, the core refuses
    /// data and commands ([`CommandError::Negotiating`]; only
    /// [`try_set_port_settings`](Client::try_set_port_settings) succeeds, by
    /// replacing the request) and the `alloc` layer holds them.
    Negotiating,
    /// The peer sent `DO COM-PORT-OPTION`: the settings round has been sent,
    /// and settings and commands can be exchanged. A peer that echoes what it
    /// receives (a loopback, or a raw port in front of an echoing device)
    /// also gets here, by echoing our own `DO`; it never answers a setting,
    /// so each echoed request becomes an [`Event::Unknown`] and
    /// [`readiness`](Client::readiness) stays [`NotReady::Pending`].
    Active,
    /// The peer declined (`DONT COM-PORT-OPTION`) or later withdrew the
    /// option: a Telnet server that does not do RFC 2217. Commands are
    /// refused ([`CommandError::Inactive`]; the `alloc` layer drops them),
    /// data still flows. Can become `Active` again if the peer sends
    /// `DO COM-PORT-OPTION`, with a fresh settings round.
    Refused,
}

/// Why a [`Client`] is not ready: the error of [`Client::readiness`]. Its
/// `Display` is an actionable message, ready to report when a deadline
/// passes. `Refused` and `Mismatched` will not go away by waiting;
/// `Negotiating` and `Pending` may.
///
/// The deadline idiom: fail fast where waiting cannot help, and report the
/// reason when the deadline passes.
///
/// ```
/// use std::io;
/// use std::time::Instant;
///
/// use rfc2217::{Client, NotReady, PortSettings};
///
/// /// Call on every turn of your loop: `Ok(true)` once the bridge has
/// /// confirmed every setting, `Ok(false)` to keep going.
/// fn check(client: &Client, deadline: Instant) -> io::Result<bool> {
///     match client.readiness() {
///         Ok(()) => Ok(true),
///         Err(reason @ (NotReady::Refused | NotReady::Mismatched(_))) => {
///             Err(io::Error::other(reason))
///         }
///         Err(reason) if Instant::now() >= deadline => {
///             Err(io::Error::new(io::ErrorKind::TimedOut, reason))
///         }
///         Err(_) => Ok(false),
///     }
/// }
///
/// // A client whose bridge never answered, at its deadline:
/// let client = Client::new(PortSettings::default());
/// let error = check(&client, Instant::now()).unwrap_err();
/// assert_eq!(error.kind(), io::ErrorKind::TimedOut);
/// assert_eq!(
///     error.to_string(),
///     "no answer to the RFC 2217 COM-PORT-OPTION offer (is this the bridge's raw TCP port?)"
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NotReady {
    /// No answer to our COM-PORT-OPTION offer yet. A bridge's raw TCP port,
    /// a wrong port or ESP-Link never answers: check the port and give up at
    /// your deadline.
    Negotiating,
    /// The peer refused or withdrew COM-PORT-OPTION: a Telnet server that
    /// does not do RFC 2217. Waiting does not help.
    Refused,
    /// The bridge answered these settings with other (or undecodable)
    /// values; [`Client::reported_settings`] shows what it reported. Accept
    /// those values or give up: waiting helps only if the bridge reports
    /// again on its own.
    Mismatched(SettingSet),
    /// These settings have no answer yet. Some bridges never answer some
    /// values (ser2net: 1.5 stop bits, DCD/DSR flow control): wait until
    /// your deadline; a later request for the same setting settles them.
    Pending(SettingSet),
}

impl fmt::Display for NotReady {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Negotiating => f.write_str(
                "no answer to the RFC 2217 COM-PORT-OPTION offer (is this the bridge's raw TCP port?)",
            ),
            Self::Refused => f.write_str(
                "the peer refused RFC 2217 COM-PORT-OPTION (is it a plain Telnet server?)",
            ),
            Self::Mismatched(settings) => {
                write!(f, "the bridge reported different values for: {settings}")
            }
            Self::Pending(settings) => write!(f, "no answer from the bridge yet for: {settings}"),
        }
    }
}

impl core::error::Error for NotReady {}

/// Something the server told us, taken with [`Client::poll_event`].
/// Variants may be added in later releases.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// COM-PORT-OPTION became active: produced on every transition into
    /// [`ComPortState::Active`], the first one and every re-activation.
    /// The settings round follows it in the output.
    ComPortActive,
    /// COM-PORT-OPTION was refused: produced on every transition into
    /// [`ComPortState::Refused`], whether the peer declined the option or
    /// withdrew it. Repeated refusals produce nothing.
    ComPortRefused,
    /// The server's signature: the answer to a signature request, or one it
    /// volunteered. Raw bytes (`IAC IAC` unescaped, never decoded as text),
    /// the first 64 kept with the original length; empty if the server
    /// answered a request with nothing (ser2net does).
    Signature(InlineBytes),
    /// The baud rate the bridge reported: the answer to a request or a
    /// query, or unsolicited. Recorded in
    /// [`reported_settings`](Client::reported_settings).
    BaudRate(u32),
    /// The data bits the bridge reported.
    DataBits(DataBits),
    /// The parity the bridge reported.
    Parity(Parity),
    /// The stop bits the bridge reported.
    StopBits(StopBits),
    /// The flow control the bridge reported. sredird also answers DTR and
    /// RTS changes with its flow control value.
    FlowControl(FlowControl),
    /// The break state the bridge reported (`SET-CONTROL` 5 or 6), usually
    /// the answer to [`try_set_break`](Client::try_set_break). Such replies
    /// may never arrive (ser2net on a pty), and ser2net 4.3.11 answers
    /// BREAK-OFF with BREAK-ON.
    Break(bool),
    /// The DTR state the bridge reported (`SET-CONTROL` 8 or 9), usually the
    /// answer to [`try_set_dtr`](Client::try_set_dtr). Such replies may never
    /// arrive (ser2net on a pty); sredird answers with its flow control
    /// value instead ([`Event::FlowControl`]).
    Dtr(bool),
    /// The RTS state the bridge reported (`SET-CONTROL` 11 or 12), as
    /// [`Dtr`](Event::Dtr).
    Rts(bool),
    /// The bridge's line state (`NOTIFY-LINESTATE`): error and status flags
    /// of its serial port, as sent. Not kept by the client.
    ///
    /// | Bit | Flag |
    /// |---|---|
    /// | `0x80` | [`TIMEOUT_ERROR`](LineState::TIMEOUT_ERROR) |
    /// | `0x40` | [`SHIFT_REGISTER_EMPTY`](LineState::SHIFT_REGISTER_EMPTY) |
    /// | `0x20` | [`HOLDING_REGISTER_EMPTY`](LineState::HOLDING_REGISTER_EMPTY) |
    /// | `0x10` | [`BREAK_DETECT`](LineState::BREAK_DETECT) |
    /// | `0x08` | [`FRAMING_ERROR`](LineState::FRAMING_ERROR) |
    /// | `0x04` | [`PARITY_ERROR`](LineState::PARITY_ERROR) |
    /// | `0x02` | [`OVERRUN_ERROR`](LineState::OVERRUN_ERROR) |
    /// | `0x01` | [`DATA_READY`](LineState::DATA_READY) |
    LineState(LineState),
    /// The bridge's modem state (`NOTIFY-MODEMSTATE`): the levels of its
    /// modem input lines in the high nibble and what changed in the low one,
    /// as sent. Also kept in [`modem_state`](Client::modem_state).
    ///
    /// | Bit | Flag |
    /// |---|---|
    /// | `0x80` | [`CD`](ModemState::CD): carrier detect |
    /// | `0x40` | [`RI`](ModemState::RI): ring indicator |
    /// | `0x20` | [`DSR`](ModemState::DSR): data set ready |
    /// | `0x10` | [`CTS`](ModemState::CTS): clear to send |
    /// | `0x08` | [`DELTA_CD`](ModemState::DELTA_CD) |
    /// | `0x04` | [`TRAILING_EDGE_RI`](ModemState::TRAILING_EDGE_RI) |
    /// | `0x02` | [`DELTA_DSR`](ModemState::DELTA_DSR) |
    /// | `0x01` | [`DELTA_CTS`](ModemState::DELTA_CTS) |
    ModemState(ModemState),
    /// The server purged its buffers.
    Purged(Purge),
    /// The server asked us to stop sending data (`FLOWCONTROL-SUSPEND`
    /// while not suspended; a repeated one produces nothing). Advisory:
    /// data is still written.
    FlowSuspended,
    /// The server allowed us to resume sending data (`FLOWCONTROL-RESUME`
    /// while suspended; a repeated one produces nothing).
    FlowResumed,
    /// A COM-PORT-OPTION subnegotiation the client could not decode: the
    /// body (option byte 44, command code, payload), `IAC IAC` unescaped;
    /// the first 64 bytes kept, and `original_len()` is the body's full
    /// length. A reply to one of the five settings that cannot be decoded
    /// (an unassigned value, a wrong length, ser2net 4.3.11's inbound value
    /// 14 for DCD flow control) still answers the oldest outstanding request
    /// for that setting; once no request is left, the setting reads as
    /// mismatched. The exception is a `SET-CONTROL` reply that is not
    /// exactly one byte long: it cannot be attributed to flow control and
    /// answers nothing. Client-direction codes (below 100) come from a peer
    /// that echoes what it receives.
    Unknown(InlineBytes),
}

/// Requests per setting whose replies are tracked; queuing a ninth drops
/// the oldest.
const QUEUE_LEN: usize = 8;

/// The wire value of a query (only the baud rate has one): any reported
/// value answers it.
const QUERY: u32 = 0;

/// The wire values of one setting's requests that have no answer yet,
/// oldest first, in a ring of [`QUEUE_LEN`] entries.
struct Requests<T> {
    values: [T; QUEUE_LEN],
    /// Index of the oldest request.
    head: u8,
    /// Number of requests.
    len: u8,
}

impl<T: Copy + Default + Into<u32>> Requests<T> {
    fn new() -> Self {
        Self {
            values: [T::default(); QUEUE_LEN],
            head: 0,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The requests, oldest first, as wire values.
    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (0..usize::from(self.len))
            .map(|i| self.values[(usize::from(self.head) + i) % QUEUE_LEN].into())
    }

    /// Queue a request, first dropping the oldest if the queue is full.
    fn push(&mut self, value: T) {
        if usize::from(self.len) == QUEUE_LEN {
            self.drop_oldest(1);
        }
        self.values[(usize::from(self.head) + usize::from(self.len)) % QUEUE_LEN] = value;
        self.len += 1;
    }

    /// Settle the requests a reply answers (R4): one carrying `value` (`None`
    /// if undecodable) answers everything up to and including the first
    /// request for that value or query; any other reply answers the oldest.
    /// A reply with nothing queued is unsolicited and settles nothing.
    fn answer(&mut self, value: Option<u32>) {
        if self.is_empty() {
            return;
        }
        let matched = value.and_then(|v| self.iter().position(|w| w == v || w == QUERY));
        self.drop_oldest(matched.map_or(1, |i| i + 1));
    }

    fn drop_oldest(&mut self, n: usize) {
        debug_assert!(n <= usize::from(self.len), "settling unqueued requests");
        // Both are at most QUEUE_LEN, which fits a u8.
        self.head = ((usize::from(self.head) + n) % QUEUE_LEN) as u8;
        self.len -= n as u8;
    }
}

impl<T: Copy + Default + Into<u32>> fmt::Debug for Requests<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Settings accounting: per setting, the requests the bridge has not
/// answered yet and the last value it reported. Replies are accounted in
/// every state, solicited or not (an unsolicited one only updates the
/// reported value), so readiness is a level that follows what the bridge
/// last said.
#[derive(Debug)]
struct Accounting {
    baud_rate: Requests<u32>,
    data_bits: Requests<u8>,
    parity: Requests<u8>,
    stop_bits: Requests<u8>,
    flow_control: Requests<u8>,
    /// The last decodable value reported for each setting. An undecodable
    /// reply clears it: no rule tells "undecodable" from "not reported".
    reported: PartialSettings,
    /// A settings round has been sent; before that every setting is pending.
    sent: bool,
}

impl Accounting {
    fn new() -> Self {
        Self {
            baud_rate: Requests::new(),
            data_bits: Requests::new(),
            parity: Requests::new(),
            stop_bits: Requests::new(),
            flow_control: Requests::new(),
            reported: PartialSettings::default(),
            sent: false,
        }
    }

    /// A settings round for `s` was written: queue one request per setting.
    fn round_sent(&mut self, s: &PortSettings) {
        self.baud_rate.push(s.baud_rate);
        self.data_bits.push(s.data_bits.code());
        self.parity.push(s.parity.code());
        self.stop_bits.push(s.stop_bits.code());
        self.flow_control.push(s.flow_control.code());
        self.sent = true;
    }

    /// The bridge reported a value for `setting`: `reply` is the decoded
    /// value, or `Event::Unknown` if it could not be decoded. Records it,
    /// then settles the requests it answers.
    fn reply(&mut self, setting: Setting, reply: &Event) {
        let r = &mut self.reported;
        match (setting, reply) {
            (Setting::BaudRate, &Event::BaudRate(v)) => r.baud_rate = Some(v),
            (Setting::DataBits, &Event::DataBits(v)) => r.data_bits = Some(v),
            (Setting::Parity, &Event::Parity(v)) => r.parity = Some(v),
            (Setting::StopBits, &Event::StopBits(v)) => r.stop_bits = Some(v),
            (Setting::FlowControl, &Event::FlowControl(v)) => r.flow_control = Some(v),
            (Setting::BaudRate, _) => r.baud_rate = None,
            (Setting::DataBits, _) => r.data_bits = None,
            (Setting::Parity, _) => r.parity = None,
            (Setting::StopBits, _) => r.stop_bits = None,
            (Setting::FlowControl, _) => r.flow_control = None,
        }
        let value = self.reported.wire(setting);
        match setting {
            Setting::BaudRate => self.baud_rate.answer(value),
            Setting::DataBits => self.data_bits.answer(value),
            Setting::Parity => self.parity.answer(value),
            Setting::StopBits => self.stop_bits.answer(value),
            Setting::FlowControl => self.flow_control.answer(value),
        }
    }

    /// `setting` has requests without an answer.
    fn awaits(&self, setting: Setting) -> bool {
        !match setting {
            Setting::BaudRate => self.baud_rate.is_empty(),
            Setting::DataBits => self.data_bits.is_empty(),
            Setting::Parity => self.parity.is_empty(),
            Setting::StopBits => self.stop_bits.is_empty(),
            Setting::FlowControl => self.flow_control.is_empty(),
        }
    }

    /// No round sent yet, or the latest request for `setting` has no answer.
    fn pending(&self, setting: Setting) -> bool {
        !self.sent || self.awaits(setting)
    }

    /// Answered, but not with the requested value (or any decodable value,
    /// for a query).
    fn mismatched(&self, setting: Setting, requested: &PortSettings) -> bool {
        if self.pending(setting) {
            return false;
        }
        let wanted = requested.wire(setting);
        match self.reported.wire(setting) {
            Some(value) => wanted != QUERY && value != wanted,
            None => true,
        }
    }
}

/// Output of the settings round, worst case: `SB(1, rate)` is 14 bytes when
/// all four rate bytes are 0xFF (doubled), and the four one-byte SETs are 7
/// bytes each (their codes are never 0xFF).
const ROUND_MAX: usize = 14 + 4 * 7;
/// Room a byte that completes a negotiation needs: `WILL 44` when the peer
/// re-enables COM-PORT-OPTION, plus the settings round that follows.
const NEG_RESERVE: usize = 3 + ROUND_MAX;
/// Room a byte that completes a subnegotiation needs: the client answers
/// none.
const SB_RESERVE: usize = 0;
/// The larger of the two receive reserves. Calls leave it free, so that a
/// call never takes the room a receive step needs.
const RX_RESERVE: usize = NEG_RESERVE;

/// RFC 2217 client state machine. One instance per TCP connection.
///
/// [`new`](Self::new) queues the opening and the settings you want; once the
/// bridge accepts COM-PORT-OPTION, the client sends all five settings and
/// tracks the bridge's answers ([`readiness`](Self::readiness)).
///
/// The core API (every configuration, never allocates): feed the bytes read
/// from the socket to [`receive_into`](Self::receive_into), write
/// [`output`](Self::output) to the socket and report what it took with
/// [`consume_output`](Self::consume_output), and take events with
/// [`poll_event`](Self::poll_event). Data and commands are queued with the
/// `try_*` calls. With the `alloc` feature, `receive`, `take_output`,
/// `write_data` and the infallible commands do the same with `Vec`s. The
/// crate's `examples/blocking_client.rs` (alloc layer) and
/// `examples/core_client.rs` (core) are complete command-line clients.
///
/// A complete negotiation with a [`Server`](crate::Server), in memory, through
/// "sockets" that take 5 bytes per write and a 3-byte payload buffer:
///
/// ```
/// use rfc2217::{Client, ComPortState, Parity, PortSettings, Server};
///
/// let wanted = PortSettings { baud_rate: 4800, parity: Parity::Even, ..Default::default() };
/// let mut client = Client::new(wanted);
/// let mut server = Server::new(PortSettings::default());
/// // Bytes on the wire in each direction: written by one half and not yet
/// // consumed by the other. Input that was not consumed stays there.
/// let (mut to_server, mut to_client) = (Vec::new(), Vec::new());
/// // Serial data each half received.
/// let (mut at_server, mut at_client) = (Vec::new(), Vec::new());
///
/// // One pass of a driving loop for `$half`: write up to 5 bytes of its output
/// // to `$tx`, then feed it `$rx` until it consumes nothing more, putting the
/// // payload into `$serial` and polling events after every call.
/// macro_rules! poll {
///     ($half:expr, $rx:expr, $tx:expr, $serial:expr) => {
///         let n = $half.output().len().min(5);
///         $tx.extend_from_slice(&$half.output()[..n]);
///         $half.consume_output(n);
///         loop {
///             let mut payload = [0u8; 3];
///             let p = $half.receive_into(&$rx, &mut payload);
///             $rx.drain(..p.consumed);
///             $serial.extend_from_slice(&payload[..p.payload_len]);
///             while let Some(_event) = $half.poll_event() {}
///             if p.consumed == 0 {
///                 break; // all consumed, or the output must be written first
///             }
///         }
///     };
/// }
///
/// // Run both halves until they go quiet.
/// let busy = |c: &Client, s: &Server, a: &[u8], b: &[u8]| {
///     c.has_output() || s.has_output() || !a.is_empty() || !b.is_empty()
/// };
/// while busy(&client, &server, &to_server, &to_client) {
///     poll!(client, to_client, to_server, at_client);
///     poll!(server, to_server, to_client, at_server);
/// }
/// assert_eq!(client.com_port_state(), ComPortState::Active);
/// assert_eq!(client.readiness(), Ok(()));
/// assert_eq!(client.reported_settings().complete(), Some(wanted));
/// assert_eq!(server.settings(), wanted);
///
/// // Serial data, IAC bytes included, passes through unchanged.
/// assert_eq!(client.try_write_data(&[b'h', b'i', 0xFF]), Ok(3));
/// while busy(&client, &server, &to_server, &to_client) {
///     poll!(client, to_client, to_server, at_client);
///     poll!(server, to_server, to_client, at_server);
/// }
/// assert_eq!(at_server, [b'h', b'i', 0xFF]);
/// assert!(at_client.is_empty());
/// ```
#[derive(Debug)]
pub struct Client {
    parser: Parser,
    negotiator: Negotiator,
    settings: PortSettings,
    accounting: Accounting,
    /// The last NOTIFY-MODEMSTATE, as received.
    modem: Option<ModemState>,
    /// The state the last `ComPortActive`/`ComPortRefused` announced.
    announced: ComPortState,
    /// FLOWCONTROL-SUSPEND received and no RESUME since.
    suspended: bool,
    /// Signature requests written and not answered yet.
    signature_requests: u16,
    pub(crate) io: Io<Event>,
}

/// BINARY, SGA and COM-PORT-OPTION are accepted in both directions.
const ACCEPT: u8 = ACCEPT_BINARY | ACCEPT_SGA | ACCEPT_COM_PORT;

impl Client {
    /// Bytes the fixed output buffer holds (256). 45 of them are kept free
    /// for the replies one input byte can cause (a negotiation reply plus a
    /// settings round), so calls can use at most 211 between drains.
    pub const OUTPUT_CAPACITY: usize = OUTPUT_CAPACITY;

    /// Start a session that will configure the remote port to `settings`.
    ///
    /// The opening (`WILL`/`DO` BINARY, SGA and COM-PORT-OPTION, 18 bytes)
    /// is queued immediately: write [`output`](Self::output) to the socket.
    pub fn new(settings: PortSettings) -> Self {
        let mut client = Self {
            parser: Parser::new(),
            negotiator: Negotiator::new(ACCEPT, ACCEPT),
            settings,
            accounting: Accounting::new(),
            modem: None,
            announced: ComPortState::Negotiating,
            suspended: false,
            signature_requests: 0,
            io: Io::new(),
        };
        for option in [OPT_BINARY, OPT_SGA, OPT_COM_PORT] {
            let request = client.negotiator.request_us(option);
            client.send_reply(request);
            let request = client.negotiator.request_them(option);
            client.send_reply(request);
        }
        client
    }

    // ---- core I/O --------------------------------------------------------

    /// Process bytes read from the socket in order, writing the serial
    /// payload they carry to the start of `payload` (any length of at least
    /// one byte). Returns how many input bytes were processed
    /// ([`Progress::consumed`]; the rest was not looked at) and how many
    /// payload bytes were written ([`Progress::payload_len`]). Empty input
    /// consumes nothing.
    ///
    /// Stops before a byte it cannot complete: an event is waiting for
    /// [`poll_event`](Self::poll_event), the byte is payload and `payload`
    /// is full, or the byte completes a negotiation or subnegotiation and
    /// the output buffer lacks the room its replies may need. Also returns
    /// right after every byte that produced an event (an input byte produces
    /// at most one), so the payload of this call precedes the event and the
    /// payload of later calls follows it. Nothing is dropped.
    ///
    /// # Driving rules
    ///
    /// 1. Pass unconsumed input again, in order: keep `&input[consumed..]`
    ///    in your receive buffer.
    /// 2. After every call, use the payload, then call `poll_event` until it
    ///    returns `None`.
    /// 3. Write out [`output`](Self::output) (reporting each write with
    ///    [`consume_output`](Self::consume_output)) after every receive call
    ///    and every accepted call, or at least whenever a call consumed
    ///    nothing.
    /// 4. When a `try_*` call returns [`CommandError::OutputFull`], or
    ///    [`try_write_data`](Self::try_write_data) takes only part of your
    ///    data, write out output and retry (with the rest) before passing
    ///    more input. This is needed only if the exact interleaving of your
    ///    calls with protocol replies matters: their content, and their
    ///    order among themselves, never depend on it.
    ///
    /// # Guarantees
    ///
    /// For a caller that follows these rules:
    ///
    /// - **Progress.** With the output written out and the events polled, a
    ///   call with non-empty input consumes at least one byte. So
    ///   `consumed == 0` means the input was empty, output is still waiting
    ///   to be written (the socket did not take all of it), or an event was
    ///   not polled.
    /// - **Invariance.** The payload, the events with their positions in
    ///   the payload, every output byte that input produces, and all
    ///   observable state depend only on the bytes received and on the
    ///   input positions at which your own calls were accepted. They never
    ///   depend on how the input was split into calls, on the length of
    ///   `payload`, on how many bytes each socket write took, or on when
    ///   output was written. Your calls appear in the output in the order
    ///   they were accepted.
    /// - **Fixed memory.** Nothing grows with the input. Nothing is
    ///   allocated, except on an instance whose `alloc` layer holds calls
    ///   made while negotiating: the byte that ends the negotiation releases
    ///   them, also here.
    ///
    /// The `alloc` layer's `receive` and `receive_until_event` are loops
    /// over this method and give the same results.
    pub fn receive_into(&mut self, input: &[u8], payload: &mut [u8]) -> Progress {
        let mut consumed = 0;
        let mut payload_len = 0;
        for &byte in input {
            if self.io.event_pending() {
                break;
            }
            let reserve = match self.parser.effect(byte) {
                Effect::Nothing => 0,
                Effect::Payload if payload_len == payload.len() => break,
                Effect::Payload => 0,
                Effect::Negotiation => NEG_RESERVE,
                Effect::Subnegotiation => SB_RESERVE,
            };
            if self.io.out.free() < reserve {
                break;
            }
            consumed += 1;
            let pending = self.io.out.len();
            match self.parser.push(byte) {
                None => {}
                Some(TelnetEvent::Data(d)) => {
                    payload[payload_len] = d;
                    payload_len += 1;
                }
                Some(TelnetEvent::Command(c)) => telnet::ignore_command(c),
                Some(TelnetEvent::Do(o)) => {
                    let reply = self.negotiator.on_do(o);
                    self.negotiated(reply);
                }
                Some(TelnetEvent::Dont(o)) => {
                    let reply = self.negotiator.on_dont(o);
                    self.negotiated(reply);
                }
                Some(TelnetEvent::Will(o)) => {
                    let reply = self.negotiator.on_will(o);
                    self.negotiated(reply);
                }
                Some(TelnetEvent::Wont(o)) => {
                    let reply = self.negotiator.on_wont(o);
                    self.negotiated(reply);
                }
                Some(TelnetEvent::Subnegotiation) => {
                    // Copy the kept bytes out of the parser so that decoding
                    // can update `self`.
                    let mut body = [0; SB_KEEP];
                    let kept = self.parser.body().len();
                    body[..kept].copy_from_slice(self.parser.body());
                    let len = self.parser.body_len();
                    self.subnegotiation(&body[..kept], len);
                }
            }
            debug_assert!(
                self.io.out.len() <= pending + reserve,
                "byte {byte:#04x} wrote more than its reserve of {reserve} bytes"
            );
        }
        Progress {
            consumed,
            payload_len,
        }
    }

    /// The oldest bytes that must be written to the socket: non-empty iff
    /// [`has_output`](Self::has_output). Write any prefix of them and report
    /// it with [`consume_output`](Self::consume_output); the slice may be
    /// shorter than everything pending, so loop until `has_output()` is
    /// false.
    #[must_use = "these bytes must be written to the socket"]
    pub fn output(&self) -> &[u8] {
        self.io.output()
    }

    /// The first `n` bytes of [`output`](Self::output) were written.
    ///
    /// # Panics
    /// If `n > output().len()`.
    pub fn consume_output(&mut self, n: usize) {
        self.io.consume_output(n);
    }

    /// `true` if there are bytes to write to the socket. Calls the `alloc`
    /// layer holds while COM-PORT-OPTION is negotiating are not output yet
    /// and do not count.
    pub fn has_output(&self) -> bool {
        self.io.has_output()
    }

    /// The event the last receive call stopped at, if any (events queued by
    /// the `alloc` layer come first). The core keeps one event: input is not
    /// processed while it is waiting, so call this until it returns `None`
    /// after every receive call.
    ///
    /// Ignoring events loses no state: [`com_port_state`](Self::com_port_state),
    /// [`reported_settings`](Self::reported_settings),
    /// [`modem_state`](Self::modem_state) and
    /// [`flow_suspended`](Self::flow_suspended) keep what they reported.
    #[must_use = "events report what the bridge did"]
    pub fn poll_event(&mut self) -> Option<Event> {
        self.io.poll_event()
    }

    /// Escape and queue the longest prefix of `data` that fits (an escaped
    /// 0xFF is never split); returns how many bytes of `data` were taken, at
    /// least one unless `data` is empty. Write out the output, then pass the
    /// rest again.
    ///
    /// Calls use at most the write budget: the free output space minus the
    /// 45 bytes kept for the replies input may cause, so writing never
    /// starves the protocol. With the output drained, at least 105 bytes are
    /// taken (all of them `0xFF`, escaped to 210). Data flows while Refused (a
    /// plain Telnet data channel), and is not held back by
    /// `FLOWCONTROL-SUSPEND` ([`flow_suspended`](Self::flow_suspended) is
    /// advisory).
    ///
    /// # Errors
    /// - [`CommandError::Negotiating`] while COM-PORT-OPTION is negotiating:
    ///   the settings round must precede all data, so retry after
    ///   [`Event::ComPortActive`]. The core holds nothing back: against a
    ///   peer that never answers COM-PORT-OPTION (a raw TCP port) this keeps
    ///   failing until your deadline.
    /// - [`CommandError::OutputFull`] if not even one byte fits: write out
    ///   output and retry.
    pub fn try_write_data(&mut self, data: &[u8]) -> Result<usize, CommandError> {
        if self.com_port_state() == ComPortState::Negotiating {
            return Err(CommandError::Negotiating);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let budget = self.budget();
        match telnet::put_data(&mut self.io.out, data, budget) {
            0 => Err(CommandError::OutputFull),
            n => Ok(n),
        }
    }

    // ---- core calls: atomic, an error means nothing changed --------------

    /// Change the remote port settings. While COM-PORT-OPTION is not active
    /// this only replaces the request, which is sent once it becomes active,
    /// and always succeeds. While it is active the request is replaced and
    /// all five settings are queued (up to 42 bytes; with the output drained
    /// they always fit); [`readiness`](Self::readiness) then waits for the
    /// bridge to answer them. A baud rate of 0 asks for the bridge's current
    /// rate. To resend the same settings, pass
    /// [`requested_settings`](Self::requested_settings).
    ///
    /// Known limits of the reply accounting: a bridge that drops the reply
    /// to one request and answers a later request for the same setting with
    /// a substituted value leaves that later request pending until your
    /// deadline (a further request settles it); and sredird answers DTR and
    /// RTS changes with its flow control value, so while a flow control
    /// change is outstanding, flow control can read as mismatched until the
    /// bridge's real answer arrives.
    ///
    /// # Errors
    /// [`CommandError::OutputFull`] (only while active) if the five settings
    /// do not fit; the request and the accounting are then unchanged. Write
    /// out output and retry.
    pub fn try_set_port_settings(&mut self, settings: PortSettings) -> Result<(), CommandError> {
        if self.com_port_state() == ComPortState::Active {
            if self.budget() < round_len(&settings) {
                return Err(CommandError::OutputFull);
            }
            self.settings = settings;
            self.send_round();
        } else {
            self.settings = settings;
        }
        Ok(())
    }

    /// Set the DTR line of the remote port. The bridge may answer with
    /// [`Event::Dtr`], but replies may never arrive (ser2net on a pty), and
    /// sredird answers with its flow control value ([`Event::FlowControl`]),
    /// which updates [`reported_settings`](Self::reported_settings): see the
    /// known limits on [`try_set_port_settings`](Self::try_set_port_settings).
    /// `Dtr` replies never count toward readiness. Atomic: an error means
    /// nothing was written.
    ///
    /// # Errors
    /// - [`CommandError::Negotiating`] while COM-PORT-OPTION is negotiating:
    ///   retry after [`Event::ComPortActive`], or give up at your deadline.
    /// - [`CommandError::Inactive`] while Refused: the bridge does not do
    ///   RFC 2217, so no command can be sent.
    /// - [`CommandError::OutputFull`] if the 7-byte command does not fit:
    ///   write out output and retry. With the output drained it always fits.
    pub fn try_set_dtr(&mut self, on: bool) -> Result<(), CommandError> {
        use com::control::{DTR_OFF, DTR_ON};
        self.try_com(com::SET_CONTROL, &[if on { DTR_ON } else { DTR_OFF }])
    }

    /// Set the RTS line of the remote port. Answered, if at all, as
    /// [`try_set_dtr`](Self::try_set_dtr) is ([`Event::Rts`]).
    ///
    /// # Errors
    /// As [`try_set_dtr`](Self::try_set_dtr).
    pub fn try_set_rts(&mut self, on: bool) -> Result<(), CommandError> {
        use com::control::{RTS_OFF, RTS_ON};
        self.try_com(com::SET_CONTROL, &[if on { RTS_ON } else { RTS_OFF }])
    }

    /// Set or clear a break condition on the remote port. Answered, if at
    /// all, as [`try_set_dtr`](Self::try_set_dtr) is ([`Event::Break`]).
    ///
    /// # Errors
    /// As [`try_set_dtr`](Self::try_set_dtr).
    pub fn try_set_break(&mut self, on: bool) -> Result<(), CommandError> {
        use com::control::{BREAK_OFF, BREAK_ON};
        self.try_com(com::SET_CONTROL, &[if on { BREAK_ON } else { BREAK_OFF }])
    }

    /// Ask the bridge to discard buffered data; answered by
    /// [`Event::Purged`].
    ///
    /// # Errors
    /// As [`try_set_dtr`](Self::try_set_dtr).
    pub fn try_purge(&mut self, purge: Purge) -> Result<(), CommandError> {
        self.try_com(com::PURGE_DATA, &[purge.code()])
    }

    /// Ask for the server's signature (an empty SIGNATURE); answered by
    /// [`Event::Signature`]. Each accepted request is counted, so that an
    /// empty answer is reported while an empty SIGNATURE nobody asked for
    /// (the server asking for ours) is not.
    ///
    /// # Errors
    /// As [`try_set_dtr`](Self::try_set_dtr).
    pub fn try_request_signature(&mut self) -> Result<(), CommandError> {
        self.try_com(com::SIGNATURE, &[])?;
        self.signature_requests = self.signature_requests.saturating_add(1);
        Ok(())
    }

    // ---- state -----------------------------------------------------------

    /// The answer to our `WILL COM-PORT-OPTION`: unanswered →
    /// [`Negotiating`](ComPortState::Negotiating), accepted →
    /// [`Active`](ComPortState::Active), declined or withdrawn →
    /// [`Refused`](ComPortState::Refused). The peer's own `WILL` or `WONT`
    /// for the option never changes it.
    pub fn com_port_state(&self) -> ComPortState {
        match self.negotiator.us(OPT_COM_PORT) {
            Q::WantYes => ComPortState::Negotiating,
            Q::Yes => ComPortState::Active,
            Q::No => ComPortState::Refused,
        }
    }

    /// `readiness().is_ok()`: COM-PORT-OPTION is active and the bridge has
    /// answered the latest request for every setting with exactly the
    /// requested value (a baud rate query with any rate). It may never become
    /// true (a raw TCP port, a bridge that skips a reply), so apply your own
    /// deadline. It gates nothing: data and commands behave the same either
    /// way.
    pub fn is_ready(&self) -> bool {
        self.readiness().is_ok()
    }

    /// Why the client is not ready, checked in this order:
    /// [`NotReady::Negotiating`], [`NotReady::Refused`],
    /// [`NotReady::Mismatched`] (some settings were answered with other
    /// values), [`NotReady::Pending`] (some have no answer yet).
    ///
    /// Readiness is a level: an unsolicited report of another value turns a
    /// confirmed setting into a mismatched one, and back. sredird answers DTR
    /// and RTS changes with its flow control value, so after a flow control
    /// change made at run time, flow control can read as mismatched until
    /// the bridge's real answer arrives. Readiness may never come: see
    /// [`NotReady`] for the deadline idiom.
    ///
    /// # Errors
    /// The reason, as above; its `Display` is a ready-made diagnostic.
    pub fn readiness(&self) -> Result<(), NotReady> {
        match self.com_port_state() {
            ComPortState::Negotiating => Err(NotReady::Negotiating),
            ComPortState::Refused => Err(NotReady::Refused),
            ComPortState::Active => {
                let mismatched = self.mismatched_settings();
                let pending = self.pending_settings();
                if !mismatched.is_empty() {
                    Err(NotReady::Mismatched(mismatched))
                } else if !pending.is_empty() {
                    Err(NotReady::Pending(pending))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Settings whose latest request has no answer yet: all five before the
    /// settings round is sent.
    pub fn pending_settings(&self) -> SettingSet {
        Setting::ALL
            .into_iter()
            .filter(|&setting| self.accounting.pending(setting))
            .collect()
    }

    /// Settings whose latest request was answered with a different value, or
    /// one that could not be decoded. A baud rate query (0) is answered by
    /// any decodable rate.
    pub fn mismatched_settings(&self) -> SettingSet {
        Setting::ALL
            .into_iter()
            .filter(|&setting| self.accounting.mismatched(setting, &self.settings))
            .collect()
    }

    /// The settings last passed to `new` or to `try_set_port_settings` (or
    /// `set_port_settings`) and accepted.
    pub fn requested_settings(&self) -> PortSettings {
        self.settings
    }

    /// The last decodable value the bridge reported for each setting, whether
    /// it answered a request or a query or was unsolicited. An undecodable
    /// reply clears that setting's value. Cleared when COM-PORT-OPTION is
    /// refused or withdrawn.
    pub fn reported_settings(&self) -> PartialSettings {
        self.accounting.reported
    }

    /// The last modem state the bridge sent (`NOTIFY-MODEMSTATE`), as sent:
    /// delta bits included. `None` before the first one, and again once
    /// COM-PORT-OPTION is refused or withdrawn.
    pub fn modem_state(&self) -> Option<ModemState> {
        self.modem
    }

    /// The server sent `FLOWCONTROL-SUSPEND` and no `FLOWCONTROL-RESUME`
    /// since. Advisory: data writes neither refuse nor buffer because of
    /// it. Cleared when COM-PORT-OPTION is refused or withdrawn.
    pub fn flow_suspended(&self) -> bool {
        self.suspended
    }

    // ---- internals -------------------------------------------------------

    /// Output room for calls: everything but the receive reserve.
    fn budget(&self) -> usize {
        self.io.out.free().saturating_sub(RX_RESERVE)
    }

    /// Queue one COM-PORT-OPTION command, if the state and the room allow.
    fn try_com(&mut self, command: u8, payload: &[u8]) -> Result<(), CommandError> {
        match self.com_port_state() {
            ComPortState::Negotiating => Err(CommandError::Negotiating),
            ComPortState::Refused => Err(CommandError::Inactive),
            ComPortState::Active if self.budget() < telnet::com_len(payload) => {
                Err(CommandError::OutputFull)
            }
            ComPortState::Active => {
                telnet::put_com(&mut self.io.out, command, payload);
                Ok(())
            }
        }
    }

    fn send_reply(&mut self, reply: Option<Reply>) {
        if let Some(reply) = reply {
            telnet::put_neg(&mut self.io.out, reply);
        }
    }

    /// After every negotiation command: queue its reply, then, if
    /// [`com_port_state`](Self::com_port_state) differs from the state last
    /// announced, announce the new one and run its transition. Entering
    /// Active: the event, the settings round, then the calls the `alloc`
    /// layer held while negotiating. Entering Refused (declined or
    /// withdrawn): reset everything learned from the bridge, write the data
    /// the `alloc` layer held (its held commands are dropped), then the
    /// event. Repeated refusals change nothing, so they announce nothing.
    fn negotiated(&mut self, reply: Option<Reply>) {
        // The reply precedes the round: `WILL` when the peer re-enables
        // the option.
        self.send_reply(reply);
        let state = self.com_port_state();
        if state == self.announced {
            return;
        }
        self.announced = state;
        match state {
            ComPortState::Active => {
                self.io.put_event(Event::ComPortActive);
                // Always fits: it is part of the reserve of the byte that
                // completed the negotiation.
                self.send_round();
                #[cfg(feature = "alloc")]
                self.release_held();
            }
            ComPortState::Refused => {
                self.accounting = Accounting::new();
                self.modem = None;
                self.suspended = false;
                self.signature_requests = 0;
                #[cfg(feature = "alloc")]
                self.release_held();
                self.io.put_event(Event::ComPortRefused);
            }
            // Never re-entered: only `new` asks for the option.
            ComPortState::Negotiating => {}
        }
    }

    /// Write the settings round (all five SETs, baud rate first) and queue
    /// each request for its reply. Callers check the room first.
    fn send_round(&mut self) {
        let s = self.settings;
        let out = &mut self.io.out;
        telnet::put_com(out, com::SET_BAUDRATE, &s.baud_rate.to_be_bytes());
        telnet::put_com(out, com::SET_DATASIZE, &[s.data_bits.code()]);
        telnet::put_com(out, com::SET_PARITY, &[s.parity.code()]);
        telnet::put_com(out, com::SET_STOPSIZE, &[s.stop_bits.code()]);
        telnet::put_com(out, com::SET_CONTROL, &[s.flow_control.code()]);
        self.accounting.round_sent(&s);
    }

    /// Decode a completed subnegotiation: `sb` holds its kept bytes (option
    /// first) and `len` its true length, which exceeds `sb.len()` only for a
    /// body longer than the parser keeps. Such a body never decodes as a
    /// value: only SIGNATURE and `Unknown` carry it, truncated.
    fn subnegotiation(&mut self, sb: &[u8], len: usize) {
        let [OPT_COM_PORT, code, payload @ ..] = sb else {
            debug!("ignoring a subnegotiation without a COM-PORT-OPTION command: {sb:?}");
            return;
        };
        let unknown = || Event::Unknown(InlineBytes::from_prefix(sb, len));
        // The payload, if it is complete: every rule with a fixed length
        // checks the true length.
        let exact = (len == sb.len()).then_some(payload);
        let event = match code.checked_sub(com::SERVER_OFFSET) {
            Some(com::SIGNATURE) => {
                let text = InlineBytes::from_prefix(payload, len - 2);
                if self.signature_requests > 0 {
                    // The answer to a request, even if empty.
                    self.signature_requests -= 1;
                } else if text.is_empty() {
                    debug!("ignoring the server's request for our signature (not answered)");
                    return;
                }
                Event::Signature(text)
            }
            Some(com::SET_BAUDRATE) => {
                let rate = exact.and_then(decode_baud);
                let event = rate.map_or_else(unknown, Event::BaudRate);
                self.setting_reply(Setting::BaudRate, event)
            }
            Some(com::SET_DATASIZE) => {
                let bits = single(exact).and_then(DataBits::from_code);
                let event = bits.map_or_else(unknown, Event::DataBits);
                self.setting_reply(Setting::DataBits, event)
            }
            Some(com::SET_PARITY) => {
                let parity = single(exact).and_then(Parity::from_code);
                let event = parity.map_or_else(unknown, Event::Parity);
                self.setting_reply(Setting::Parity, event)
            }
            Some(com::SET_STOPSIZE) => {
                let bits = single(exact).and_then(StopBits::from_code);
                let event = bits.map_or_else(unknown, Event::StopBits);
                self.setting_reply(Setting::StopBits, event)
            }
            Some(com::SET_CONTROL) => match single(exact) {
                Some(value) => self.control_reply(value, unknown),
                // Not one value byte: never accounted.
                None => unknown(),
            },
            Some(com::NOTIFY_LINESTATE) => match single(exact) {
                Some(v) => Event::LineState(LineState::from_bits(v)),
                None => unknown(),
            },
            Some(com::NOTIFY_MODEMSTATE) => match single(exact) {
                Some(v) => {
                    let state = ModemState::from_bits(v);
                    self.modem = Some(state);
                    Event::ModemState(state)
                }
                None => unknown(),
            },
            // Edge-triggered: only a change of the flow state is reported.
            Some(com::FLOWCONTROL_SUSPEND) => {
                if self.suspended {
                    return;
                }
                self.suspended = true;
                Event::FlowSuspended
            }
            Some(com::FLOWCONTROL_RESUME) => {
                if !self.suspended {
                    return;
                }
                self.suspended = false;
                Event::FlowResumed
            }
            // Mask acknowledgements (ser2net sends one unsolicited).
            Some(com::SET_LINESTATE_MASK | com::SET_MODEMSTATE_MASK) => return,
            Some(com::PURGE_DATA) => single(exact)
                .and_then(Purge::from_code)
                .map_or_else(unknown, Event::Purged),
            Some(_) => unknown(),
            // A client-direction command (code below 100): the peer echoes
            // what we send, or is another client.
            None => {
                debug!("peer sent a client-direction COM-PORT command: is it echoing? {sb:?}");
                unknown()
            }
        };
        trace!("rfc2217 event {event:?}");
        self.io.put_event(event);
    }

    /// A reply to one of the five settings, decodable or not (`event`):
    /// account it, then report it.
    fn setting_reply(&mut self, setting: Setting, event: Event) -> Event {
        self.accounting.reply(setting, &event);
        event
    }

    /// A SET-CONTROL reply carrying `value`, classified by the value alone:
    /// replies are never matched to DTR, RTS or BREAK requests.
    fn control_reply(&mut self, value: u8, unknown: impl FnOnce() -> Event) -> Event {
        use com::control::{BREAK_OFF, BREAK_ON, DTR_OFF, DTR_ON, RTS_OFF, RTS_ON};
        match value {
            BREAK_ON | BREAK_OFF => Event::Break(value == BREAK_ON),
            DTR_ON | DTR_OFF => Event::Dtr(value == DTR_ON),
            RTS_ON | RTS_OFF => Event::Rts(value == RTS_ON),
            value => match FlowControl::from_code(value) {
                Some(flow) => self.setting_reply(Setting::FlowControl, Event::FlowControl(flow)),
                // An inbound flow value, a query or an unassigned value: the
                // answer to an outstanding flow control request, if there is
                // one (ser2net 4.3.11 answers DCD with 14).
                None if self.accounting.awaits(Setting::FlowControl) => {
                    self.setting_reply(Setting::FlowControl, unknown())
                }
                None => unknown(),
            },
        }
    }
}

/// Bytes the settings round for `settings` takes on the wire.
fn round_len(settings: &PortSettings) -> usize {
    telnet::com_len(&settings.baud_rate.to_be_bytes()) + 4 * 7
}

/// The single byte of a complete one-byte payload.
fn single(payload: Option<&[u8]>) -> Option<u8> {
    match payload {
        Some(&[value]) => Some(value),
        _ => None,
    }
}

/// A SET-BAUDRATE reply's payload as a rate: 4 bytes big-endian, or
/// sredird's 8 (an `unsigned long` through `htonl` on an LP64 host: the rate
/// then four zero bytes on little-endian hosts, four zero bytes then the
/// rate on big-endian ones). Any other payload, and the rate 0, are
/// undecodable.
fn decode_baud(payload: &[u8]) -> Option<u32> {
    let rate = match *payload {
        [a, b, c, d] | [a, b, c, d, 0, 0, 0, 0] | [0, 0, 0, 0, a, b, c, d] => {
            u32::from_be_bytes([a, b, c, d])
        }
        _ => return None,
    };
    (rate != 0).then_some(rate)
}
