//! Backpressure: the core's fixed buffers push back instead of losing
//! anything, calls are atomic and leave the receive reserve free, and the
//! output slice contract. Core API only, so it runs in every configuration.

mod common;

use common::{Half, hex, sb, ser2net_stream};
use rfc2217::{
    Client, ComPortState, CommandError, Event, LineState, ModemState, PortSettings, Purge, Server,
    ServerEvent,
};

/// Remove and return everything `half` has pending for the socket.
fn drain<H: Half>(half: &mut H) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let pending = half.pending_output();
        if pending.is_empty() {
            return bytes;
        }
        let n = pending.len();
        bytes.extend_from_slice(pending);
        half.written(n);
    }
}

/// The opening of a client (`WILL`/`DO` BINARY, SGA, COM-PORT-OPTION).
fn opening() -> Vec<u8> {
    hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C")
}

/// An active client whose opening, settings round and event were drained.
fn active_client(settings: PortSettings) -> Client {
    let mut c = Client::new(settings);
    assert_eq!(drain(&mut c), opening());
    let p = c.receive_into(&hex("FF FD 2C"), &mut [0; 8]);
    assert_eq!(p.consumed, 3);
    assert_eq!(c.poll_event(), Some(Event::ComPortActive));
    assert_eq!(c.com_port_state(), ComPortState::Active);
    let _round = drain(&mut c);
    c
}

/// An active server (`WILL 44` accepted) whose reply and event were drained.
fn active_server(server: Server) -> Server {
    let mut s = server;
    let p = s.receive_into(&hex("FF FB 2C"), &mut [0; 8]);
    assert_eq!(p.consumed, 3);
    assert_eq!(s.poll_event(), Some(ServerEvent::ComPortActive));
    assert!(s.com_port_active());
    let _reply = drain(&mut s);
    s
}

/// Feed all of `input` through `receive_into` with a payload buffer of
/// `payload_len` bytes, polling after every call and draining whenever a call
/// consumes nothing. Returns the payload, the events with the payload length
/// at which each was polled, and the output.
fn run<H: Half>(half: &mut H, input: &[u8], payload_len: usize) -> Transcript<H::Event> {
    let mut t = Transcript {
        payload: Vec::new(),
        events: Vec::new(),
        output: Vec::new(),
    };
    let mut buf = vec![0; payload_len];
    let mut rest = input;
    while !rest.is_empty() {
        let p = half.receive_step(rest, &mut buf);
        rest = &rest[p.consumed..];
        t.payload.extend_from_slice(&buf[..p.payload_len]);
        while let Some(event) = half.next_event() {
            t.events.push((t.payload.len(), event));
        }
        t.output.extend(drain(half));
    }
    t
}

#[derive(Debug, PartialEq)]
struct Transcript<E> {
    payload: Vec<u8>,
    events: Vec<(usize, E)>,
    output: Vec<u8>,
}

#[test]
fn receive_stops_when_output_is_full_and_loses_nothing() {
    let mut c = active_client(PortSettings::default());
    let input = hex("FF FD 18").repeat(120);
    let mut payload = [0; 16];

    // Each refused DO TTYPE queues 3 bytes. The 71st reply leaves 43 bytes
    // free, below the 45 an option byte needs, so the call stops right
    // before the 72nd option byte (its IAC and DO need no room).
    let p = c.receive_into(&input, &mut payload);
    assert_eq!((p.consumed, p.payload_len), (71 * 3 + 2, 0));
    assert_eq!(c.output().len(), 71 * 3);
    assert!(Client::OUTPUT_CAPACITY - c.output().len() < 45);
    for _ in 0..3 {
        let again = c.receive_into(&input[p.consumed..], &mut payload);
        assert_eq!(again.consumed, 0, "no room, no progress");
    }

    // Draining lets it continue; nothing was lost or reordered.
    let mut wire = drain(&mut c);
    let mut at = p.consumed;
    while at < input.len() {
        let p = c.receive_into(&input[at..], &mut payload);
        assert!(p.consumed > 0);
        at += p.consumed;
        wire.extend(drain(&mut c));
    }
    assert_eq!(wire, hex("FF FC 18").repeat(120));
}

#[test]
fn unpolled_event_stalls_input_and_is_not_lost() {
    let mut c = Client::new(PortSettings::default());
    let input = [
        hex("FF FD 2C"),
        sb(107, &[0x30]).repeat(6),
        b"data".to_vec(),
    ]
    .concat();
    let mut payload = [0; 16];

    // Never polling: the first call stops right after the byte that
    // produced ComPortActive, and nothing after it is processed.
    let p = c.receive_into(&input, &mut payload);
    assert_eq!((p.consumed, p.payload_len), (3, 0));
    for _ in 0..5 {
        let again = c.receive_into(&input[3..], &mut payload);
        assert_eq!((again.consumed, again.payload_len), (0, 0));
    }

    // Polling after every call delivers every event, in order, and the
    // payload after the last one.
    let mut events = vec![c.poll_event().expect("the stalled event")];
    assert_eq!(c.poll_event(), None);
    let mut data = Vec::new();
    let mut at = 3;
    while at < input.len() {
        let p = c.receive_into(&input[at..], &mut payload);
        at += p.consumed;
        data.extend_from_slice(&payload[..p.payload_len]);
        while let Some(event) = c.poll_event() {
            assert!(data.is_empty(), "the payload follows the last event");
            events.push(event);
        }
    }
    let mut expected = vec![Event::ComPortActive];
    let state = ModemState::DSR | ModemState::CTS;
    expected.extend(std::iter::repeat_n(Event::ModemState(state), 6));
    assert_eq!(events, expected);
    assert_eq!(data, b"data");
}

#[test]
fn one_byte_payload_buffer_delivers_everything() {
    let stream = ser2net_stream();
    let reference = run(&mut Client::new(PortSettings::default()), &stream, 4096);
    let one = run(&mut Client::new(PortSettings::default()), &stream, 1);
    assert_eq!(one, reference);
    assert_eq!(reference.payload, b"JULABO HT30-M1 VERSION 4.00\r\n");
    // ComPortActive, the modem state, five settings replies and the purge.
    // Nothing asked for a signature here, so the empty one is the server's
    // request for ours: no event.
    assert_eq!(reference.events.len(), 2 + 5 + 1);
}

#[test]
fn calls_are_atomic_and_keep_the_receive_reserve() {
    let mut c = active_client(PortSettings::default());
    // Fill the write budget with data.
    let mut accepted = 0;
    loop {
        match c.try_write_data(&[0x55; 50]) {
            Ok(n) => accepted += n,
            Err(error) => {
                assert_eq!(error, CommandError::OutputFull);
                break;
            }
        }
    }
    assert_eq!(accepted, Client::OUTPUT_CAPACITY - 45);
    assert_eq!(c.output().len(), accepted);

    // A failing call writes nothing and changes nothing.
    let before = c.output().to_vec();
    assert_eq!(c.try_set_dtr(false), Err(CommandError::OutputFull));
    let other = PortSettings {
        baud_rate: 300,
        ..PortSettings::default()
    };
    assert_eq!(
        c.try_set_port_settings(other),
        Err(CommandError::OutputFull)
    );
    assert_eq!(c.requested_settings(), PortSettings::default());
    assert_eq!(c.try_write_data(&[0xFF]), Err(CommandError::OutputFull));
    assert_eq!(c.output(), &before[..]);

    // The receive reserve is still free: a negotiation is answered
    // without draining.
    let p = c.receive_into(&hex("FF FD 01"), &mut [0; 4]);
    assert_eq!(p.consumed, 3);
    let mut expected = before.clone();
    expected.extend(hex("FF FC 01"));
    assert_eq!(drain(&mut c), expected);

    // Server: a failing try_set_modem_state sends nothing and leaves the
    // model unchanged; after draining, the retry yields exactly the bytes of
    // an undisturbed run. Had the failing call stored the levels, the retry
    // would find nothing changed and send nothing.
    let first = ModemState::DSR;
    let state = ModemState::DSR | ModemState::CTS;
    let mut undisturbed = active_server(Server::new(PortSettings::default()));
    assert_eq!(undisturbed.try_set_modem_state(first), Ok(()));
    assert_eq!(undisturbed.try_set_modem_state(state), Ok(()));
    let undisturbed = drain(&mut undisturbed);
    // The first state as it is, then the CTS change.
    assert_eq!(undisturbed, [sb(107, &[0x20]), sb(107, &[0x31])].concat());

    let mut s = active_server(Server::new(PortSettings::default()));
    let mut notifications = Vec::new();
    for state in [first, state] {
        while s.try_write_data(b"data").is_ok() {}
        let before = s.output().to_vec();
        assert_eq!(s.try_set_modem_state(state), Err(CommandError::OutputFull));
        assert_eq!(s.output(), &before[..]);
        // 19 bytes stay free: the reply to the default signature.
        assert_eq!(Server::OUTPUT_CAPACITY - before.len(), 19);
        assert_eq!(drain(&mut s), before);
        assert_eq!(s.try_set_modem_state(state), Ok(()));
        notifications.extend(drain(&mut s));
    }
    assert_eq!(notifications, undisturbed);

    // The other notifications are atomic too.
    let p = s.receive_into(&sb(10, &[0xFF]), &mut [0; 4]);
    assert_eq!(p.consumed, 8, "the line-state mask");
    assert_eq!(drain(&mut s), sb(110, &[0xFF]));
    while s.try_write_data(b"data").is_ok() {}
    let before = s.output().to_vec();
    assert_eq!(
        s.try_notify_line_state(LineState::BREAK_DETECT),
        Err(CommandError::OutputFull)
    );
    assert_eq!(s.try_suspend_flow(), Err(CommandError::OutputFull));
    assert_eq!(s.try_resume_flow(), Err(CommandError::OutputFull));
    assert_eq!(drain(&mut s), before);
}

#[test]
fn drained_output_accepts_every_allowed_call() {
    // The largest settings round: every baud-rate byte is 0xFF.
    let settings = PortSettings {
        baud_rate: 0xFFFF_FFFF,
        ..PortSettings::default()
    };
    let mut c = active_client(settings);
    assert_eq!(c.try_set_dtr(true), Ok(()));
    drain(&mut c);
    assert_eq!(c.try_set_rts(true), Ok(()));
    drain(&mut c);
    assert_eq!(c.try_set_break(false), Ok(()));
    drain(&mut c);
    assert_eq!(c.try_purge(Purge::Both), Ok(()));
    drain(&mut c);
    assert_eq!(c.try_request_signature(), Ok(()));
    drain(&mut c);
    assert_eq!(c.try_set_port_settings(settings), Ok(()));
    assert_eq!(drain(&mut c).len(), 42);
    let taken = c.try_write_data(&[0xFF; 300]).expect("data fits");
    assert!(taken >= 105, "{taken}");
    assert_eq!(drain(&mut c), vec![0xFF; 2 * taken]);

    // The server with the worst signature keeps 134 bytes free.
    let mut s = active_server(Server::new(PortSettings::default()).with_signature([0xFF; 64]));
    // The client's line-state mask lets every bit through.
    let p = s.receive_into(&sb(10, &[0xFF]), &mut [0; 4]);
    assert_eq!(p.consumed, 8);
    assert_eq!(drain(&mut s), sb(110, &[0xFF]));
    // The largest modem-state notification: every level comes on, and
    // every change bit is set.
    assert_eq!(s.try_set_modem_state(ModemState::default()), Ok(()));
    assert_eq!(drain(&mut s), sb(107, &[0x00]));
    let modem = ModemState::from_bits(0xFF);
    assert_eq!(s.try_set_modem_state(modem), Ok(()));
    assert_eq!(drain(&mut s), sb(107, &[0xFF]));
    let line = LineState::from_bits(0xFF);
    assert_eq!(s.try_notify_line_state(line), Ok(()));
    assert_eq!(drain(&mut s), sb(106, &[0xFF]));
    assert_eq!(s.try_suspend_flow(), Ok(()));
    assert_eq!(drain(&mut s), sb(108, &[]));
    assert_eq!(s.try_resume_flow(), Ok(()));
    assert_eq!(drain(&mut s), sb(109, &[]));
    let taken = s.try_write_data(&[0xFF; 300]).expect("data fits");
    assert!(taken >= 61, "{taken}");
}

#[test]
fn server_worst_signature_reply_fits_the_reserve() {
    let signature = [0xFF; 64];
    let reply = sb(100, &signature);
    assert_eq!(reply.len(), 134);
    let mut s = active_server(Server::new(PortSettings::default()).with_signature(signature));

    // Fill the write budget: 134 bytes stay free.
    let mut filled = 0;
    while let Ok(n) = s.try_write_data(&[b'x'; 40]) {
        filled += n;
    }
    assert_eq!(filled, Server::OUTPUT_CAPACITY - 134);

    // A refusal (3 bytes) fits the negotiation reserve; then the signature
    // request waits before its final byte until 134 bytes are free.
    let p = s.receive_into(&hex("FF FD 01"), &mut [0; 4]);
    assert_eq!(p.consumed, 3);
    let request = sb(0, &[]);
    let p = s.receive_into(&request, &mut [0; 4]);
    assert_eq!(p.consumed, request.len() - 1);
    assert_eq!(
        s.receive_into(&request[p.consumed..], &mut [0; 4]).consumed,
        0
    );
    let mut wire = s.output()[..2].to_vec();
    s.consume_output(2);
    assert_eq!(
        s.receive_into(&request[p.consumed..], &mut [0; 4]).consumed,
        0
    );
    wire.push(s.output()[0]);
    s.consume_output(1);
    assert_eq!(
        s.receive_into(&request[p.consumed..], &mut [0; 4]).consumed,
        1
    );
    assert_eq!(s.output().len(), Server::OUTPUT_CAPACITY);

    // The reply is intact, after everything queued before it.
    wire.extend(drain(&mut s));
    let expected = [vec![b'x'; filled], hex("FF FC 01"), reply].concat();
    assert_eq!(wire, expected);
}

#[test]
fn with_signature_mid_session_never_corrupts_output() {
    let mut s = active_server(Server::new(PortSettings::default()));
    let p = s.receive_into(&sb(0, &[]), &mut [0; 4]);
    assert_eq!(p.consumed, 6);

    // Half of the reply leaves, one byte at a time; then the signature
    // changes.
    let mut wire = Vec::new();
    for _ in 0..10 {
        wire.push(s.output()[0]);
        s.consume_output(1);
    }
    let mut s = s.with_signature("x");
    wire.extend(drain(&mut s));
    let default = concat!("rfc2217 ", env!("CARGO_PKG_VERSION"));
    assert_eq!(
        wire,
        sb(100, default.as_bytes()),
        "exactly one complete reply"
    );

    // A later request is answered with the new signature.
    let p = s.receive_into(&sb(0, &[]), &mut [0; 4]);
    assert_eq!(p.consumed, 6);
    assert_eq!(drain(&mut s), sb(100, b"x"));
}

#[test]
fn output_slice_contract() {
    let mut c = Client::new(PortSettings::default());
    let p = c.receive_into(&hex("FF FD 2C FF FD 18"), &mut [0; 4]);
    assert_eq!(p.consumed, 3);
    assert_eq!(c.poll_event(), Some(Event::ComPortActive));
    let p = c.receive_into(&hex("FF FD 18"), &mut [0; 4]);
    assert_eq!(p.consumed, 3);
    assert_eq!(c.try_write_data(b"a\xFFb"), Ok(3));
    assert!(c.has_output());

    // Consuming one byte at a time yields the same bytes as taking them at
    // once, and output() is non-empty exactly while has_output() is true.
    let all = c.output().to_vec();
    let mut one_by_one = Vec::new();
    while c.has_output() {
        assert!(!c.output().is_empty());
        one_by_one.push(c.output()[0]);
        c.consume_output(1);
    }
    assert!(c.output().is_empty());
    assert_eq!(one_by_one, all);
    assert!(all.starts_with(&opening()));
    assert!(all.ends_with(&hex("FF FC 18 61 FF FF 62")));

    // consume_output(0) is allowed, also with nothing pending.
    c.consume_output(0);
    let mut s = Server::new(PortSettings::default());
    assert_eq!(drain(&mut s).len(), 12, "the opening");
    assert!(!s.has_output());
    assert!(s.output().is_empty());
    s.consume_output(0);
}

#[test]
#[should_panic(expected = "consume_output past the end of output()")]
fn consume_past_the_end_panics() {
    let mut c = Client::new(PortSettings::default());
    let n = c.output().len();
    c.consume_output(n + 1);
}

#[test]
fn command_error_messages() {
    assert_eq!(
        CommandError::OutputFull.to_string(),
        "output buffer full: write out pending output, then retry"
    );
    assert_eq!(
        CommandError::Negotiating.to_string(),
        "COM-PORT-OPTION is still being negotiated: retry once it is active"
    );
    assert_eq!(
        CommandError::Inactive.to_string(),
        "COM-PORT-OPTION is not active (refused, withdrawn, or not enabled by the peer)"
    );
}
