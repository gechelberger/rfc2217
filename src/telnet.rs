//! Telnet framing (RFC 854 / 855) and the COM-PORT-OPTION codes (RFC 2217).

use crate::bytes::InlineBytes;
use crate::io::OutBuf;
use crate::log::debug;
use crate::negotiation::Reply;

/// Interpret As Command.
pub(crate) const IAC: u8 = 255;
/// Option negotiation verbs.
pub(crate) const DONT: u8 = 254;
/// Option negotiation verbs.
pub(crate) const DO: u8 = 253;
/// Option negotiation verbs.
pub(crate) const WONT: u8 = 252;
/// Option negotiation verbs.
pub(crate) const WILL: u8 = 251;
/// Start of subnegotiation.
pub(crate) const SB: u8 = 250;
/// End of subnegotiation.
pub(crate) const SE: u8 = 240;
/// No operation.
#[cfg(test)]
pub(crate) const NOP: u8 = 241;

/// Telnet option: binary transmission (RFC 856).
pub(crate) const OPT_BINARY: u8 = 0;
/// Telnet option: echo (RFC 857).
#[cfg(test)]
pub(crate) const OPT_ECHO: u8 = 1;
/// Telnet option: suppress go-ahead (RFC 858).
pub(crate) const OPT_SGA: u8 = 3;
/// Telnet option: COM-PORT-OPTION (RFC 2217).
pub(crate) const OPT_COM_PORT: u8 = 44;

/// Body length (unescaped, option byte included) at which the parser gives
/// up on a subnegotiation: the next body byte discards it and is delivered as
/// data. This bounds the data a stray `IAC SB` can swallow. The length is
/// counted, not stored, so memory does not depend on this limit.
pub(crate) const SB_LIMIT: u16 = 1024;

/// Body bytes the parser keeps: option, command code and
/// [`InlineBytes::CAPACITY`] payload bytes. Every decodable COM-PORT-OPTION
/// body is at most 10 bytes; a longer body is kept truncated, together with
/// its true length.
pub(crate) const SB_KEEP: usize = 2 + InlineBytes::CAPACITY;

/// COM-PORT-OPTION subnegotiation commands, client to server.
/// The server answers with the same code plus [`com::SERVER_OFFSET`].
pub(crate) mod com {
    /// Exchange signature strings.
    pub(crate) const SIGNATURE: u8 = 0;
    /// Set baud rate (4-byte big-endian value, 0 = query).
    pub(crate) const SET_BAUDRATE: u8 = 1;
    /// Set data bits (0 = query).
    pub(crate) const SET_DATASIZE: u8 = 2;
    /// Set parity (0 = query).
    pub(crate) const SET_PARITY: u8 = 3;
    /// Set stop bits (0 = query).
    pub(crate) const SET_STOPSIZE: u8 = 4;
    /// Flow control, break, DTR and RTS (see [`control`]).
    pub(crate) const SET_CONTROL: u8 = 5;
    /// Line state notification (server to client).
    pub(crate) const NOTIFY_LINESTATE: u8 = 6;
    /// Modem state notification (server to client).
    pub(crate) const NOTIFY_MODEMSTATE: u8 = 7;
    /// Server asks the client to stop sending.
    pub(crate) const FLOWCONTROL_SUSPEND: u8 = 8;
    /// Server allows the client to resume sending.
    pub(crate) const FLOWCONTROL_RESUME: u8 = 9;
    /// Set which line-state changes are reported.
    pub(crate) const SET_LINESTATE_MASK: u8 = 10;
    /// Set which modem-state changes are reported.
    pub(crate) const SET_MODEMSTATE_MASK: u8 = 11;
    /// Purge buffers.
    pub(crate) const PURGE_DATA: u8 = 12;
    /// Added to a command code in the server's response.
    pub(crate) const SERVER_OFFSET: u8 = 100;

    /// Values of the `SET_CONTROL` command.
    pub(crate) mod control {
        /// Query outbound flow control.
        pub(crate) const REQUEST_FLOW: u8 = 0;
        /// Use no outbound flow control.
        pub(crate) const FLOW_NONE: u8 = 1;
        /// Use XON/XOFF outbound flow control.
        pub(crate) const FLOW_XONXOFF: u8 = 2;
        /// Use hardware (RTS/CTS) outbound flow control.
        pub(crate) const FLOW_HARDWARE: u8 = 3;
        /// Request break state.
        pub(crate) const REQUEST_BREAK: u8 = 4;
        /// Set break on.
        pub(crate) const BREAK_ON: u8 = 5;
        /// Set break off.
        pub(crate) const BREAK_OFF: u8 = 6;
        /// Request DTR state.
        pub(crate) const REQUEST_DTR: u8 = 7;
        /// Set DTR on.
        pub(crate) const DTR_ON: u8 = 8;
        /// Set DTR off.
        pub(crate) const DTR_OFF: u8 = 9;
        /// Request RTS state.
        pub(crate) const REQUEST_RTS: u8 = 10;
        /// Set RTS on.
        pub(crate) const RTS_ON: u8 = 11;
        /// Set RTS off.
        pub(crate) const RTS_OFF: u8 = 12;
        /// Query inbound flow control.
        pub(crate) const REQUEST_FLOW_IN: u8 = 13;
        /// Use no inbound flow control.
        pub(crate) const FLOW_IN_NONE: u8 = 14;
        /// Use XON/XOFF inbound flow control.
        pub(crate) const FLOW_IN_XONXOFF: u8 = 15;
        /// Use hardware (RTS/CTS) inbound flow control.
        pub(crate) const FLOW_IN_HARDWARE: u8 = 16;
        /// Use DCD outbound flow control.
        pub(crate) const FLOW_DCD: u8 = 17;
        /// Use DTR inbound flow control.
        pub(crate) const FLOW_IN_DTR: u8 = 18;
        /// Use DSR outbound flow control.
        pub(crate) const FLOW_DSR: u8 = 19;
    }
}

/// Something the Telnet stream parser recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelnetEvent {
    /// A payload byte.
    Data(u8),
    /// A bare command such as `NOP`.
    Command(u8),
    /// `IAC WILL <option>`.
    Will(u8),
    /// `IAC WONT <option>`.
    Wont(u8),
    /// `IAC DO <option>`.
    Do(u8),
    /// `IAC DONT <option>`.
    Dont(u8),
    /// `IAC SB ... IAC SE` completed. Read it with [`Parser::body`] (the kept
    /// bytes, option first, `IAC IAC` unescaped) and [`Parser::body_len`]
    /// (its true length) before the next [`Parser::push`].
    Subnegotiation,
}

/// What pushing a byte would produce, by kind, so that the owner can check
/// it has room for that kind's worst case before it consumes the byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// No payload, no reply, no event: a state change or a bare command.
    Nothing,
    /// One payload byte (also the byte that ends an oversized subnegotiation).
    Payload,
    /// The option byte that completes `IAC WILL|WONT|DO|DONT <option>`.
    Negotiation,
    /// The `SE` that completes a subnegotiation.
    Subnegotiation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Data,
    Iac,
    Will,
    Wont,
    Do,
    Dont,
    Sb,
    SbIac,
}

/// Incremental Telnet stream parser on fixed storage.
#[derive(Debug)]
pub(crate) struct Parser {
    state: State,
    /// Unescaped body length of the current or last completed
    /// subnegotiation, option byte included. Never above [`SB_LIMIT`].
    sb_len: u16,
    /// The first [`SB_KEEP`] body bytes.
    sb: [u8; SB_KEEP],
}

impl Parser {
    /// A parser in the initial state.
    pub(crate) const fn new() -> Self {
        Self {
            state: State::Data,
            sb_len: 0,
            sb: [0; SB_KEEP],
        }
    }

    /// Kept bytes of the subnegotiation just completed: at most [`SB_KEEP`].
    pub(crate) fn body(&self) -> &[u8] {
        &self.sb[..usize::from(self.sb_len).min(SB_KEEP)]
    }

    /// True length of the subnegotiation just completed. Above
    /// `body().len()` only when the body was longer than [`SB_KEEP`].
    pub(crate) fn body_len(&self) -> usize {
        usize::from(self.sb_len)
    }

    /// What [`push`](Self::push) would produce for `byte`, without changing
    /// any state. Agrees with `push` for every state and byte.
    pub(crate) fn effect(&self, byte: u8) -> Effect {
        let full = self.sb_len >= SB_LIMIT;
        match self.state {
            State::Data if byte == IAC => Effect::Nothing,
            State::Data => Effect::Payload,
            State::Iac if byte == IAC => Effect::Payload,
            State::Iac => Effect::Nothing,
            State::Will | State::Wont | State::Do | State::Dont => Effect::Negotiation,
            State::Sb if byte != IAC && full => Effect::Payload,
            State::Sb => Effect::Nothing,
            State::SbIac => match byte {
                IAC if full => Effect::Payload,
                SE => Effect::Subnegotiation,
                // An escaped IAC that is kept, or a command that aborts the
                // subnegotiation and only changes state (or is bare).
                _ => Effect::Nothing,
            },
        }
    }

    /// Feed one byte.
    pub(crate) fn push(&mut self, byte: u8) -> Option<TelnetEvent> {
        match self.state {
            State::Data => {
                if byte == IAC {
                    self.state = State::Iac;
                    None
                } else {
                    Some(TelnetEvent::Data(byte))
                }
            }
            State::Iac => self.command(byte),
            State::Will => {
                self.state = State::Data;
                Some(TelnetEvent::Will(byte))
            }
            State::Wont => {
                self.state = State::Data;
                Some(TelnetEvent::Wont(byte))
            }
            State::Do => {
                self.state = State::Data;
                Some(TelnetEvent::Do(byte))
            }
            State::Dont => {
                self.state = State::Data;
                Some(TelnetEvent::Dont(byte))
            }
            State::Sb => {
                if byte == IAC {
                    self.state = State::SbIac;
                    None
                } else {
                    self.append(byte)
                }
            }
            State::SbIac => match byte {
                IAC => {
                    self.state = State::Sb;
                    self.append(IAC)
                }
                SE => {
                    self.state = State::Data;
                    Some(TelnetEvent::Subnegotiation)
                }
                other => {
                    // `IAC <command>` inside a subnegotiation: the partial
                    // body is dropped and the command processed as if it had
                    // followed an IAC in the data state.
                    debug!(
                        "IAC {other} inside a subnegotiation: discarding {} body bytes",
                        self.sb_len
                    );
                    self.sb_len = 0;
                    self.command(other)
                }
            },
        }
    }

    /// The byte after an `IAC` outside a subnegotiation.
    fn command(&mut self, byte: u8) -> Option<TelnetEvent> {
        let (state, event) = match byte {
            IAC => (State::Data, Some(TelnetEvent::Data(IAC))),
            WILL => (State::Will, None),
            WONT => (State::Wont, None),
            DO => (State::Do, None),
            DONT => (State::Dont, None),
            SB => {
                self.sb_len = 0;
                (State::Sb, None)
            }
            other => (State::Data, Some(TelnetEvent::Command(other))),
        };
        self.state = state;
        event
    }

    /// Append one unescaped byte to the body, or give up on an oversized
    /// subnegotiation and deliver the byte as data.
    fn append(&mut self, byte: u8) -> Option<TelnetEvent> {
        if self.sb_len >= SB_LIMIT {
            debug!("subnegotiation longer than {SB_LIMIT} bytes: discarding it, resuming as data");
            self.state = State::Data;
            self.sb_len = 0;
            return Some(TelnetEvent::Data(byte));
        }
        if let Some(slot) = self.sb.get_mut(usize::from(self.sb_len)) {
            *slot = byte;
        }
        self.sb_len += 1;
        None
    }
}

/// Log a bare command (`NOP`, a stray `SE`, ...), which both halves ignore.
pub(crate) fn ignore_command(command: u8) {
    if command < SE {
        debug!("ignoring IAC {command}: not a Telnet command, the peer may not be speaking Telnet");
    } else {
        debug!("ignoring Telnet command {command}");
    }
}

/// Bytes `IAC SB 44 <command> <payload, IAC doubled> IAC SE` takes on the wire.
pub(crate) fn com_len(payload: &[u8]) -> usize {
    6 + payload.len() + payload.iter().filter(|&&b| b == IAC).count()
}

/// Queue one negotiation command, `IAC <verb> <option>`.
pub(crate) fn put_neg(out: &mut OutBuf, reply: Reply) {
    out.push(&reply);
}

/// Queue `IAC SB 44 <command> <payload, IAC doubled> IAC SE`
/// ([`com_len`] bytes). Command codes are below 255 and go out as they are.
pub(crate) fn put_com(out: &mut OutBuf, command: u8, payload: &[u8]) {
    debug_assert!(command != IAC, "COM-PORT-OPTION command 255");
    out.push(&[IAC, SB, OPT_COM_PORT, command]);
    for &b in payload {
        if b == IAC {
            out.push(&[IAC, IAC]);
        } else {
            out.push(&[b]);
        }
    }
    out.push(&[IAC, SE]);
}

/// Escape and queue the longest prefix of `data` whose escaped form fits in
/// `budget` bytes; an escaped IAC is never split. Returns how many bytes of
/// `data` were taken.
pub(crate) fn put_data(out: &mut OutBuf, data: &[u8], budget: usize) -> usize {
    let mut used = 0;
    let mut taken = 0;
    for &b in data {
        let escaped: &[u8] = if b == IAC { &[IAC, IAC] } else { &[b] };
        if used + escaped.len() > budget {
            break;
        }
        out.push(escaped);
        used += escaped.len();
        taken += 1;
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kind of what `push` returned, in `effect`'s terms.
    fn kind(event: Option<TelnetEvent>) -> Effect {
        match event {
            None | Some(TelnetEvent::Command(_)) => Effect::Nothing,
            Some(TelnetEvent::Data(_)) => Effect::Payload,
            Some(
                TelnetEvent::Will(_)
                | TelnetEvent::Wont(_)
                | TelnetEvent::Do(_)
                | TelnetEvent::Dont(_),
            ) => Effect::Negotiation,
            Some(TelnetEvent::Subnegotiation) => Effect::Subnegotiation,
        }
    }

    /// Push every byte, checking before each one that `effect` predicts what
    /// `push` returns, and hand every event to `seen` with the parser.
    fn run(bytes: &[u8], mut seen: impl FnMut(&Parser, TelnetEvent)) {
        let mut p = Parser::new();
        for &b in bytes {
            let effect = p.effect(b);
            let event = p.push(b);
            assert_eq!(effect, kind(event), "byte {b:#04x}");
            if let Some(event) = event {
                seen(&p, event);
            }
        }
    }

    fn parse(bytes: &[u8]) -> Vec<TelnetEvent> {
        let mut events = Vec::new();
        run(bytes, |_, event| events.push(event));
        events
    }

    /// Kept bytes and true length of every completed subnegotiation.
    fn subnegotiations(bytes: &[u8]) -> Vec<(Vec<u8>, usize)> {
        let mut bodies = Vec::new();
        run(bytes, |p, event| {
            if event == TelnetEvent::Subnegotiation {
                bodies.push((p.body().to_vec(), p.body_len()));
            }
        });
        bodies
    }

    #[test]
    fn data_and_escaped_iac() {
        assert_eq!(
            parse(&[b'a', IAC, IAC, b'b']),
            vec![
                TelnetEvent::Data(b'a'),
                TelnetEvent::Data(IAC),
                TelnetEvent::Data(b'b')
            ]
        );
    }

    #[test]
    fn negotiation_verbs() {
        assert_eq!(
            parse(&[
                IAC, WILL, 44, IAC, WONT, OPT_ECHO, IAC, DO, 3, IAC, DONT, 0, IAC, NOP
            ]),
            vec![
                TelnetEvent::Will(44),
                TelnetEvent::Wont(OPT_ECHO),
                TelnetEvent::Do(3),
                TelnetEvent::Dont(0),
                TelnetEvent::Command(NOP)
            ]
        );
    }

    #[test]
    fn subnegotiation_with_escaped_iac() {
        let mut buf = OutBuf::new();
        put_com(&mut buf, 1, &[0, 0, IAC, 0]);
        let out = buf.as_slice().to_vec();
        assert_eq!(out, vec![IAC, SB, 44, 1, 0, 0, IAC, IAC, 0, IAC, SE]);
        assert_eq!(out.len(), com_len(&[0, 0, IAC, 0]));
        assert_eq!(parse(&out), vec![TelnetEvent::Subnegotiation]);
        assert_eq!(subnegotiations(&out), vec![(vec![44, 1, 0, 0, IAC, 0], 6)]);
    }

    #[test]
    fn data_is_escaped_within_the_budget() {
        let mut out = OutBuf::new();
        put_neg(&mut out, [IAC, WILL, 1]);
        // `a`, then the escaped IAC needs 2 of the remaining 2 bytes.
        assert_eq!(put_data(&mut out, &[b'a', IAC, b'b'], 3), 2);
        // An escaped IAC is never split: 1 byte of budget takes nothing.
        assert_eq!(put_data(&mut out, &[IAC], 1), 0);
        assert_eq!(put_data(&mut out, b"b", 1), 1);
        assert_eq!(put_data(&mut out, &[], 10), 0);
        assert_eq!(out.as_slice(), &[IAC, WILL, 1, b'a', IAC, IAC, b'b']);
    }

    #[test]
    fn iac_command_inside_sb_aborts_it_and_is_processed() {
        assert_eq!(
            parse(&[IAC, SB, 44, IAC, 0x78, 0x79]),
            vec![TelnetEvent::Command(0x78), TelnetEvent::Data(0x79)]
        );
        // A verb is negotiated; the SE that would have ended the
        // subnegotiation is then a stray bare command.
        assert_eq!(
            parse(&[IAC, SB, 44, 106, IAC, WILL, 1, b'A', IAC, SE]),
            vec![
                TelnetEvent::Will(1),
                TelnetEvent::Data(b'A'),
                TelnetEvent::Command(SE)
            ]
        );
        assert_eq!(
            parse(&[IAC, SB, 44, 107, IAC, NOP, 0x30, IAC, SE]),
            vec![
                TelnetEvent::Command(NOP),
                TelnetEvent::Data(0x30),
                TelnetEvent::Command(SE)
            ]
        );
    }

    #[test]
    fn nested_iac_sb_starts_new_subnegotiation() {
        let input = [IAC, SB, 44, 0x6B, IAC, SB, 44, 0x6B, 0x30, IAC, SE];
        assert_eq!(parse(&input), vec![TelnetEvent::Subnegotiation]);
        assert_eq!(subnegotiations(&input), vec![(vec![44, 0x6B, 0x30], 3)]);
    }

    #[test]
    fn unterminated_sb_is_bounded_and_data_resumes() {
        let mut input = vec![IAC, SB, 44];
        input.extend([b'a'; 2000]);
        assert_eq!(parse(&input), vec![TelnetEvent::Data(b'a'); 977]);

        // A body of exactly the limit still completes.
        let mut input = vec![IAC, SB, 44];
        input.extend([b'a'; 1023]);
        input.extend([IAC, SE]);
        let mut kept = vec![44];
        kept.extend([b'a'; SB_KEEP - 1]);
        assert_eq!(subnegotiations(&input), vec![(kept, 1024)]);
    }

    #[test]
    fn escaped_iac_at_cap_overflows_to_data() {
        let mut input = vec![IAC, SB, 44];
        input.extend([b'a'; 1023]);
        input.extend([IAC, IAC, b'b', IAC, SE]);
        assert_eq!(
            parse(&input),
            vec![
                TelnetEvent::Data(IAC),
                TelnetEvent::Data(b'b'),
                TelnetEvent::Command(SE)
            ]
        );

        // One byte earlier the escaped IAC is the body's last byte.
        let mut input = vec![IAC, SB, 44];
        input.extend([b'a'; 1022]);
        input.extend([IAC, IAC, IAC, SE]);
        let mut kept = vec![44];
        kept.extend([b'a'; SB_KEEP - 1]);
        assert_eq!(subnegotiations(&input), vec![(kept, 1024)]);
    }

    #[test]
    fn stray_se_outside_sb_is_ignored() {
        // `IAC SE` outside a subnegotiation is a bare command, which the
        // owners ignore; it never completes a subnegotiation.
        assert_eq!(
            parse(&[b'a', IAC, SE, b'b']),
            vec![
                TelnetEvent::Data(b'a'),
                TelnetEvent::Command(SE),
                TelnetEvent::Data(b'b')
            ]
        );
        // Without an IAC, SE is a payload byte like any other.
        assert_eq!(parse(&[SE]), vec![TelnetEvent::Data(SE)]);
    }

    #[test]
    fn long_subnegotiation_is_kept_truncated_with_its_length() {
        let mut input = vec![IAC, SB, 44, 0x64];
        input.extend([0x78; 200]);
        input.extend([IAC, SE]);
        let mut kept = vec![44, 0x64];
        kept.extend([0x78; 64]);
        assert_eq!(subnegotiations(&input), vec![(kept, 202)]);
    }

    /// xorshift64: deterministic, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn effect_agrees_with_push() {
        let mut rng = Rng(0x2217_0000_2217_0001);
        let mut give_ups = 0;
        for _ in 0..10_000 {
            // One sequence in eight opens a subnegotiation whose body only
            // escapes IAC until it has run past the give-up point (about 1.2
            // input bytes per body byte).
            let long = rng.next() % 8 == 0;
            let len = if long {
                1300 + rng.next() % 400
            } else {
                rng.next() % 48
            };
            let mut input = Vec::new();
            if long {
                input.extend([IAC, SB, OPT_COM_PORT]);
            }
            while (input.len() as u64) < len {
                let r = rng.next();
                let byte = match r % 32 {
                    0..=5 => IAC,
                    6 => SB,
                    7 => SE,
                    8 => WILL,
                    9 => WONT,
                    10 => DO,
                    11 => DONT,
                    _ => (r >> 32) as u8,
                };
                if long && byte == IAC && input.len() < 1300 {
                    input.extend([IAC, IAC]);
                } else {
                    input.push(byte);
                }
            }

            let mut p = Parser::new();
            for &b in &input {
                let in_sb = matches!(p.state, State::Sb | State::SbIac);
                let effect = p.effect(b);
                let event = p.push(b);
                assert_eq!(effect, kind(event), "byte {b:#04x} of {input:02x?}");
                if in_sb && effect == Effect::Payload {
                    give_ups += 1;
                }
                assert!(p.body_len() <= usize::from(SB_LIMIT));
                assert!(p.body().len() <= SB_KEEP);
            }
        }
        assert!(give_ups > 0, "no sequence reached the give-up point");
    }
}
