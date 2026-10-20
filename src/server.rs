use crate::bytes::InlineBytes;
use crate::io::{CommandError, Io, OUTPUT_CAPACITY, Progress};
use crate::lines::{LineState, ModemState};
use crate::log::{debug, trace};
use crate::negotiation::{ACCEPT_BINARY, ACCEPT_COM_PORT, ACCEPT_SGA, Negotiator, Q, Reply};
use crate::settings::{DataBits, FlowControl, Parity, PortSettings, Purge, StopBits};
use crate::telnet::{
    self, Effect, OPT_BINARY, OPT_COM_PORT, OPT_SGA, Parser, SB_KEEP, TelnetEvent, com,
};

/// Something a client asked the server to do, taken with
/// [`Server::poll_event`]. Variants may be added in later releases.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerEvent {
    /// The client enabled COM-PORT-OPTION: produced on every transition
    /// into [`com_port_active`](Server::com_port_active), the first one and
    /// every re-enable. A withdrawal produces no event.
    ComPortActive,
    /// One request changed one of the five settings (baud rate, data bits,
    /// parity, stop bits, flow control); carries all five, as
    /// [`settings`](Server::settings) now returns them. One event per
    /// request that changed a setting, never coalesced: a client that
    /// reconfigures all five settings causes up to five events, each with
    /// the settings applied so far. DTR, RTS and BREAK requests produce
    /// their own events instead.
    SettingsChanged(PortSettings),
    /// The client asked to purge buffers.
    Purge(Purge),
    /// The client set DTR (also to the state it already had).
    Dtr(bool),
    /// The client set RTS (also to the state it already had).
    Rts(bool),
    /// The client set or cleared break (also to the state it already had).
    Break(bool),
    /// The client sent its signature: raw bytes (`IAC IAC` unescaped, never
    /// decoded as text), the first 64 kept with the original length.
    ClientSignature(InlineBytes),
}

/// Room a byte that completes a negotiation needs: the reply, plus the
/// NOTIFY-MODEMSTATE snapshot (7 bytes) that activation will send.
const NEG_RESERVE: usize = 10;
/// Room a byte that completes a subnegotiation needs, signature replies
/// aside: a SET-MODEMSTATE-MASK echo of 0xFF (8 bytes) plus the snapshot that
/// follows it (7 bytes). Every other reply is shorter.
const SB_RESERVE_MIN: usize = 15;

/// The level bits of a modem state (CD, RI, DSR, CTS). The low four bits
/// report changes.
const LEVELS: u8 = 0xF0;

/// The subnegotiation reserve for `signature`: `max(15, com_len(signature))`,
/// 19 for the default signature and at most 134 (64 bytes of 0xFF).
fn sb_reserve(signature: &InlineBytes) -> u8 {
    let reserve = SB_RESERVE_MIN.max(telnet::com_len(signature));
    // At most 6 + 2 * InlineBytes::CAPACITY = 134.
    reserve as u8
}

/// RFC 2217 server (bridge side) state machine. One instance per TCP
/// connection.
///
/// [`new`](Self::new) queues the opening: the server offers BINARY and SGA
/// in both directions. COM-PORT-OPTION is the client's to offer and to
/// perform: the server accepts the client's `WILL COM-PORT-OPTION` with
/// `DO`, and refuses `DO COM-PORT-OPTION` with `WONT`. COM-PORT-OPTION
/// requests are ignored until the client's `WILL` is accepted
/// ([`com_port_active`](Self::com_port_active)).
///
/// Every request is acknowledged automatically, at the byte that completes
/// it: a value is applied to the port model ([`settings`](Self::settings),
/// DTR, RTS, break) and answered with the value now in effect. A value
/// RFC 2217 does not define changes nothing and is answered with the value
/// in effect, except that a `PURGE-DATA` value is always echoed as
/// received. Inbound flow control requests (`SET-CONTROL` 13 to 16 and 18),
/// which RFC 2217 defines but the server does not apply, are answered with
/// the inbound value the outbound setting implies. A request of the wrong
/// length is not answered at all. This suits simulators, tests, and bridges
/// whose port accepts every value RFC 2217 can encode: the application
/// cannot refuse a value in 0.1.
///
/// Each request that changed one of the five settings (baud rate, data
/// bits, parity, stop bits, flow control) produces one
/// [`ServerEvent::SettingsChanged`], never coalesced. A DTR, RTS or BREAK
/// request produces [`Dtr`](ServerEvent::Dtr), [`Rts`](ServerEvent::Rts) or
/// [`Break`](ServerEvent::Break) every time, also when the state is
/// unchanged, and never `SettingsChanged`; a mask change produces no event.
/// The server has no accessor for DTR, RTS and break: they reach the
/// application only as these events, which a bridge must apply to its port.
///
/// The application supplies the port's modem lines with
/// [`try_set_modem_state`](Self::try_set_modem_state): the server reports
/// their changes (`NOTIFY-MODEMSTATE`, with the change bits computed) through
/// the client's modem-state mask, sends the current levels when the client
/// enables COM-PORT-OPTION, changes that mask or polls, and never invents a
/// state. Line-state notifications go through the client's line-state mask,
/// which lets nothing through until the client sets it. Nothing but
/// negotiation and data is sent before the client enables COM-PORT-OPTION.
///
/// Driven like the [`Client`](crate::Client): feed input to
/// [`receive_into`](Self::receive_into), write [`output`](Self::output), take
/// events with [`poll_event`](Self::poll_event). `receive_into` returns right
/// after each byte that produced an event, so a bridge applies a baud rate
/// change, a purge or a break to its UART at its exact position in the data.
/// The crate's `examples/sim_server.rs`
/// ([source](https://docs.rs/crate/rfc2217/latest/source/examples/sim_server.rs))
/// is a complete simulated bridge; [`Client`](crate::Client) shows an
/// in-memory exchange between the two halves.
///
/// A simulator's view of a client that connects, enables COM-PORT-OPTION,
/// asks for 115200 baud and sends "hi":
///
/// ```
/// use rfc2217::{ModemState, PortSettings, Server, ServerEvent};
///
/// let mut server = Server::new(PortSettings::default()).with_signature("bench-sim 1.0");
/// // The port's modem lines: stored, and reported once the client enables
/// // COM-PORT-OPTION.
/// assert_eq!(server.try_set_modem_state(ModemState::CTS | ModemState::DSR), Ok(()));
/// // The opening goes out as soon as the client connects.
/// assert_eq!(server.output().len(), 12);
/// server.consume_output(12);
///
/// let input = [
///     0xFF, 0xFB, 0x2C, // WILL COM-PORT-OPTION
///     0xFF, 0xFA, 0x2C, 0x01, 0x00, 0x01, 0xC2, 0x00, 0xFF, 0xF0, // SET-BAUDRATE 115200
///     b'h', b'i',
/// ];
/// let (mut at, mut to_uart, mut events) = (0, Vec::new(), Vec::new());
/// while at < input.len() {
///     let mut payload = [0u8; 16];
///     let p = server.receive_into(&input[at..], &mut payload);
///     at += p.consumed;
///     to_uart.extend_from_slice(&payload[..p.payload_len]); // the bytes before an event...
///     while let Some(event) = server.poll_event() {
///         events.push(event); // ...then the event, applied at its position
///     }
/// }
/// let wanted = PortSettings { baud_rate: 115_200, ..PortSettings::default() };
/// assert_eq!(events, [ServerEvent::ComPortActive, ServerEvent::SettingsChanged(wanted)]);
/// assert_eq!(to_uart, b"hi");
/// // The client's WILL accepted, the modem state supplied (CTS and DSR), then
/// // the acknowledgement.
/// assert_eq!(
///     server.output(),
///     [
///         0xFF, 0xFD, 0x2C, // DO COM-PORT-OPTION
///         0xFF, 0xFA, 0x2C, 0x6B, 0x30, 0xFF, 0xF0, // NOTIFY-MODEMSTATE
///         0xFF, 0xFA, 0x2C, 0x65, 0x00, 0x01, 0xC2, 0x00, 0xFF, 0xF0, // SET-BAUDRATE reply
///     ]
/// );
/// ```
#[derive(Debug)]
pub struct Server {
    parser: Parser,
    negotiator: Negotiator,
    settings: PortSettings,
    signature: InlineBytes,
    /// See [`sb_reserve`].
    sb_reserve: u8,
    dtr: bool,
    rts: bool,
    brk: bool,
    /// SET-LINESTATE-MASK: the line-state bits the client wants reported
    /// (RFC default 0: none).
    line_mask: u8,
    /// SET-MODEMSTATE-MASK: the modem-state bits the client wants reported
    /// (RFC default 0xFF: all).
    modem_mask: u8,
    /// The modem line levels the application last supplied (the [`LEVELS`]
    /// bits only); `None` until it supplies some.
    levels: Option<u8>,
    /// [`com_port_active`](Self::com_port_active) after the last negotiation
    /// command: each change to `true` is announced.
    was_active: bool,
    pub(crate) io: Io<ServerEvent>,
}

/// Options the server performs when the client asks (`DO`): BINARY and SGA.
/// COM-PORT-OPTION is the client's to perform.
const ACCEPT_US: u8 = ACCEPT_BINARY | ACCEPT_SGA;
/// Options the server lets the client perform (`WILL`): BINARY, SGA and
/// COM-PORT-OPTION.
const ACCEPT_THEM: u8 = ACCEPT_BINARY | ACCEPT_SGA | ACCEPT_COM_PORT;

impl Server {
    /// Bytes the fixed output buffer holds (256). `max(15, com_len(signature))`
    /// of them (19 with the default signature, at most 134) are kept free for
    /// the replies one input byte can cause, where `com_len(signature)` is
    /// 6 bytes plus the signature with every 0xFF doubled. Calls use the rest.
    pub const OUTPUT_CAPACITY: usize = OUTPUT_CAPACITY;

    /// A server whose port currently has `settings`, with DTR and RTS on and
    /// break off, the RFC 2217 default masks (line state 0x00, modem state
    /// 0xFF) and no modem state until the application supplies one.
    /// `settings.baud_rate` should be the port's real rate: a client's baud
    /// query (rate 0) is answered with it, and clients, this crate's
    /// included, cannot decode a reported rate of 0.
    ///
    /// The opening (`WILL`/`DO` BINARY and SGA; 12 bytes) is queued
    /// immediately: write [`output`](Self::output) to the socket once the
    /// client is connected. It does not ask for COM-PORT-OPTION: the client
    /// offers it (`WILL`) and the server accepts (`DO`).
    pub fn new(settings: PortSettings) -> Self {
        let signature = InlineBytes::from(concat!("rfc2217 ", env!("CARGO_PKG_VERSION")));
        let mut server = Self {
            parser: Parser::new(),
            negotiator: Negotiator::new(ACCEPT_US, ACCEPT_THEM),
            settings,
            sb_reserve: sb_reserve(&signature),
            signature,
            dtr: true,
            rts: true,
            brk: false,
            line_mask: 0x00,
            modem_mask: 0xFF,
            levels: None,
            was_active: false,
            io: Io::new(),
        };
        // COM-PORT-OPTION is left to the client: its `WILL` is answered with
        // `DO`. Asking for it here (`DO`) trips a race in pyserial 3.5's
        // client, which then never sends its `WILL`.
        for option in [OPT_BINARY, OPT_SGA] {
            let request = server.negotiator.request_us(option);
            server.send_reply(request);
            let request = server.negotiator.request_them(option);
            server.send_reply(request);
        }
        server
    }

    /// Reply for SIGNATURE requests: the first 64 bytes of `signature` are
    /// kept (a debug log notes a longer one). An empty value is ignored, so
    /// the server never sends an empty signature: the default,
    /// `"rfc2217 <version>"`, stays unless another signature was set. Safe
    /// to call at any time: replies already queued are bytes in the output
    /// buffer and do not change. The room kept free for replies grows with
    /// the signature (to at most 134 bytes for 64 bytes of 0xFF).
    #[must_use]
    pub fn with_signature(mut self, signature: impl AsRef<[u8]>) -> Self {
        let signature = signature.as_ref();
        if signature.is_empty() {
            return self;
        }
        if signature.len() > InlineBytes::CAPACITY {
            debug!(
                "server signature is {} bytes long: keeping the first {}",
                signature.len(),
                InlineBytes::CAPACITY
            );
        }
        self.signature = InlineBytes::new(signature);
        self.sb_reserve = sb_reserve(&self.signature);
        self
    }

    /// The port model: the settings passed to [`new`](Self::new), with every
    /// value the client has set since.
    pub fn settings(&self) -> PortSettings {
        self.settings
    }

    /// The client has enabled COM-PORT-OPTION: its `WILL COM-PORT-OPTION`
    /// was accepted. Requests are answered only while this is true; the
    /// client can withdraw the option (`WONT`) and enable it again.
    pub fn com_port_active(&self) -> bool {
        self.negotiator.them(OPT_COM_PORT) == Q::Yes
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
    /// is full, or the byte completes a negotiation or subnegotiation and the
    /// output buffer lacks the room its replies may need. Also returns right
    /// after every byte that produced an event (an input byte produces at
    /// most one), so the payload of this call precedes the event and the
    /// payload of later calls follows it: a bridge applies
    /// `SettingsChanged`, `Purge`, `Break`, `Dtr` and `Rts` to its UART at
    /// their exact position in the data. Nothing is dropped.
    ///
    /// # Driving rules
    ///
    /// The same as for [`Client::receive_into`](crate::Client::receive_into):
    ///
    /// 1. Pass unconsumed input again, in order.
    /// 2. After every call, use the payload, then call `poll_event` until it
    ///    returns `None`.
    /// 3. Write out [`output`](Self::output) (reporting each write with
    ///    [`consume_output`](Self::consume_output)) after every receive call
    ///    and every accepted call, or at least whenever a call consumed
    ///    nothing.
    /// 4. When a `try_*` call returns [`CommandError::OutputFull`], or
    ///    [`try_write_data`](Self::try_write_data) takes only part of your
    ///    data, write out output and retry (with the rest) before passing
    ///    more input, if the exact interleaving of your calls with the
    ///    replies matters.
    ///
    /// # Guarantees
    ///
    /// For a caller that follows these rules: with the output written out
    /// and the events polled, a call with non-empty input consumes at least
    /// one byte, so `consumed == 0` means the input was empty, output is
    /// still waiting to be written (the socket did not take all of it), or
    /// an event was not polled. The payload, the events with their positions
    /// in the payload, every output byte that input produces, and all
    /// observable
    /// state depend only on the bytes received and on the input positions at
    /// which your own calls were accepted: never on how the input was split,
    /// on the length of `payload`, on how many bytes each socket write took,
    /// or on when output was written. Your calls appear in the output in the
    /// order they were accepted. Nothing is allocated.
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
                Effect::Subnegotiation => usize::from(self.sb_reserve),
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
                    // Copy the kept bytes out of the parser so that handling
                    // the request can update `self`.
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

    /// The oldest bytes that must be written to the socket; same contract
    /// as [`Client::output`](crate::Client::output).
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

    /// `true` if there are bytes to write to the socket:
    /// [`output`](Self::output) is not empty.
    pub fn has_output(&self) -> bool {
        self.io.has_output()
    }

    /// The event the last receive call stopped at, if any (events queued by
    /// the `alloc` layer come first). The core keeps one event: input is not
    /// processed while it is waiting, so call this until it returns `None`
    /// after every receive call. Ignoring events loses neither
    /// [`settings`](Self::settings) nor
    /// [`com_port_active`](Self::com_port_active). DTR, RTS and break are
    /// reported only as events ([`ServerEvent::Dtr`], [`ServerEvent::Rts`],
    /// [`ServerEvent::Break`]), which a bridge must apply to its port.
    #[must_use = "events report what the client asked for"]
    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.io.poll_event()
    }

    /// Escape and queue the longest prefix of `data` that fits (an escaped
    /// 0xFF is never split); returns how many bytes of `data` were taken, at
    /// least one unless `data` is empty. Write out the output, then pass the
    /// rest again. Server data is never gated by COM-PORT-OPTION.
    ///
    /// Calls use at most the write budget: the free output space minus the
    /// room kept for the replies input may cause (19 bytes with the default
    /// signature, see [`OUTPUT_CAPACITY`](Self::OUTPUT_CAPACITY)), so writing
    /// never starves the protocol. With the output drained, at least 61
    /// bytes are taken whatever the signature.
    ///
    /// # Errors
    /// [`CommandError::OutputFull`] if not even one byte fits: write out
    /// output and retry.
    pub fn try_write_data(&mut self, data: &[u8]) -> Result<usize, CommandError> {
        if data.is_empty() {
            return Ok(0);
        }
        let budget = self.budget();
        match telnet::put_data(&mut self.io.out, data, budget) {
            0 => Err(CommandError::OutputFull),
            n => Ok(n),
        }
    }

    // ---- core notifications: atomic, an error means nothing changed ------

    /// Record the levels of the port's modem lines (CD, RI, DSR, CTS) and
    /// notify the client of what changed (`NOTIFY-MODEMSTATE`).
    ///
    /// The first state supplied is sent as its levels through the client's
    /// modem-state mask (even when that leaves 0, but not when the mask is
    /// 0). After that, the change bits are computed from the previous levels
    /// (CTS, DSR or CD changed; RI went from on to off) and added to any
    /// change bits set in `state`, for a pulse too short to show in the
    /// levels; if that gives any change bit, the new levels and the change
    /// bits are sent through the mask, unless that leaves nothing. RFC 2217
    /// has no change bit for RI turning on, so a rising RI alone sends
    /// nothing: the client sees it in the next notification or poll (a ring
    /// is reported when RI turns off again). Only the levels are stored.
    /// While COM-PORT-OPTION is not active the levels are stored and
    /// nothing is sent: the client is sent the current levels when it
    /// enables the option.
    ///
    /// pyserial's client needs a modem state from the server (reading `cts`
    /// fails without one): supply one before the client enables
    /// COM-PORT-OPTION, or when [`ServerEvent::ComPortActive`] arrives. The
    /// server never invents one: without it, polls go unanswered.
    ///
    /// ```
    /// use rfc2217::{ModemState, PortSettings, Server, ServerEvent};
    ///
    /// let mut server = Server::new(PortSettings::default());
    /// // Not active yet: the levels are stored, nothing is sent.
    /// assert_eq!(server.try_set_modem_state(ModemState::DSR), Ok(()));
    ///
    /// // The client enables COM-PORT-OPTION: the server reports the levels.
    /// let p = server.receive_into(&[0xFF, 0xFB, 0x2C], &mut [0; 8]);
    /// assert_eq!(p.consumed, 3);
    /// assert_eq!(server.poll_event(), Some(ServerEvent::ComPortActive));
    /// assert!(server.output().ends_with(&[0xFF, 0xFA, 0x2C, 0x6B, 0x20, 0xFF, 0xF0]));
    /// server.consume_output(server.output().len());
    ///
    /// // CTS rises: the new levels go out with the CTS change bit.
    /// assert_eq!(server.try_set_modem_state(ModemState::DSR | ModemState::CTS), Ok(()));
    /// assert_eq!(server.output(), [0xFF, 0xFA, 0x2C, 0x6B, 0x31, 0xFF, 0xF0]);
    /// ```
    ///
    /// # Errors
    /// [`CommandError::OutputFull`] if a notification is due and does not
    /// fit: nothing was stored. Write out output and retry with the same
    /// state (retrying with a newer one may hide a short pulse between the
    /// two).
    pub fn try_set_modem_state(&mut self, state: ModemState) -> Result<(), CommandError> {
        let levels = state.bits() & LEVELS;
        let notification = match self.levels {
            // The first state supplied: the levels as they are.
            None => self.snapshot(levels, false),
            Some(old) => {
                let delta = changes(old, levels) | (state.bits() & !LEVELS);
                let value = (levels | delta) & self.modem_mask;
                (self.com_port_active() && delta != 0 && value != 0).then_some(value)
            }
        };
        if let Some(value) = notification {
            self.try_com(com::NOTIFY_MODEMSTATE, &[value])?;
        }
        self.levels = Some(levels);
        Ok(())
    }

    /// Send `state` through the client's line-state mask as a line-state
    /// notification (`NOTIFY-LINESTATE`). Nothing is sent, and `Ok` returned,
    /// when the mask leaves nothing: its default is 0, so nothing is sent
    /// until the client sets one. The state is not stored.
    ///
    /// # Errors
    /// [`CommandError::Inactive`] if the client has not enabled
    /// COM-PORT-OPTION; [`CommandError::OutputFull`] if the notification does
    /// not fit: write out output and retry.
    pub fn try_notify_line_state(&mut self, state: LineState) -> Result<(), CommandError> {
        self.check_active()?;
        match state.bits() & self.line_mask {
            0 => Ok(()),
            value => self.try_com(com::NOTIFY_LINESTATE, &[value]),
        }
    }

    /// Ask the client to pause sending (`FLOWCONTROL-SUSPEND`). Advisory:
    /// nothing in the protocol makes the client comply, and pyserial's client
    /// ignores it (it expects the server's SUSPEND as code 8, not 108). The
    /// server keeps no flow state: each call sends the command.
    ///
    /// # Errors
    /// As [`try_notify_line_state`](Self::try_notify_line_state).
    pub fn try_suspend_flow(&mut self) -> Result<(), CommandError> {
        self.check_active()?;
        self.try_com(com::FLOWCONTROL_SUSPEND, &[])
    }

    /// Allow the client to resume sending (`FLOWCONTROL-RESUME`). Ignored by
    /// pyserial's client, as [`try_suspend_flow`](Self::try_suspend_flow).
    ///
    /// # Errors
    /// As [`try_notify_line_state`](Self::try_notify_line_state).
    pub fn try_resume_flow(&mut self) -> Result<(), CommandError> {
        self.check_active()?;
        self.try_com(com::FLOWCONTROL_RESUME, &[])
    }

    // ---- internals -------------------------------------------------------

    /// Output room for calls: everything but the larger receive reserve.
    fn budget(&self) -> usize {
        let reserve = usize::from(self.sb_reserve).max(NEG_RESERVE);
        self.io.out.free().saturating_sub(reserve)
    }

    /// Queue one server-to-client COM-PORT-OPTION command (`command` is the
    /// client-direction code), if it fits.
    fn try_com(&mut self, command: u8, payload: &[u8]) -> Result<(), CommandError> {
        if self.budget() < telnet::com_len(payload) {
            return Err(CommandError::OutputFull);
        }
        self.ack(command, payload);
        Ok(())
    }

    /// Notifications other than the modem state are only sent while the
    /// client has COM-PORT-OPTION enabled.
    fn check_active(&self) -> Result<(), CommandError> {
        if self.com_port_active() {
            Ok(())
        } else {
            Err(CommandError::Inactive)
        }
    }

    /// The modem-state snapshot of `levels` (the levels through the modem
    /// mask, sent even when that is 0), if one is due: only while active,
    /// and, unless the client polled, only if the mask lets anything through.
    fn snapshot(&self, levels: u8, poll: bool) -> Option<u8> {
        let due = self.com_port_active() && (poll || self.modem_mask != 0);
        due.then_some(levels & self.modem_mask)
    }

    /// Send the snapshot of the stored levels, if any, for a trigger in the
    /// input: activation, a modem mask change or a poll. It fits the receive
    /// reserve of the byte that caused it.
    fn send_snapshot(&mut self, poll: bool) {
        if let Some(value) = self.levels.and_then(|levels| self.snapshot(levels, poll)) {
            self.ack(com::NOTIFY_MODEMSTATE, &[value]);
        }
    }

    fn send_reply(&mut self, reply: Option<Reply>) {
        if let Some(reply) = reply {
            telnet::put_neg(&mut self.io.out, reply);
        }
    }

    /// After every negotiation command: queue its reply, then follow
    /// [`com_port_active`](Self::com_port_active). Every transition into
    /// active announces `ComPortActive` and sends the modem-state snapshot,
    /// if the application has supplied a state; a withdrawal is silent.
    fn negotiated(&mut self, reply: Option<Reply>) {
        self.send_reply(reply);
        let active = self.com_port_active();
        if active == self.was_active {
            return;
        }
        self.was_active = active;
        if active {
            self.io.put_event(ServerEvent::ComPortActive);
            self.send_snapshot(false);
        }
    }

    /// Queue the server-direction form of `command` (code + 100).
    fn ack(&mut self, command: u8, payload: &[u8]) {
        debug_assert!(
            command < com::SERVER_OFFSET,
            "{command} is not a client-direction command"
        );
        telnet::put_com(&mut self.io.out, command + com::SERVER_OFFSET, payload);
    }

    /// Handle a completed subnegotiation: `sb` holds its kept bytes (option
    /// first) and `len` its true length, which exceeds `sb.len()` only for a
    /// body longer than the parser keeps. Such a body is never a fixed-length
    /// request. Requests are ignored until the client has enabled
    /// COM-PORT-OPTION.
    fn subnegotiation(&mut self, sb: &[u8], len: usize) {
        let [OPT_COM_PORT, command, ref payload @ ..] = *sb else {
            debug!("ignoring a subnegotiation without a COM-PORT-OPTION command: {sb:?}");
            return;
        };
        if !self.com_port_active() {
            debug!(
                "ignoring COM-PORT-OPTION command {command}: the client has not enabled the option"
            );
            return;
        }
        trace!(
            "rfc2217 request {command}, payload {payload:?} ({} bytes)",
            len - 2
        );
        // The payload, if it is complete: every request but SIGNATURE has a
        // fixed length, which is checked against the true length.
        let exact = (len == sb.len()).then_some(payload);
        let before = self.settings;
        match command {
            // An empty SIGNATURE asks for ours.
            com::SIGNATURE if len == 2 => {
                let signature = self.signature;
                self.ack(com::SIGNATURE, &signature);
            }
            // Any other is the client's own, kept truncated if long.
            com::SIGNATURE => {
                let signature = InlineBytes::from_prefix(payload, len - 2);
                self.io.put_event(ServerEvent::ClientSignature(signature));
            }
            com::SET_BAUDRATE => match exact.and_then(baud_request) {
                Some(baud) => {
                    // 0 asks for the current rate.
                    if baud != 0 {
                        self.settings.baud_rate = baud;
                    }
                    self.ack(command, &self.settings.baud_rate.to_be_bytes());
                }
                None => malformed(command, len),
            },
            com::SET_DATASIZE => match single(exact) {
                Some(code) => {
                    if let Some(bits) = DataBits::from_code(code) {
                        self.settings.data_bits = bits;
                    }
                    self.ack(command, &[self.settings.data_bits.code()]);
                }
                None => malformed(command, len),
            },
            com::SET_PARITY => match single(exact) {
                Some(code) => {
                    if let Some(parity) = Parity::from_code(code) {
                        self.settings.parity = parity;
                    }
                    self.ack(command, &[self.settings.parity.code()]);
                }
                None => malformed(command, len),
            },
            com::SET_STOPSIZE => match single(exact) {
                Some(code) => {
                    if let Some(bits) = StopBits::from_code(code) {
                        self.settings.stop_bits = bits;
                    }
                    self.ack(command, &[self.settings.stop_bits.code()]);
                }
                None => malformed(command, len),
            },
            com::SET_CONTROL => match single(exact) {
                Some(value) => self.set_control(value),
                None => malformed(command, len),
            },
            com::SET_LINESTATE_MASK => match single(exact) {
                Some(mask) => {
                    self.line_mask = mask;
                    self.ack(command, &[mask]);
                }
                None => malformed(command, len),
            },
            com::SET_MODEMSTATE_MASK => match single(exact) {
                Some(mask) => {
                    self.modem_mask = mask;
                    self.ack(command, &[mask]);
                    self.send_snapshot(false);
                }
                None => malformed(command, len),
            },
            com::PURGE_DATA => match single(exact) {
                Some(code) => {
                    if let Some(purge) = Purge::from_code(code) {
                        self.io.put_event(ServerEvent::Purge(purge));
                    }
                    self.ack(command, &[code]);
                }
                None => malformed(command, len),
            },
            // An empty NOTIFY-MODEMSTATE is pyserial's modem state poll:
            // answered with the levels through the mask, whatever the mask,
            // once the application has supplied them.
            com::NOTIFY_MODEMSTATE if len == 2 => {
                if self.levels.is_none() {
                    debug!("no modem state to answer the client's poll with");
                }
                self.send_snapshot(true);
            }
            // Other notifications go from the server to the client.
            com::NOTIFY_LINESTATE | com::NOTIFY_MODEMSTATE => {
                debug!("ignoring COM-PORT-OPTION command {command}: a server notification");
            }
            // The client cannot pause the server's data in 0.1.
            com::FLOWCONTROL_SUSPEND | com::FLOWCONTROL_RESUME => {
                debug!("ignoring the client's FLOWCONTROL-SUSPEND/RESUME ({command})");
            }
            _ => debug!("ignoring unknown COM-PORT-OPTION command {command}"),
        }
        if self.settings != before {
            self.io
                .put_event(ServerEvent::SettingsChanged(self.settings));
        }
    }

    /// Handle SET-CONTROL `value`: apply it, report a line change, and
    /// answer with the state now in effect.
    fn set_control(&mut self, value: u8) {
        use com::control::*;
        let reply = match value {
            REQUEST_FLOW => self.settings.flow_control.code(),
            FLOW_NONE | FLOW_XONXOFF | FLOW_HARDWARE | FLOW_DCD | FLOW_DSR => {
                // `from_code` decodes exactly these five values.
                if let Some(flow) = FlowControl::from_code(value) {
                    self.settings.flow_control = flow;
                }
                self.settings.flow_control.code()
            }
            REQUEST_BREAK => {
                if self.brk {
                    BREAK_ON
                } else {
                    BREAK_OFF
                }
            }
            BREAK_ON | BREAK_OFF => {
                self.brk = value == BREAK_ON;
                self.io.put_event(ServerEvent::Break(self.brk));
                value
            }
            REQUEST_DTR => {
                if self.dtr {
                    DTR_ON
                } else {
                    DTR_OFF
                }
            }
            DTR_ON | DTR_OFF => {
                self.dtr = value == DTR_ON;
                self.io.put_event(ServerEvent::Dtr(self.dtr));
                value
            }
            REQUEST_RTS => {
                if self.rts {
                    RTS_ON
                } else {
                    RTS_OFF
                }
            }
            RTS_ON | RTS_OFF => {
                self.rts = value == RTS_ON;
                self.io.put_event(ServerEvent::Rts(self.rts));
                value
            }
            // Inbound flow control is not set apart from outbound flow
            // control: every inbound request or query is answered with the
            // inbound value the outbound setting implies.
            REQUEST_FLOW_IN | FLOW_IN_NONE | FLOW_IN_XONXOFF | FLOW_IN_HARDWARE | FLOW_IN_DTR => {
                match self.settings.flow_control {
                    FlowControl::RtsCts => FLOW_IN_HARDWARE,
                    FlowControl::XonXoff => FLOW_IN_XONXOFF,
                    FlowControl::None | FlowControl::Dcd | FlowControl::Dsr => FLOW_IN_NONE,
                }
            }
            // Unassigned (20-255): nothing to apply.
            _ => self.settings.flow_control.code(),
        };
        self.ack(com::SET_CONTROL, &[reply]);
    }
}

/// Log a request whose payload length is wrong (`len` is the true body
/// length): it is not answered.
fn malformed(command: u8, len: usize) {
    debug!(
        "ignoring COM-PORT-OPTION command {command}: a {}-byte payload is not a valid request",
        len - 2
    );
}

/// The change bits of `NOTIFY-MODEMSTATE` for modem levels going from `old`
/// to `new`: CTS, DSR or CD changed, or RI went from on to off.
fn changes(old: u8, new: u8) -> u8 {
    let (old, new) = (ModemState::from_bits(old), ModemState::from_bits(new));
    let mut delta = ModemState::default();
    for (happened, bit) in [
        (old.cts() != new.cts(), ModemState::DELTA_CTS),
        (old.dsr() != new.dsr(), ModemState::DELTA_DSR),
        (old.ri() && !new.ri(), ModemState::TRAILING_EDGE_RI),
        (old.cd() != new.cd(), ModemState::DELTA_CD),
    ] {
        if happened {
            delta |= bit;
        }
    }
    delta.bits()
}

/// The single byte of a complete one-byte payload.
fn single(payload: Option<&[u8]>) -> Option<u8> {
    match payload {
        Some(&[value]) => Some(value),
        _ => None,
    }
}

/// A SET-BAUDRATE request's payload as a rate, 0 being a query. The lengths
/// the client accepts in replies: 4 bytes big-endian, or 8 with the rate in
/// one half and zeros in the other (sredird's `unsigned long`). `None` for
/// any other payload.
fn baud_request(payload: &[u8]) -> Option<u32> {
    match *payload {
        [a, b, c, d] | [a, b, c, d, 0, 0, 0, 0] | [0, 0, 0, 0, a, b, c, d] => {
            Some(u32::from_be_bytes([a, b, c, d]))
        }
        _ => None,
    }
}
