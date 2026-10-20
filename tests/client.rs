//! Client behaviour, through the public API and the `common` driver.

mod common;

use common::{Wire, activate, hex, replies, requests, sb, set};
use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, InlineBytes, LineState,
    ModemState, NotReady, Parity, PartialSettings, PortSettings, Purge, Setting, SettingSet,
    StopBits,
};

/// The wire codes of `PortSettings::default()` for `requests`/`replies`:
/// 8 data bits, no parity, 1 stop bit, no flow control.
const DEFAULT_CODES: [u8; 4] = [8, 1, 1, 1];

/// A new client whose opening has been drained.
fn client() -> Wire<Client> {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    let _ = c.out();
    c
}

/// An active client for `settings` whose opening, settings round and
/// `ComPortActive` have been drained: every setting awaits its reply.
fn active(settings: PortSettings) -> Wire<Client> {
    let mut c = Wire::new(Client::new(settings));
    activate(&mut c);
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));
    c
}

/// An active client whose default request (9600 8N1, no flow control) the
/// bridge has confirmed; its events have been drained.
fn ready() -> Wire<Client> {
    let mut c = active(PortSettings::default());
    assert!(c.feed(&replies(9600, DEFAULT_CODES)).is_empty());
    let _ = c.events();
    assert_eq!(c.readiness(), Ok(()));
    c
}

/// The replies to the default round except the one for flow control.
fn all_but_flow_control() -> Vec<u8> {
    [
        sb(101, &hex("00 00 25 80")),
        sb(102, &[8]),
        sb(103, &[1]),
        sb(104, &[1]),
    ]
    .concat()
}

/// All five settings.
fn all() -> SettingSet {
    set(&[
        Setting::BaudRate,
        Setting::DataBits,
        Setting::Parity,
        Setting::StopBits,
        Setting::FlowControl,
    ])
}

/// The `Unknown` event for a reply with this code and payload.
fn unknown(code: u8, payload: &[u8]) -> Event {
    Event::Unknown(InlineBytes::new(&[&[44, code][..], payload].concat()))
}

#[test]
fn initial_requests_are_unchanged() {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    assert_eq!(
        c.out(),
        hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C")
    );
    assert_eq!(c.com_port_state(), ComPortState::Negotiating);
    assert!(c.events().is_empty());
}

#[test]
fn client_refuses_do_ttype_naws_echo() {
    let mut c = client();
    for (request, refusal) in [
        ("FF FD 18", "FF FC 18"),
        ("FF FD 1F", "FF FC 1F"),
        ("FF FB 01", "FF FE 01"),
        ("FF FD 01", "FF FC 01"),
    ] {
        assert!(c.feed(&hex(request)).is_empty());
        assert_eq!(c.out(), hex(refusal), "answer to {request}");
    }
    assert_eq!(c.com_port_state(), ComPortState::Negotiating);
    assert!(c.events().is_empty());
}

#[test]
fn data_is_unescaped_around_refused_options() {
    let mut c = client();
    // "ok", an escaped IAC, an ECHO offer, "!".
    assert_eq!(c.feed(&hex("6F 6B FF FF FF FB 01 21")), hex("6F 6B FF 21"));
    assert_eq!(c.out(), hex("FF FE 01"));
    assert!(c.events().is_empty());
}

#[test]
fn iac_will_inside_sb_is_answered() {
    let mut c = client();
    activate(&mut c);
    // `IAC WILL ECHO` aborts the NOTIFY-LINESTATE subnegotiation it
    // interrupts, is refused, and the stream continues as data.
    assert_eq!(c.feed(&hex("FF FA 2C 6A FF FB 01 41")), b"A");
    assert_eq!(c.out(), hex("FF FE 01"));
    assert!(c.events().is_empty(), "no LineState from the partial body");
}

#[test]
fn empty_and_foreign_subnegotiations_are_ignored() {
    // Data around an empty subnegotiation, TTYPE SEND, NAWS 80x24 and a
    // COM-PORT-OPTION subnegotiation without a command.
    let input = [
        &b"x"[..],
        &hex("FF FA FF F0"),
        b"y",
        &hex("FF FA 18 01 FF F0"),
        &hex("FF FA 1F 00 50 00 18 FF F0"),
        &hex("FF FA 2C FF F0"),
        b"z",
    ]
    .concat();

    let mut c = client();
    assert_eq!(c.feed(&input), b"xyz");
    assert!(c.out().is_empty());
    assert!(c.events().is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Negotiating);

    activate(&mut c);
    assert_eq!(c.feed(&input), b"xyz");
    assert!(c.out().is_empty());
    assert!(c.events().is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Active);
}

#[test]
fn long_replies_are_kept_truncated_and_never_decode_as_values() {
    let mut c = client();
    activate(&mut c);
    // A baud-rate reply that starts like 9600 but carries 96 more bytes: its
    // true length rules out a value.
    let mut payload = vec![0x00, 0x00, 0x25, 0x80];
    payload.extend([0x00; 96]);
    assert!(c.feed(&sb(101, &payload)).is_empty());
    let body = [&[44, 101][..], &payload].concat();
    let events = c.events();
    let [Event::Unknown(kept)] = &events[..] else {
        panic!("expected one Unknown event")
    };
    assert_eq!(kept, &InlineBytes::new(&body));
    assert_eq!((kept.len(), kept.original_len()), (64, 102));
    assert_eq!(&kept[..], &body[..64]);
    // It still answers the baud rate request, as an undecodable value.
    assert_eq!(c.mismatched_settings(), set(&[Setting::BaudRate]));
    assert_eq!(c.reported_settings().baud_rate, None);

    // A signature keeps its first 64 bytes and its length.
    assert!(c.feed(&sb(100, &[b's'; 100])).is_empty());
    let events = c.events();
    let [Event::Signature(signature)] = &events[..] else {
        panic!("expected one Signature event")
    };
    assert_eq!(signature, &InlineBytes::new(&[b's'; 100]));
    assert_eq!((signature.len(), signature.original_len()), (64, 100));
}

#[test]
fn core_calls_wait_for_activation() {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    assert_eq!(c.try_set_dtr(false), Err(CommandError::Negotiating));
    assert_eq!(c.try_set_rts(true), Err(CommandError::Negotiating));
    assert_eq!(c.try_set_break(true), Err(CommandError::Negotiating));
    assert_eq!(c.try_purge(Purge::Both), Err(CommandError::Negotiating));
    assert_eq!(c.try_request_signature(), Err(CommandError::Negotiating));
    assert_eq!(c.try_write_data(b"X"), Err(CommandError::Negotiating));
    assert_eq!(
        c.out(),
        hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C"),
        "nothing but the opening"
    );

    // Changing the request writes nothing: the round at activation reads it.
    let seven_bits = PortSettings {
        data_bits: DataBits::Seven,
        ..PortSettings::default()
    };
    assert_eq!(c.try_set_port_settings(seven_bits), Ok(()));
    assert_eq!(c.requested_settings(), seven_bits);
    assert!(c.out().is_empty());
    assert!(c.events().is_empty());
}

#[test]
fn core_commands_after_refusal_are_inactive_but_data_flows() {
    let mut c = client();
    assert!(c.feed(&hex("FF FE 2C FF FC 2C")).is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Refused);
    assert_eq!(c.events(), vec![Event::ComPortRefused]);
    assert!(c.out().is_empty());

    assert_eq!(c.try_set_dtr(false), Err(CommandError::Inactive));
    assert_eq!(c.try_set_rts(true), Err(CommandError::Inactive));
    assert_eq!(c.try_set_break(true), Err(CommandError::Inactive));
    assert_eq!(c.try_purge(Purge::Receive), Err(CommandError::Inactive));
    assert_eq!(c.try_request_signature(), Err(CommandError::Inactive));
    assert!(c.out().is_empty(), "no subnegotiation for a refused option");

    // A refused client is a plain Telnet data channel.
    assert_eq!(c.try_write_data(b"A\xFF"), Ok(2));
    assert_eq!(c.out(), hex("41 FF FF"));
}

#[test]
fn set_port_settings_before_active_is_deferred_and_sent_once() {
    let first = PortSettings {
        baud_rate: 19_200,
        ..PortSettings::default()
    };
    let second = PortSettings {
        baud_rate: 57_600,
        parity: Parity::Odd,
        ..PortSettings::default()
    };
    let mut c = client();
    assert_eq!(c.try_set_port_settings(first), Ok(()));
    assert_eq!(c.try_set_port_settings(second), Ok(()));
    assert!(c.out().is_empty(), "nothing is sent while negotiating");
    assert_eq!(c.requested_settings(), second);
    assert_eq!(c.pending_settings(), all());

    // Activation sends one round, for the latest request; one set of
    // replies settles it, so no other round is in flight.
    assert!(c.feed(&hex("FF FD 2C")).is_empty());
    assert_eq!(c.events(), [Event::ComPortActive]);
    assert_eq!(c.out(), requests(57_600, [8, 2, 1, 1]));
    assert!(c.feed(&replies(57_600, [8, 2, 1, 1])).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    let _ = c.events();

    // While Refused the request waits the same way, for a re-activation.
    assert!(c.feed(&hex("FF FE 2C")).is_empty());
    assert_eq!(c.out(), hex("FF FC 2C"));
    assert_eq!(c.events(), [Event::ComPortRefused]);
    assert_eq!(c.try_set_port_settings(first), Ok(()));
    assert!(c.out().is_empty(), "nothing is sent while refused");
    assert_eq!(c.requested_settings(), first);
    assert!(c.feed(&hex("FF FD 2C")).is_empty());
    assert_eq!(c.events(), [Event::ComPortActive]);
    assert_eq!(
        c.out(),
        [hex("FF FB 2C"), requests(19_200, DEFAULT_CODES)].concat()
    );
    assert!(c.feed(&replies(19_200, DEFAULT_CODES)).is_empty());
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn set_port_settings_while_active_resends_all_five() {
    let mut c = ready();
    let settings = PortSettings {
        baud_rate: 57_600,
        data_bits: DataBits::Seven,
        parity: Parity::Odd,
        stop_bits: StopBits::OnePointFive,
        flow_control: FlowControl::XonXoff,
    };
    assert_eq!(c.try_set_port_settings(settings), Ok(()));
    assert_eq!(
        c.out(),
        hex(concat!(
            "FF FA 2C 01 00 00 E1 00 FF F0 FF FA 2C 02 07 FF F0 FF FA 2C 03 02 FF F0 ",
            "FF FA 2C 04 03 FF F0 FF FA 2C 05 02 FF F0"
        ))
    );
    assert_eq!(c.requested_settings(), settings);
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));

    // Changing one field sends all five again, and all five are awaited.
    let one_change = PortSettings {
        data_bits: DataBits::Eight,
        ..settings
    };
    assert!(c.feed(&replies(57_600, [7, 2, 3, 2])).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.try_set_port_settings(one_change), Ok(()));
    assert_eq!(
        c.out(),
        hex(concat!(
            "FF FA 2C 01 00 00 E1 00 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 02 FF F0 ",
            "FF FA 2C 04 03 FF F0 FF FA 2C 05 02 FF F0"
        ))
    );
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));
}

#[test]
fn exact_frames_for_every_command() {
    type Call = fn(&mut Client) -> Result<(), CommandError>;
    let calls: [(Call, &str); 10] = [
        (|c| c.try_set_rts(true), "FF FA 2C 05 0B FF F0"),
        (|c| c.try_set_rts(false), "FF FA 2C 05 0C FF F0"),
        (|c| c.try_set_break(true), "FF FA 2C 05 05 FF F0"),
        (|c| c.try_set_break(false), "FF FA 2C 05 06 FF F0"),
        (|c| c.try_set_dtr(true), "FF FA 2C 05 08 FF F0"),
        (|c| c.try_set_dtr(false), "FF FA 2C 05 09 FF F0"),
        (|c| c.try_purge(Purge::Receive), "FF FA 2C 0C 01 FF F0"),
        (|c| c.try_purge(Purge::Transmit), "FF FA 2C 0C 02 FF F0"),
        (|c| c.try_purge(Purge::Both), "FF FA 2C 0C 03 FF F0"),
        (Client::try_request_signature, "FF FA 2C 00 FF F0"),
    ];
    let mut c = ready();
    for (call, frame) in calls {
        assert_eq!(call(&mut c), Ok(()), "{frame}");
        assert_eq!(c.out(), hex(frame));
    }
    // Commands are not settings: readiness is unchanged.
    assert_eq!(c.readiness(), Ok(()));
    assert!(c.events().is_empty());
}

// ---- COM-PORT-OPTION lifecycle (L-rules) ---------------------------------

#[test]
fn peer_will_alone_does_not_activate() {
    let mut c = client();
    // The answer to our DO: the peer may perform the option, but our WILL
    // is still unanswered.
    assert!(c.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Negotiating);
    assert!(c.out().is_empty(), "no reply, no round");
    assert!(c.events().is_empty());
    assert_eq!(c.readiness(), Err(NotReady::Negotiating));
    assert_eq!(c.try_set_dtr(true), Err(CommandError::Negotiating));

    // Its withdrawal is acknowledged and changes nothing either.
    assert!(c.feed(&hex("FF FC 2C")).is_empty());
    assert_eq!(c.out(), hex("FF FE 2C"));
    assert_eq!(c.com_port_state(), ComPortState::Negotiating);
    assert!(c.events().is_empty());
}

#[test]
fn declined_will_is_refused_whatever_the_do_side_says() {
    // The peer accepts our DO but declines our WILL, in either order.
    for input in ["FF FB 2C FF FE 2C", "FF FE 2C FF FB 2C"] {
        let mut c = client();
        assert!(c.feed(&hex(input)).is_empty());
        assert_eq!(c.com_port_state(), ComPortState::Refused, "{input}");
        assert_eq!(c.events(), [Event::ComPortRefused], "{input}");
        assert!(c.out().is_empty(), "{input}");
        assert_eq!(c.readiness(), Err(NotReady::Refused));
    }
}

#[test]
fn refusal_is_detected() {
    let mut c = client();
    assert!(c.feed(&hex("FF FE 2C FF FC 2C")).is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Refused);
    assert_eq!(c.events(), [Event::ComPortRefused]);
    assert!(c.out().is_empty(), "answers to our requests need no reply");
}

#[test]
fn repeated_refusal_emits_once_even_after_drain() {
    let mut c = client();
    assert!(c.feed(&hex("FF FE 2C")).is_empty());
    assert_eq!(c.events(), [Event::ComPortRefused]);

    // More refusals after the event was taken: nothing.
    for input in ["FF FE 2C", "FF FC 2C", "FF FE 2C FF FC 2C FF FE 2C"] {
        assert!(c.feed(&hex(input)).is_empty());
        assert!(c.events().is_empty(), "{input}");
        assert!(c.out().is_empty(), "{input}");
    }
    // The peer enabling and withdrawing its own side is answered, and
    // changes nothing for ours.
    assert!(c.feed(&hex("FF FB 2C FF FC 2C")).is_empty());
    assert_eq!(c.out(), hex("FF FD 2C FF FE 2C"));
    assert!(c.events().is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Refused);

    // A peer that keeps enabling and withdrawing the option: each transition
    // is announced once, until the loop breaker refuses the ninth re-enable
    // (the first DO answers our WILL and does not count); from then on the
    // option stays refused and nothing more is announced.
    let mut c = client();
    let mut events = Vec::new();
    let mut out = Vec::new();
    for _ in 0..20 {
        assert!(c.feed(&hex("FF FD 2C FF FE 2C")).is_empty());
        events.extend(c.events());
        out.extend(c.out());
    }
    let round = requests(9600, DEFAULT_CODES);
    let mut expected_out = [round.clone(), hex("FF FC 2C")].concat();
    for _ in 0..8 {
        expected_out.extend([hex("FF FB 2C"), round.clone(), hex("FF FC 2C")].concat());
    }
    for _ in 0..11 {
        expected_out.extend(hex("FF FC 2C"));
    }
    assert_eq!(out, expected_out);
    let transitions = [Event::ComPortActive, Event::ComPortRefused];
    let expected_events: Vec<Event> = std::iter::repeat_n(transitions, 9).flatten().collect();
    assert_eq!(events, expected_events);
    assert_eq!(c.com_port_state(), ComPortState::Refused);
}

#[test]
fn com_port_withdrawn_and_reenabled() {
    // 19200 7E1: the request survives the withdrawal and is sent again.
    let wanted = PortSettings {
        baud_rate: 19_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        ..PortSettings::default()
    };
    let codes = [7, 3, 1, 1];
    let mut c = active(wanted);
    assert!(c.feed(&replies(19_200, codes)).is_empty());
    let _ = c.events();
    assert_eq!(c.readiness(), Ok(()));
    // While active the bridge sent a modem state and a suspend, and a
    // signature request of ours is unanswered.
    assert!(
        c.feed(&[sb(107, &[0x30]), sb(108, &[])].concat())
            .is_empty()
    );
    assert_eq!(c.try_request_signature(), Ok(()));
    let _ = c.out();
    let _ = c.events();
    assert_eq!(c.modem_state(), Some(ModemState::DSR | ModemState::CTS));
    assert!(c.flow_suspended());

    // The bridge withdraws COM-PORT-OPTION: acknowledged, announced, and
    // everything learned from the bridge is forgotten.
    assert!(c.feed(&hex("FF FE 2C")).is_empty());
    assert_eq!(c.out(), hex("FF FC 2C"));
    assert_eq!(c.events(), [Event::ComPortRefused]);
    assert_eq!(c.com_port_state(), ComPortState::Refused);
    assert_eq!(c.readiness(), Err(NotReady::Refused));
    assert_eq!(c.reported_settings(), PartialSettings::default());
    assert_eq!(c.modem_state(), None);
    assert!(!c.flow_suspended());
    assert_eq!(c.pending_settings(), all());
    assert!(c.mismatched_settings().is_empty());
    assert_eq!(c.requested_settings(), wanted);
    assert_eq!(c.try_set_dtr(false), Err(CommandError::Inactive));

    // Re-enabled: our WILL, then a fresh round; pending until answered.
    assert!(c.feed(&hex("FF FD 2C")).is_empty());
    assert_eq!(c.out(), [hex("FF FB 2C"), requests(19_200, codes)].concat());
    assert_eq!(c.events(), [Event::ComPortActive]);
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));

    // The unanswered signature request was forgotten too: an empty
    // SIGNATURE now is the server asking for ours.
    assert!(c.feed(&sb(100, &[])).is_empty());
    assert!(c.events().is_empty());

    assert!(c.feed(&replies(19_200, codes)).is_empty());
    assert_eq!(c.events().len(), 5);
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().complete(), Some(wanted));
}

#[test]
fn client_processes_com_port_sbs_in_any_state() {
    let input = [
        sb(101, &hex("00 00 4B 00")),
        sb(106, &[0x10]),
        sb(107, &[0x30]),
        sb(108, &[]),
        sb(100, b"sig"),
        sb(112, &[2]),
    ]
    .concat();
    let expected = [
        Event::BaudRate(19_200),
        Event::LineState(LineState::BREAK_DETECT),
        Event::ModemState(ModemState::DSR | ModemState::CTS),
        Event::FlowSuspended,
        Event::Signature(InlineBytes::new(b"sig")),
        Event::Purged(Purge::Transmit),
    ];

    let negotiating = client();
    let mut active = client();
    activate(&mut active);
    let mut refused = client();
    assert!(refused.feed(&hex("FF FE 2C")).is_empty());
    assert_eq!(refused.events(), [Event::ComPortRefused]);

    for (mut c, state) in [
        (negotiating, ComPortState::Negotiating),
        (active, ComPortState::Active),
        (refused, ComPortState::Refused),
    ] {
        assert!(c.feed(&input).is_empty());
        assert_eq!(c.events(), expected, "{state:?}");
        assert!(c.out().is_empty(), "{state:?}");
        assert_eq!(c.com_port_state(), state);
        assert_eq!(c.reported_settings().baud_rate, Some(19_200), "{state:?}");
        assert_eq!(
            c.modem_state(),
            Some(ModemState::DSR | ModemState::CTS),
            "{state:?}"
        );
        assert!(c.flow_suspended(), "{state:?}");
        if state == ComPortState::Negotiating {
            // Activation keeps what was learned before it; only a refusal
            // clears it.
            assert!(c.feed(&hex("FF FD 2C")).is_empty());
            assert_eq!(c.events(), [Event::ComPortActive]);
            assert_eq!(c.reported_settings().baud_rate, Some(19_200));
            assert_eq!(c.modem_state(), Some(ModemState::DSR | ModemState::CTS));
            assert!(c.flow_suspended());
        }
    }
}

#[test]
fn echo_peer_goes_active_and_stays_pending() {
    // A peer that sends back whatever it receives.
    let mut c = Wire::new(Client::new(PortSettings::default()));
    for _ in 0..6 {
        let echo = c.out();
        assert!(c.feed(&echo).is_empty());
    }
    assert_eq!(c.com_port_state(), ComPortState::Active);
    // The echoed round is five client-direction commands: reported, never
    // taken as replies.
    assert_eq!(
        c.events(),
        [
            Event::ComPortActive,
            unknown(1, &hex("00 00 25 80")),
            unknown(2, &[8]),
            unknown(3, &[1]),
            unknown(4, &[1]),
            unknown(5, &[1]),
        ]
    );
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));
    assert!(c.out().is_empty(), "nothing left to say");
}

// ---- decoding of server replies (D-rules) --------------------------------

#[test]
fn client_decodes_every_server_reply() {
    // Each input runs on a fresh active client: (input, exact events).
    let mut rows: Vec<(Vec<u8>, Vec<Event>)> = Vec::new();
    // One subnegotiation; `None` means it decodes as `Unknown` of its body.
    let mut row = |code: u8, payload: &[u8], event: Option<Event>| {
        let event = event.unwrap_or_else(|| unknown(code, payload));
        rows.push((sb(code, payload), vec![event]));
    };

    // 100 SIGNATURE: raw bytes, never decoded as text.
    row(
        100,
        b"ser2net",
        Some(Event::Signature(InlineBytes::new(b"ser2net"))),
    );
    let raw = [0xFF, 0x00, 0xE9];
    row(100, &raw, Some(Event::Signature(InlineBytes::new(&raw))));

    // 101 SET-BAUDRATE: 4 bytes big-endian, or sredird's 8 bytes with one
    // zero half; 0 and every other length are undecodable.
    for (payload, rate) in [
        ("00 00 25 80", 9600),
        ("00 0E 10 00", 921_600),
        ("FF FF FF FF", u32::MAX),
        ("00 00 25 80 00 00 00 00", 9600),
        ("00 00 00 00 00 00 25 80", 9600),
    ] {
        row(101, &hex(payload), Some(Event::BaudRate(rate)));
    }
    for payload in [
        "",
        "25 80",
        "00 25 80",
        "00 00 00 00",
        "00 00 25 80 00",
        "00 00 25 80 00 00 25 80",
        "00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 25 80",
    ] {
        row(101, &hex(payload), None);
    }

    // 102-104: exactly one byte with a valid code.
    for (code, bits) in [
        (5, DataBits::Five),
        (6, DataBits::Six),
        (7, DataBits::Seven),
        (8, DataBits::Eight),
    ] {
        row(102, &[code], Some(Event::DataBits(bits)));
    }
    for (code, parity) in [
        (1, Parity::None),
        (2, Parity::Odd),
        (3, Parity::Even),
        (4, Parity::Mark),
        (5, Parity::Space),
    ] {
        row(103, &[code], Some(Event::Parity(parity)));
    }
    for (code, bits) in [
        (1, StopBits::One),
        (2, StopBits::Two),
        (3, StopBits::OnePointFive),
    ] {
        row(104, &[code], Some(Event::StopBits(bits)));
    }
    for (code, invalid) in [
        (102, [0, 4, 9, 0xFF]),
        (103, [0, 6, 0x10, 0xFF]),
        (104, [0, 4, 5, 0xFF]),
    ] {
        for value in invalid {
            row(code, &[value], None);
        }
        row(code, &[], None);
        row(code, &[1, 1], None);
    }

    // 105 SET-CONTROL: classified by value alone.
    for (value, flow) in [
        (1, FlowControl::None),
        (2, FlowControl::XonXoff),
        (3, FlowControl::RtsCts),
        (17, FlowControl::Dcd),
        (19, FlowControl::Dsr),
    ] {
        row(105, &[value], Some(Event::FlowControl(flow)));
    }
    for (value, event) in [
        (5, Event::Break(true)),
        (6, Event::Break(false)),
        (8, Event::Dtr(true)),
        (9, Event::Dtr(false)),
        (11, Event::Rts(true)),
        (12, Event::Rts(false)),
    ] {
        row(105, &[value], Some(event));
    }
    for value in [0, 4, 7, 10, 13, 14, 15, 16, 18, 20, 0x7F, 0xFF] {
        row(105, &[value], None);
    }
    row(105, &[], None);
    row(105, &[1, 1], None);

    // 106/107 NOTIFY-LINESTATE/-MODEMSTATE: one byte, as received.
    for value in [0x00, 0x11, 0xFF] {
        row(
            106,
            &[value],
            Some(Event::LineState(LineState::from_bits(value))),
        );
        row(
            107,
            &[value],
            Some(Event::ModemState(ModemState::from_bits(value))),
        );
    }
    for code in [106, 107] {
        row(code, &[], None);
        row(code, &[0x30, 0x30], None);
    }

    // 112 PURGE-DATA: one valid purge code.
    for (value, purge) in [(1, Purge::Receive), (2, Purge::Transmit), (3, Purge::Both)] {
        row(112, &[value], Some(Event::Purged(purge)));
    }
    for payload in [&[0][..], &[4], &[], &[1, 1]] {
        row(112, payload, None);
    }

    // Unknown server codes, and client-direction commands (an echoing peer).
    for code in [113, 150, 255, 1, 99] {
        row(code, &[], None);
        row(code, &[0x00, 0x00, 0x25, 0x80], None);
    }

    // 108/109 FLOWCONTROL-SUSPEND/-RESUME with any payload (a resume after a
    // suspend); 110/111 mask acknowledgements produce nothing.
    rows.push((sb(108, &[]), vec![Event::FlowSuspended]));
    rows.push((
        [sb(108, &[0x55]), sb(109, &[1, 2])].concat(),
        vec![Event::FlowSuspended, Event::FlowResumed],
    ));
    for code in [110, 111] {
        rows.push((
            [sb(code, &[0xFF]), sb(code, &[]), sb(code, &[1, 2])].concat(),
            vec![],
        ));
    }
    // With no request of ours outstanding, an empty 100 is the server asking
    // for our signature; a lone 109 resumes nothing. Neither is an event.
    rows.push((sb(100, &[]), vec![]));
    rows.push((sb(109, &[]), vec![]));

    for (input, expected) in rows {
        let mut c = active(PortSettings::default());
        assert!(c.feed(&input).is_empty(), "{input:02X?}");
        assert_eq!(c.events(), expected, "{input:02X?}");
        assert!(c.out().is_empty(), "no answer to {input:02X?}");
    }
}

#[test]
fn eight_byte_baud_little_and_big_endian_halves() {
    // sredird on a little-endian LP64 host: the rate, then four zero bytes.
    let mut c = active(PortSettings::default());
    assert!(c.feed(&sb(101, &hex("00 00 25 80 00 00 00 00"))).is_empty());
    assert_eq!(c.events(), [Event::BaudRate(9600)]);
    let rest = [sb(102, &[8]), sb(103, &[1]), sb(104, &[1]), sb(105, &[1])].concat();
    assert!(c.feed(&rest).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    let _ = c.events();

    // On a big-endian host: four zero bytes, then the rate.
    let faster = PortSettings {
        baud_rate: 19_200,
        ..PortSettings::default()
    };
    assert_eq!(c.try_set_port_settings(faster), Ok(()));
    assert_eq!(c.out(), requests(19_200, DEFAULT_CODES));
    assert!(c.feed(&sb(101, &hex("00 00 00 00 00 00 4B 00"))).is_empty());
    assert_eq!(c.events(), [Event::BaudRate(19_200)]);
    assert!(c.feed(&rest).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().baud_rate, Some(19_200));
    let _ = c.events();

    // Neither half zero, or both: undecodable.
    for payload in ["00 00 4B 00 00 00 4B 00", "00 00 00 00 00 00 00 00"] {
        let payload = hex(payload);
        assert!(c.feed(&sb(101, &payload)).is_empty());
        assert_eq!(c.events(), [unknown(101, &payload)]);
        assert_eq!(c.reported_settings().baud_rate, None);
        assert_eq!(
            c.readiness(),
            Err(NotReady::Mismatched(set(&[Setting::BaudRate])))
        );
    }
}

#[test]
fn zero_baud_reply_is_undecodable() {
    let mut c = active(PortSettings::default());
    assert!(c.feed(&replies(0, DEFAULT_CODES)).is_empty());
    assert_eq!(
        c.events(),
        [
            unknown(101, &[0; 4]),
            Event::DataBits(DataBits::Eight),
            Event::Parity(Parity::None),
            Event::StopBits(StopBits::One),
            Event::FlowControl(FlowControl::None),
        ]
    );
    assert_eq!(c.reported_settings().baud_rate, None);
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::BaudRate])))
    );

    // Not even the answer to a query: 0 is not a rate.
    let mut c = active(PortSettings {
        baud_rate: 0,
        ..PortSettings::default()
    });
    assert!(c.feed(&replies(0, DEFAULT_CODES)).is_empty());
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::BaudRate])))
    );
}

#[test]
fn long_unknown_body_keeps_its_length() {
    let mut c = active(PortSettings::default());
    assert!(c.feed(&sb(113, &[0x41; 100])).is_empty());
    let events = c.events();
    let [Event::Unknown(body)] = &events[..] else {
        panic!("expected one Unknown event, got {events:?}")
    };
    let expected = [&[44, 113][..], &[0x41; 62]].concat();
    assert_eq!(body.as_bytes(), &expected[..]);
    assert_eq!(body.len(), 64);
    assert_eq!(body.original_len(), 102);
    assert!(body.is_truncated());
}

// ---- settings accounting and readiness (R-rules) -------------------------

#[test]
fn settings_follow_activation_and_all_replies_are_decoded() {
    let settings = PortSettings {
        baud_rate: 115_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::RtsCts,
    };
    let mut c = Wire::new(Client::new(settings));
    let _ = c.out();
    assert_eq!(c.readiness(), Err(NotReady::Negotiating));
    assert_eq!(c.pending_settings(), all());

    // The round follows the activating DO COM-PORT-OPTION at once.
    assert!(c.feed(&hex("FF FD 2C")).is_empty());
    assert_eq!(c.events(), [Event::ComPortActive]);
    assert_eq!(
        c.out(),
        hex(concat!(
            "FF FA 2C 01 00 01 C2 00 FF F0 FF FA 2C 02 07 FF F0 FF FA 2C 03 03 FF F0 ",
            "FF FA 2C 04 02 FF F0 FF FA 2C 05 03 FF F0"
        ))
    );
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));
    assert!(c.mismatched_settings().is_empty());
    assert_eq!(c.reported_settings(), PartialSettings::default());

    // Each reply is decoded and settles its setting.
    let mut waiting = vec![
        Setting::BaudRate,
        Setting::DataBits,
        Setting::Parity,
        Setting::StopBits,
        Setting::FlowControl,
    ];
    for (reply, event) in [
        (sb(101, &hex("00 01 C2 00")), Event::BaudRate(115_200)),
        (sb(102, &[7]), Event::DataBits(DataBits::Seven)),
        (sb(103, &[3]), Event::Parity(Parity::Even)),
        (sb(104, &[2]), Event::StopBits(StopBits::Two)),
        (sb(105, &[3]), Event::FlowControl(FlowControl::RtsCts)),
    ] {
        assert!(c.feed(&reply).is_empty());
        assert_eq!(c.events(), [event]);
        waiting.remove(0);
        assert_eq!(c.pending_settings(), set(&waiting));
    }
    assert_eq!(c.readiness(), Ok(()));
    assert!(c.is_ready());
    assert_eq!(c.reported_settings().complete(), Some(settings));
    assert_eq!(c.requested_settings(), settings);
    assert!(c.out().is_empty());
}

#[test]
fn different_value_is_mismatched_not_ready() {
    let settings = PortSettings {
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        ..PortSettings::default()
    };
    let mut c = active(settings);
    // sredird on a pty acknowledges 8N for a 7E request.
    assert!(c.feed(&replies(9600, [8, 1, 2, 1])).is_empty());
    assert_eq!(
        c.events(),
        [
            Event::BaudRate(9600),
            Event::DataBits(DataBits::Eight),
            Event::Parity(Parity::None),
            Event::StopBits(StopBits::Two),
            Event::FlowControl(FlowControl::None),
        ]
    );
    let wrong = set(&[Setting::DataBits, Setting::Parity]);
    assert_eq!(c.readiness(), Err(NotReady::Mismatched(wrong)));
    assert!(!c.is_ready());
    assert_eq!(c.mismatched_settings(), wrong);
    assert!(c.pending_settings().is_empty());
    let reported = c.reported_settings();
    assert_eq!(reported.data_bits, Some(DataBits::Eight));
    assert_eq!(reported.parity, Some(Parity::None));
    assert_eq!(reported.stop_bits, Some(StopBits::Two));
    assert_eq!(c.requested_settings(), settings);
}

#[test]
fn undecodable_value_is_mismatched() {
    let mut c = active(PortSettings::default());
    assert!(c.feed(&replies(9600, [9, 1, 1, 1])).is_empty());
    assert_eq!(c.events()[1], unknown(102, &[9]));
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::DataBits])))
    );
    assert_eq!(c.reported_settings().data_bits, None);

    // A reply of another length is undecodable too, and clears the value
    // reported before.
    assert_eq!(c.reported_settings().parity, Some(Parity::None));
    assert!(c.feed(&sb(103, &[1, 1])).is_empty());
    assert_eq!(c.events(), [unknown(103, &[1, 1])]);
    assert_eq!(c.reported_settings().parity, None);
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[
            Setting::DataBits,
            Setting::Parity
        ])))
    );
}

#[test]
fn stale_reply_does_not_confirm_a_newer_request() {
    let mut c = active(PortSettings::default());
    let faster = PortSettings {
        baud_rate: 19_200,
        ..PortSettings::default()
    };
    assert_eq!(c.try_set_port_settings(faster), Ok(()));
    assert_eq!(c.out(), requests(19_200, DEFAULT_CODES));

    // The answers to the first round settle the first round only.
    assert!(c.feed(&replies(9600, DEFAULT_CODES)).is_empty());
    assert_eq!(c.reported_settings().baud_rate, Some(9600));
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));
    assert!(c.mismatched_settings().is_empty());

    assert!(c.feed(&replies(19_200, DEFAULT_CODES)).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().baud_rate, Some(19_200));
}

#[test]
fn skipped_ack_then_fallback_becomes_ready() {
    // ser2net does not answer a request for 1.5 stop bits.
    let mut c = active(PortSettings {
        stop_bits: StopBits::OnePointFive,
        ..PortSettings::default()
    });
    let skipped = [
        sb(101, &hex("00 00 25 80")),
        sb(102, &[8]),
        sb(103, &[1]),
        sb(105, &[1]),
    ]
    .concat();
    assert!(c.feed(&skipped).is_empty());
    assert_eq!(
        c.readiness(),
        Err(NotReady::Pending(set(&[Setting::StopBits])))
    );

    // Falling back to 1 stop bit: its answer settles the skipped request too.
    assert_eq!(c.try_set_port_settings(PortSettings::default()), Ok(()));
    assert_eq!(c.out(), requests(9600, DEFAULT_CODES));
    assert!(c.feed(&replies(9600, DEFAULT_CODES)).is_empty());
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn overlapping_aba_reconfiguration_has_no_transient() {
    let a = PortSettings::default();
    let b = PortSettings {
        baud_rate: 19_200,
        ..a
    };
    let mut c = active(a);
    assert_eq!(c.try_set_port_settings(b), Ok(()));
    assert_eq!(c.try_set_port_settings(a), Ok(()));
    assert_eq!(
        c.out(),
        [
            requests(19_200, DEFAULT_CODES),
            requests(9600, DEFAULT_CODES)
        ]
        .concat()
    );

    // The bridge answers the three rounds in order, one reply at a time:
    // nothing reads as mismatched in between, and only the very last reply
    // makes the client ready.
    let mut answers = Vec::new();
    for baud in [9600_u32, 19_200, 9600] {
        answers.push(sb(101, &baud.to_be_bytes()));
        for (code, value) in [(102, 8), (103, 1), (104, 1), (105, 1)] {
            answers.push(sb(code, &[value]));
        }
    }
    let last = answers.len() - 1;
    for (i, answer) in answers.iter().enumerate() {
        assert!(c.feed(answer).is_empty());
        assert!(c.mismatched_settings().is_empty(), "after reply {i}");
        assert_eq!(c.is_ready(), i == last, "after reply {i}");
    }
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn skipped_then_substituted_stays_pending() {
    let mut c = ready();
    for stop_bits in [StopBits::OnePointFive, StopBits::Two] {
        let settings = PortSettings {
            stop_bits,
            ..PortSettings::default()
        };
        assert_eq!(c.try_set_port_settings(settings), Ok(()));
    }
    let _ = c.out();

    // The bridge skips its reply to 1.5 stop bits, then answers the request
    // for 2 with 1 (a value it substituted). That reply answers the oldest
    // request, so the request for 2 stays pending: a documented limit.
    let skipped = [
        sb(101, &hex("00 00 25 80")),
        sb(102, &[8]),
        sb(103, &[1]),
        sb(105, &[1]),
    ]
    .concat();
    assert!(c.feed(&skipped).is_empty());
    assert!(c.feed(&replies(9600, DEFAULT_CODES)).is_empty());
    assert_eq!(
        c.readiness(),
        Err(NotReady::Pending(set(&[Setting::StopBits])))
    );
    assert_eq!(c.reported_settings().stop_bits, Some(StopBits::One));
    assert!(c.mismatched_settings().is_empty());

    // A further request settles it.
    assert_eq!(c.try_set_port_settings(PortSettings::default()), Ok(()));
    let _ = c.out();
    assert!(c.feed(&replies(9600, DEFAULT_CODES)).is_empty());
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn baud_zero_is_a_query_confirmed_by_any_reply() {
    let query = PortSettings {
        baud_rate: 0,
        ..PortSettings::default()
    };
    let mut c = Wire::new(Client::new(query));
    let _ = c.out();
    assert!(c.feed(&hex("FF FD 2C")).is_empty());
    assert_eq!(c.events(), [Event::ComPortActive]);
    // SET-BAUDRATE 0 asks for the current rate.
    assert_eq!(c.out(), requests(0, DEFAULT_CODES));
    assert!(c.out().is_empty());

    assert!(c.feed(&replies(38_400, DEFAULT_CODES)).is_empty());
    assert_eq!(c.events()[0], Event::BaudRate(38_400));
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().baud_rate, Some(38_400));
    assert_eq!(c.requested_settings().baud_rate, 0);

    // Any later report keeps it confirmed.
    assert!(c.feed(&sb(101, &57_600u32.to_be_bytes())).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().baud_rate, Some(57_600));

    // A query queued behind a request: one reply settles both.
    let mut c = active(PortSettings::default());
    assert_eq!(c.try_set_port_settings(query), Ok(()));
    let _ = c.out();
    assert!(c.feed(&replies(38_400, DEFAULT_CODES)).is_empty());
    assert_eq!(
        c.pending_settings(),
        set(&[
            Setting::DataBits,
            Setting::Parity,
            Setting::StopBits,
            Setting::FlowControl
        ])
    );
    assert!(c.mismatched_settings().is_empty());
}

#[test]
fn unsolicited_reply_can_unconfirm() {
    let mut c = ready();
    assert!(c.feed(&sb(102, &[7])).is_empty());
    assert_eq!(c.events(), [Event::DataBits(DataBits::Seven)]);
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::DataBits])))
    );
    assert_eq!(c.reported_settings().data_bits, Some(DataBits::Seven));
    assert!(c.feed(&sb(102, &[8])).is_empty());
    assert_eq!(c.readiness(), Ok(()));

    // An undecodable report as well.
    assert!(c.feed(&sb(104, &[9])).is_empty());
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::StopBits])))
    );
    assert!(c.feed(&sb(104, &[1])).is_empty());
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn queue_evicts_oldest_after_eight() {
    // Eight rounds after activation's, asking for 2400, 3600, ..., 10800 baud:
    // nine in flight.
    let eight_more_rounds = |c: &mut Wire<Client>| {
        for i in 2..10 {
            let settings = PortSettings {
                baud_rate: 1200 * i,
                ..PortSettings::default()
            };
            assert_eq!(c.try_set_port_settings(settings), Ok(()));
            let _ = c.out();
        }
    };

    // Each setting keeps its eight newest requests. Replies with values never
    // requested each answer the oldest request: seven leave one request per
    // setting, the eighth none.
    let mut c = active(PortSettings::default());
    eight_more_rounds(&mut c);
    let unrequested = replies(110, [5, 5, 3, 2]);
    for _ in 0..7 {
        assert!(c.feed(&unrequested).is_empty());
    }
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));
    assert!(c.feed(&unrequested).is_empty());
    assert_eq!(c.readiness(), Err(NotReady::Mismatched(all())));
    assert!(c.pending_settings().is_empty());

    // Reports of the requested values confirm them.
    assert!(c.feed(&replies(10_800, DEFAULT_CODES)).is_empty());
    assert_eq!(c.readiness(), Ok(()));

    // The new request is kept, and unrequested replies answer the oldest
    // request left: after six of them the baud queue holds the eighth round's
    // 9600 and the ninth's 10800, so a report of 10800 settles both.
    let mut c = active(PortSettings::default());
    eight_more_rounds(&mut c);
    for _ in 0..6 {
        assert!(c.feed(&sb(101, &110u32.to_be_bytes())).is_empty());
    }
    assert!(c.pending_settings().contains(Setting::BaudRate));
    assert!(c.feed(&sb(101, &10_800u32.to_be_bytes())).is_empty());
    assert!(!c.pending_settings().contains(Setting::BaudRate));
    assert!(!c.mismatched_settings().contains(Setting::BaudRate));

    // The oldest request is the one evicted. With distinct rates 1200, 2400,
    // ..., 10800 the ninth round evicts 1200, so reports of 2400 to 9600 each
    // settle only the oldest request and leave 10800 for its own report. Had
    // 1200 stayed in place of any request from 3600 to 10800, the report of
    // 2400 would settle it too and the queue would empty early. (In place of
    // 2400 it goes unnoticed here: 1200 matches no report, and an unmatched
    // reply settles the oldest request, so both leave the same queue. The
    // query below catches it.)
    let mut c = active(PortSettings {
        baud_rate: 1200,
        ..PortSettings::default()
    });
    eight_more_rounds(&mut c);
    for i in 2..9u32 {
        assert!(c.feed(&sb(101, &(1200 * i).to_be_bytes())).is_empty());
    }
    assert!(c.pending_settings().contains(Setting::BaudRate));
    assert!(c.feed(&sb(101, &10_800u32.to_be_bytes())).is_empty());
    assert!(!c.pending_settings().contains(Setting::BaudRate));
    assert!(!c.mismatched_settings().contains(Setting::BaudRate));

    // An evicted query is the one entry whose value a reply can observe:
    // with the first round a query (0), the ninth round evicts it, so a
    // report of 9600 settles up to 9600 and leaves 10800. Had the query
    // stayed at the head (the second-oldest evicted instead), it would match
    // that report first and leave seven requests.
    let mut c = active(PortSettings {
        baud_rate: 0,
        ..PortSettings::default()
    });
    eight_more_rounds(&mut c);
    assert!(c.feed(&sb(101, &9600u32.to_be_bytes())).is_empty());
    assert!(c.pending_settings().contains(Setting::BaudRate));
    assert!(c.feed(&sb(101, &110u32.to_be_bytes())).is_empty());
    assert!(!c.pending_settings().contains(Setting::BaudRate));
}

#[test]
fn readiness_precedence() {
    // Negotiating first, whatever the bridge reported (replies are decoded
    // before activation).
    let mut c = client();
    assert!(c.feed(&sb(102, &[7])).is_empty());
    assert_eq!(c.events(), [Event::DataBits(DataBits::Seven)]);
    assert_eq!(c.readiness(), Err(NotReady::Negotiating));
    assert_eq!(c.pending_settings(), all());
    assert!(c.mismatched_settings().is_empty());
    assert!(!c.is_ready());

    // Mismatched before Pending.
    let mut c = active(PortSettings::default());
    assert!(c.feed(&sb(102, &[7])).is_empty());
    assert_eq!(
        c.pending_settings(),
        set(&[
            Setting::BaudRate,
            Setting::Parity,
            Setting::StopBits,
            Setting::FlowControl
        ])
    );
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::DataBits])))
    );
    assert!(!c.is_ready());

    // Refused before Mismatched.
    assert!(c.feed(&hex("FF FE 2C FF FC 2C")).is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Refused);
    assert_eq!(c.readiness(), Err(NotReady::Refused));
    assert!(!c.is_ready());
}

#[test]
fn readiness_messages() {
    for (reason, message) in [
        (
            NotReady::Negotiating,
            "no answer to the RFC 2217 COM-PORT-OPTION offer (is this the bridge's raw TCP port?)",
        ),
        (
            NotReady::Refused,
            "the peer refused RFC 2217 COM-PORT-OPTION (is it a plain Telnet server?)",
        ),
        (
            NotReady::Mismatched(set(&[Setting::DataBits, Setting::Parity])),
            "the bridge reported different values for: data bits, parity",
        ),
        (
            NotReady::Pending(set(&[Setting::StopBits])),
            "no answer from the bridge yet for: stop bits",
        ),
        (
            NotReady::Pending(all()),
            "no answer from the bridge yet for: baud rate, data bits, parity, stop bits, flow control",
        ),
    ] {
        assert_eq!(reason.to_string(), message);
        let error: &dyn core::error::Error = &reason;
        assert_eq!(error.to_string(), message);
    }

    let c = active(PortSettings::default());
    let error = c.readiness().unwrap_err();
    assert_eq!(
        error.to_string(),
        "no answer from the bridge yet for: baud rate, data bits, parity, stop bits, flow control"
    );
}

#[test]
fn reported_settings_complete_after_ready() {
    let settings = PortSettings {
        baud_rate: 115_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::RtsCts,
    };
    let mut c = active(settings);
    assert_eq!(c.reported_settings(), PartialSettings::default());
    assert_eq!(c.reported_settings().complete(), None);
    assert!(c.feed(&replies(115_200, [7, 3, 2, 3])).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().complete(), Some(settings));
    assert_eq!(c.requested_settings(), settings);
}

#[test]
fn round_that_does_not_fit_changes_nothing() {
    let mut c = ready();
    // Fill the write budget with data.
    let mut written = Vec::new();
    loop {
        match c.try_write_data(&[0x55; 64]) {
            Ok(n) => written.extend_from_slice(&[0x55; 64][..n]),
            Err(error) => {
                assert_eq!(error, CommandError::OutputFull);
                break;
            }
        }
    }

    let other = PortSettings {
        baud_rate: 19_200,
        parity: Parity::Odd,
        ..PortSettings::default()
    };
    assert_eq!(
        c.try_set_port_settings(other),
        Err(CommandError::OutputFull)
    );
    assert_eq!(c.requested_settings(), PortSettings::default());
    assert!(c.pending_settings().is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.out(), written, "nothing but the data");

    // After draining it fits, and it is the only round in flight: one set
    // of replies settles it.
    assert_eq!(c.try_set_port_settings(other), Ok(()));
    assert_eq!(c.out(), requests(19_200, [8, 2, 1, 1]));
    assert_eq!(c.requested_settings(), other);
    assert_eq!(c.readiness(), Err(NotReady::Pending(all())));
    assert!(c.feed(&replies(19_200, [8, 2, 1, 1])).is_empty());
    assert_eq!(c.readiness(), Ok(()));
}

// ---- SET-CONTROL replies (K-rules) ----------------------------------------

#[test]
fn dtr_rts_break_echoes_do_not_ack_flow_control() {
    let mut c = active(PortSettings::default());
    assert!(c.feed(&all_but_flow_control()).is_empty());
    let _ = c.events();

    let echoes = [8, 9, 11, 12, 5, 6]
        .iter()
        .flat_map(|&value| sb(105, &[value]))
        .collect::<Vec<u8>>();
    assert!(c.feed(&echoes).is_empty());
    assert_eq!(
        c.events(),
        [
            Event::Dtr(true),
            Event::Dtr(false),
            Event::Rts(true),
            Event::Rts(false),
            Event::Break(true),
            Event::Break(false),
        ]
    );
    assert_eq!(
        c.readiness(),
        Err(NotReady::Pending(set(&[Setting::FlowControl])))
    );
    assert_eq!(c.reported_settings().flow_control, None);

    assert!(c.feed(&sb(105, &[1])).is_empty());
    assert_eq!(c.events(), [Event::FlowControl(FlowControl::None)]);
    assert_eq!(c.readiness(), Ok(()));

    // Once ready, they change nothing either.
    assert!(c.feed(&echoes).is_empty());
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn inbound_flow_reply_to_outbound_request_is_mismatch() {
    let mut c = active(PortSettings {
        flow_control: FlowControl::Dcd,
        ..PortSettings::default()
    });
    // ser2net 4.3.11 answers DCD flow control with 14 (inbound: none).
    assert!(c.feed(&replies(9600, [8, 1, 1, 14])).is_empty());
    assert_eq!(c.events().last(), Some(&unknown(105, &[14])));
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::FlowControl])))
    );
    assert_eq!(c.reported_settings().flow_control, None);

    // With no flow control request outstanding, such a value is only
    // reported.
    let mut c = ready();
    assert!(c.feed(&sb(105, &[14])).is_empty());
    assert_eq!(c.events(), [unknown(105, &[14])]);
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().flow_control, Some(FlowControl::None));
}

#[test]
fn sredird_dtr_reply_is_reported_as_flow_control() {
    let mut c = ready();
    assert_eq!(c.try_set_dtr(false), Ok(()));
    assert_eq!(c.out(), sb(5, &[9]));
    // sredird answers a DTR change with its flow control value.
    assert!(c.feed(&sb(105, &[1])).is_empty());
    assert_eq!(c.events(), [Event::FlowControl(FlowControl::None)]);
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn sredird_dtr_before_flow_change_is_transient() {
    let mut c = ready();
    let rts_cts = PortSettings {
        flow_control: FlowControl::RtsCts,
        ..PortSettings::default()
    };
    assert_eq!(c.try_set_dtr(true), Ok(()));
    assert_eq!(c.try_set_port_settings(rts_cts), Ok(()));
    assert_eq!(
        c.out(),
        [sb(5, &[8]), requests(9600, [8, 1, 1, 3])].concat()
    );

    // sredird answers the DTR change with the flow value still in effect.
    // It answers the flow control request, as a mismatch, until the real
    // answer arrives.
    assert!(c.feed(&sb(105, &[1])).is_empty());
    assert_eq!(c.events(), [Event::FlowControl(FlowControl::None)]);
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::FlowControl])))
    );
    assert!(c.feed(&all_but_flow_control()).is_empty());
    assert_eq!(
        c.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::FlowControl])))
    );
    assert!(c.feed(&sb(105, &[3])).is_empty());
    assert_eq!(
        c.events().last(),
        Some(&Event::FlowControl(FlowControl::RtsCts))
    );
    assert_eq!(c.readiness(), Ok(()));
}

#[test]
fn control_reply_with_wrong_length_is_unknown() {
    let mut c = active(PortSettings::default());
    assert!(c.feed(&all_but_flow_control()).is_empty());
    let _ = c.events();

    // Not one value byte: reported, never accounted.
    let long = [1; 80];
    for payload in [&[][..], &[1, 1], &[0xFF, 1, 0], &long] {
        assert!(c.feed(&sb(105, payload)).is_empty());
        assert_eq!(c.events(), [unknown(105, payload)]);
        assert_eq!(
            c.readiness(),
            Err(NotReady::Pending(set(&[Setting::FlowControl]))),
            "{payload:02X?}"
        );
    }
    assert!(c.feed(&sb(105, &[1])).is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert!(c.feed(&sb(105, &[1, 1])).is_empty());
    assert_eq!(c.readiness(), Ok(()));
}

// ---- SIGNATURE (G-rules) --------------------------------------------------

#[test]
fn signature_request_from_server_is_not_a_reply() {
    // With no request of ours outstanding, an empty SIGNATURE is the server
    // asking for our signature, in any state: no event, and no answer.
    let mut c = client();
    assert!(c.feed(&sb(100, &[])).is_empty());
    assert!(c.events().is_empty());
    assert!(c.out().is_empty());
    activate(&mut c);
    assert!(c.feed(&sb(100, &[])).is_empty());
    assert!(c.events().is_empty());
    assert!(c.out().is_empty(), "not answered");

    // A non-empty one is a signature the server volunteered.
    assert!(c.feed(&sb(100, b"bridge 1.0")).is_empty());
    assert_eq!(
        c.events(),
        [Event::Signature(InlineBytes::new(b"bridge 1.0"))]
    );
    assert!(c.out().is_empty());

    // A request of ours answered with text is settled by that answer: a
    // later empty SIGNATURE is the server's request again.
    assert_eq!(c.try_request_signature(), Ok(()));
    assert_eq!(c.out(), sb(0, &[]));
    assert!(c.feed(&sb(100, b"ser2net")).is_empty());
    assert_eq!(c.events(), [Event::Signature(InlineBytes::new(b"ser2net"))]);
    assert!(c.feed(&sb(100, &[])).is_empty());
    assert!(c.events().is_empty());
}

#[test]
fn empty_signature_reply_to_our_request_is_reported() {
    // A refused request is not outstanding.
    let mut c = client();
    assert_eq!(c.try_request_signature(), Err(CommandError::Negotiating));
    assert!(c.feed(&sb(100, &[])).is_empty());
    assert!(c.events().is_empty());

    // ser2net 4.6.4 answers a request with an empty signature: each answer
    // to a request of ours is reported.
    activate(&mut c);
    assert_eq!(c.try_request_signature(), Ok(()));
    assert_eq!(c.try_request_signature(), Ok(()));
    assert_eq!(c.out(), [sb(0, &[]), sb(0, &[])].concat());
    assert!(c.feed(&[sb(100, &[]), sb(100, &[])].concat()).is_empty());
    assert_eq!(
        c.events(),
        [
            Event::Signature(InlineBytes::EMPTY),
            Event::Signature(InlineBytes::EMPTY)
        ]
    );

    // Both are answered: a third empty one is the server's request.
    assert!(c.feed(&sb(100, &[])).is_empty());
    assert!(c.events().is_empty());
}

#[test]
fn signature_keeps_64_bytes_and_the_true_length() {
    let long: Vec<u8> = (0..100).map(|i| b'a' + i % 26).collect();
    let mut c = active(PortSettings::default());
    assert_eq!(c.try_request_signature(), Ok(()));
    assert!(c.feed(&sb(100, &long)).is_empty());
    let events = c.events();
    let [Event::Signature(signature)] = &events[..] else {
        panic!("expected one Signature event, got {events:?}")
    };
    assert_eq!(signature.as_bytes(), &long[..64]);
    assert_eq!(signature.original_len(), 100);
    assert!(signature.is_truncated());

    // 64 bytes fit, also when every one of them was escaped on the wire.
    for text in [long[..64].to_vec(), vec![0xFF; 64]] {
        assert_eq!(c.try_request_signature(), Ok(()));
        assert!(c.feed(&sb(100, &text)).is_empty());
        let events = c.events();
        let [Event::Signature(signature)] = &events[..] else {
            panic!("expected one Signature event, got {events:?}")
        };
        assert_eq!(signature, &InlineBytes::new(&text));
        assert_eq!(signature.original_len(), 64);
        assert!(!signature.is_truncated());
    }
}

// ---- notifications and flow control (F-rules) -----------------------------

#[test]
fn notifications_and_flow_control() {
    let mut c = active(PortSettings::default());
    assert_eq!(c.modem_state(), None);
    assert!(!c.flow_suspended());
    // A resume while not suspended changes nothing.
    assert!(c.feed(&sb(109, &[])).is_empty());
    assert!(c.events().is_empty());

    // Two suspends in a row: one FlowSuspended.
    let input = [
        sb(106, &[0x61]),
        sb(107, &[0x31]),
        sb(108, &[]),
        sb(108, &[]),
    ]
    .concat();
    assert!(c.feed(&input).is_empty());
    let line =
        LineState::SHIFT_REGISTER_EMPTY | LineState::HOLDING_REGISTER_EMPTY | LineState::DATA_READY;
    let modem = ModemState::DSR | ModemState::CTS | ModemState::DELTA_CTS;
    assert_eq!(
        c.events(),
        [
            Event::LineState(line),
            Event::ModemState(modem),
            Event::FlowSuspended
        ]
    );
    assert_eq!(c.modem_state(), Some(modem), "kept as sent, delta included");
    assert!(c.flow_suspended());

    // Advisory: data is still written while suspended.
    assert_eq!(c.try_write_data(b"x"), Ok(1));
    assert_eq!(c.out(), b"x");

    assert!(c.feed(&[sb(109, &[]), sb(109, &[])].concat()).is_empty());
    assert_eq!(c.events(), [Event::FlowResumed]);
    assert!(!c.flow_suspended());

    // The cache follows the latest notification; a notification of another
    // length is Unknown and leaves it alone.
    assert!(c.feed(&sb(107, &[0x20])).is_empty());
    assert_eq!(c.events(), [Event::ModemState(ModemState::DSR)]);
    assert_eq!(c.modem_state(), Some(ModemState::DSR));
    assert!(c.feed(&sb(107, &[])).is_empty());
    assert_eq!(c.events(), [unknown(107, &[])]);
    assert_eq!(c.modem_state(), Some(ModemState::DSR));
    assert!(c.out().is_empty());
}

#[test]
fn modem_cache_cleared_on_refusal() {
    let mut c = active(PortSettings::default());
    assert!(
        c.feed(&[sb(107, &[0xB0]), sb(108, &[])].concat())
            .is_empty()
    );
    let _ = c.events();
    assert_eq!(
        c.modem_state(),
        Some(ModemState::CD | ModemState::DSR | ModemState::CTS)
    );
    assert!(c.flow_suspended());

    // Withdrawn: the cache and the flow state are cleared.
    assert!(c.feed(&hex("FF FE 2C")).is_empty());
    assert_eq!(c.events(), [Event::ComPortRefused]);
    assert_eq!(c.modem_state(), None);
    assert!(!c.flow_suspended());
    // Notifications are still decoded while Refused, and kept.
    assert!(c.feed(&sb(107, &[0x10])).is_empty());
    assert_eq!(c.events(), [Event::ModemState(ModemState::CTS)]);
    assert_eq!(c.modem_state(), Some(ModemState::CTS));

    // Declined before it was ever active: cleared as well.
    let mut c = client();
    assert!(c.feed(&sb(107, &[0x30])).is_empty());
    assert_eq!(c.modem_state(), Some(ModemState::DSR | ModemState::CTS));
    assert!(c.feed(&hex("FF FE 2C")).is_empty());
    assert_eq!(
        c.events(),
        [
            Event::ModemState(ModemState::DSR | ModemState::CTS),
            Event::ComPortRefused
        ]
    );
    assert_eq!(c.modem_state(), None);
}
