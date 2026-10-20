//! Server behaviour, through the public API and the `common` driver.

mod common;

use common::{Wire, activate_server, hex, pump, replies, requests, sb};
use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, InlineBytes, LineState,
    ModemState, Parity, PortSettings, Purge, Server, ServerEvent, StopBits,
};

/// The default signature: `"rfc2217 <version>"`.
const DEFAULT_SIGNATURE: &str = concat!("rfc2217 ", env!("CARGO_PKG_VERSION"));

/// A new server whose opening has been drained.
fn server() -> Wire<Server> {
    let mut s = Wire::new(Server::new(PortSettings::default()));
    let _ = s.out();
    s
}

/// An active server (the client's `WILL 44` accepted) with nothing pending.
fn active(server: Server) -> Wire<Server> {
    let mut s = Wire::new(server);
    activate_server(&mut s);
    s
}

/// 19200 baud, 7 data bits, even parity, 2 stop bits, RTS/CTS: no field
/// has its default value.
fn non_default() -> PortSettings {
    PortSettings {
        baud_rate: 19_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::RtsCts,
    }
}

/// What successive receive calls yielded, in order.
#[derive(Debug, PartialEq)]
enum Seen {
    Payload(Vec<u8>),
    Event(ServerEvent),
}

#[test]
fn receive_into_marks_event_positions() {
    let mut s = active(Server::new(PortSettings::default()));
    let input = [
        b"AT".to_vec(),
        sb(1, &hex("00 00 4B 00")),
        sb(12, &[2]),
        b"\r".to_vec(),
    ]
    .concat();
    let mut payload = [0; 64];
    let (mut seen, mut steps) = (Vec::new(), Vec::new());
    let mut at = 0;
    while at < input.len() {
        let p = s.receive_into(&input[at..], &mut payload);
        at += p.consumed;
        steps.push((p.consumed, p.payload_len));
        if p.payload_len > 0 {
            seen.push(Seen::Payload(payload[..p.payload_len].to_vec()));
        }
        while let Some(event) = s.poll_event() {
            seen.push(Seen::Event(event));
        }
    }
    // Each call returns right after the byte that produced an event: the
    // payload before it comes with it, the payload after it with a later
    // call.
    assert_eq!(steps, [(2 + 10, 2), (7, 0), (1, 1)]);
    let changed = PortSettings {
        baud_rate: 19_200,
        ..PortSettings::default()
    };
    assert_eq!(
        seen,
        [
            Seen::Payload(b"AT".to_vec()),
            Seen::Event(ServerEvent::SettingsChanged(changed)),
            Seen::Event(ServerEvent::Purge(Purge::Transmit)),
            Seen::Payload(b"\r".to_vec()),
        ]
    );
    assert_eq!(
        s.out(),
        [sb(101, &hex("00 00 4B 00")), sb(112, &[2])].concat()
    );
}

#[test]
fn receive_into_on_empty_input_consumes_nothing() {
    let mut payload = [0xAA; 4];
    let mut s = Wire::new(Server::new(PortSettings::default()));
    let p = s.receive_into(&[], &mut payload);
    assert_eq!((p.consumed, p.payload_len), (0, 0));
    assert_eq!(payload, [0xAA; 4]);
    assert_eq!(s.poll_event(), None);
    assert_eq!(s.out().len(), 12, "only the opening");

    // With an event waiting: still nothing, and the event stays.
    assert_eq!(s.receive_into(&hex("FF FB 2C"), &mut payload).consumed, 3);
    let p = s.receive_into(&[], &mut payload);
    assert_eq!((p.consumed, p.payload_len), (0, 0));
    assert_eq!(s.poll_event(), Some(ServerEvent::ComPortActive));
    assert_eq!(s.poll_event(), None);

    // The client's is the same.
    let mut c = Client::new(PortSettings::default());
    let p = c.receive_into(&[], &mut payload);
    assert_eq!((p.consumed, p.payload_len), (0, 0));
    assert_eq!(c.poll_event(), None);
    assert_eq!(c.com_port_state(), ComPortState::Negotiating);
}

#[test]
fn opening_offers() {
    let mut s = Wire::new(Server::new(PortSettings::default()));
    assert_eq!(s.out(), hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03"));
    assert!(!s.com_port_active());
    assert!(s.events().is_empty());
}

/// The server leaves COM-PORT-OPTION to the client. An unsolicited
/// `DO COM-PORT-OPTION` in the opening trips a race in pyserial 3.5's
/// client: when its reader thread sees the `DO` before the client has sent
/// its own requests, it marks the option active without ever sending
/// `WILL`, so every request it makes is ignored (about 1 open in 5 failed
/// on loopback with "Remote does not accept parameter change").
#[test]
fn opening_leaves_com_port_option_to_the_client() {
    let mut s = Wire::new(Server::new(PortSettings::default()));
    let opening = s.out();
    assert!(
        !opening.windows(3).any(|w| w == hex("FF FD 2C")),
        "no DO COM-PORT-OPTION in {opening:02X?}"
    );
    assert!(!opening.contains(&0x2C), "COM-PORT-OPTION not mentioned");

    // pyserial's client, which then always offers WILL COM-PORT-OPTION:
    // accepted with DO, and its requests are answered.
    let pyserial = hex("FF FD 01 FF FB 03 FF FD 03 FF FD 2C FF FB 2C");
    assert!(s.feed(&pyserial).is_empty());
    assert_eq!(s.out(), hex("FF FC 01 FF FC 2C FF FD 2C"));
    assert!(s.com_port_active());
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
    assert!(s.feed(&sb(1, &hex("00 00 4B 00"))).is_empty());
    assert_eq!(s.out(), sb(101, &hex("00 00 4B 00")));
    assert_eq!(s.settings().baud_rate, 19_200);

    // The crate's own client offers WILL COM-PORT-OPTION in its opening.
    let mut s = Wire::new(Server::new(PortSettings::default()));
    let _ = s.out();
    let mut c = Wire::new(Client::new(PortSettings::default()));
    assert!(s.feed(&c.out()).is_empty());
    assert!(s.com_port_active());
    assert_eq!(s.out(), hex("FF FD 2C FF FC 2C"));
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
}

#[test]
fn answers_negotiation_by_direction() {
    let mut s = server();
    // The client's WILL is accepted (DO): active.
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert!(s.com_port_active());
    assert_eq!(s.out(), hex("FF FD 2C"));
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);

    // The server never performs COM-PORT-OPTION itself; ECHO is refused
    // both ways.
    for (request, answer) in [
        ("FF FD 2C", "FF FC 2C"),
        ("FF FB 01", "FF FE 01"),
        ("FF FD 01", "FF FC 01"),
    ] {
        assert!(s.feed(&hex(request)).is_empty());
        assert_eq!(s.out(), hex(answer), "answer to {request}");
    }
    assert!(s.com_port_active());

    // The client's answers to the rest of the opening are taken silently.
    assert!(
        s.feed(&hex("FF FD 00 FF FB 00 FF FD 03 FF FB 03"))
            .is_empty()
    );
    assert!(s.out().is_empty());

    // Requests the client starts itself follow the same policy: once it has
    // disabled them, BINARY and SGA are accepted again in both directions;
    // TTYPE never is.
    let disable = hex("FF FE 00 FF FC 00 FF FE 03 FF FC 03");
    assert!(s.feed(&disable).is_empty());
    assert_eq!(s.out(), hex("FF FC 00 FF FE 00 FF FC 03 FF FE 03"));
    for (request, answer) in [
        ("FF FD 00", "FF FB 00"),
        ("FF FB 00", "FF FD 00"),
        ("FF FD 03", "FF FB 03"),
        ("FF FB 03", "FF FD 03"),
        ("FF FD 18", "FF FC 18"),
        ("FF FB 18", "FF FE 18"),
    ] {
        assert!(s.feed(&hex(request)).is_empty());
        assert_eq!(s.out(), hex(answer), "answer to {request}");
    }
    assert!(s.com_port_active());
    assert!(s.events().is_empty());
}

#[test]
fn com_port_active_follows_the_clients_will() {
    let mut s = active(Server::new(PortSettings::default()));
    // The client withdraws the option: acknowledged, no event, and requests
    // are ignored again.
    assert!(s.feed(&hex("FF FC 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FE 2C"));
    assert!(!s.com_port_active());
    assert!(s.events().is_empty());
    assert!(s.feed(&sb(5, &[9])).is_empty());
    assert!(s.out().is_empty());
    assert!(s.events().is_empty());

    // It enables the option again: accepted and announced again.
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FD 2C"));
    assert!(s.com_port_active());
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
    assert!(s.feed(&sb(5, &[9])).is_empty());
    assert_eq!(s.out(), sb(105, &[9]));
    assert_eq!(s.events(), vec![ServerEvent::Dtr(false)]);

    // A repeated WILL changes nothing.
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert!(s.out().is_empty());
    assert!(s.events().is_empty());
}

#[test]
fn subnegotiation_before_activation_is_ignored() {
    let requests = [
        sb(0, &[]),
        sb(0, b"client"),
        sb(1, &hex("00 00 12 C0")),
        sb(2, &[7]),
        sb(5, &[8]),
        sb(5, &[0]),
        sb(11, &[0xFF]),
        sb(12, &[3]),
    ]
    .concat();
    let mut s = server();
    // Before any negotiation; after a DO 44 (refused: the server never
    // performs the option); after a WONT 44.
    for negotiation in ["", "FF FD 2C", "FF FC 2C"] {
        assert!(s.feed(&hex(negotiation)).is_empty());
        let _ = s.out();
        assert!(s.feed(&requests).is_empty());
        assert!(!s.com_port_active());
        assert!(s.out().is_empty(), "after {negotiation:?}");
        assert!(s.events().is_empty(), "after {negotiation:?}");
        assert_eq!(s.settings(), PortSettings::default());
        // Data flows.
        assert_eq!(s.feed(b"AT\r"), b"AT\r");
    }

    // Once the client enables the option, the same requests are answered.
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FD 2C"));
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
    assert!(s.feed(&requests).is_empty());
    assert_eq!(
        s.out(),
        [
            sb(100, DEFAULT_SIGNATURE.as_bytes()),
            sb(101, &hex("00 00 12 C0")),
            sb(102, &[7]),
            sb(105, &[8]),
            sb(105, &[1]),
            sb(111, &[0xFF]),
            sb(112, &[3]),
        ]
        .concat()
    );
    let baud = PortSettings {
        baud_rate: 4800,
        ..PortSettings::default()
    };
    let data_bits = PortSettings {
        data_bits: DataBits::Seven,
        ..baud
    };
    assert_eq!(
        s.events(),
        vec![
            ServerEvent::ClientSignature(InlineBytes::new(b"client")),
            ServerEvent::SettingsChanged(baud),
            ServerEvent::SettingsChanged(data_bits),
            ServerEvent::Dtr(true),
            ServerEvent::Purge(Purge::Both),
        ]
    );
}

/// Feed `request` to an active server: it must answer exactly `reply`,
/// hold `expected` and report the change.
fn applied(s: &mut Wire<Server>, request: &[u8], reply: &[u8], expected: PortSettings) {
    assert!(s.feed(request).is_empty());
    assert_eq!(s.out(), reply, "reply to {request:02X?}");
    assert_eq!(s.settings(), expected);
    assert_eq!(s.events(), vec![ServerEvent::SettingsChanged(expected)]);
}

#[test]
fn applies_and_acknowledges_settings() {
    let mut s = active(Server::new(PortSettings::default()));
    let mut expected = PortSettings {
        baud_rate: 4800,
        ..PortSettings::default()
    };
    let rate = hex("00 00 12 C0");
    applied(&mut s, &sb(1, &rate), &sb(101, &rate), expected);
    expected.data_bits = DataBits::Seven;
    applied(&mut s, &sb(2, &[7]), &sb(102, &[7]), expected);
    expected.parity = Parity::Even;
    applied(&mut s, &sb(3, &[3]), &sb(103, &[3]), expected);
    expected.stop_bits = StopBits::Two;
    applied(&mut s, &sb(4, &[2]), &sb(104, &[2]), expected);
    expected.flow_control = FlowControl::RtsCts;
    applied(&mut s, &sb(5, &[3]), &sb(105, &[3]), expected);

    // sredird's 8-byte rates, either half, are acknowledged in 4 bytes.
    expected.baud_rate = 9600;
    let rate = hex("00 00 25 80 00 00 00 00");
    applied(
        &mut s,
        &sb(1, &rate),
        &sb(101, &hex("00 00 25 80")),
        expected,
    );
    expected.baud_rate = 115_200;
    let rate = hex("00 00 00 00 00 01 C2 00");
    applied(
        &mut s,
        &sb(1, &rate),
        &sb(101, &hex("00 01 C2 00")),
        expected,
    );
    // 0xFF rate bytes are escaped both ways.
    expected.baud_rate = u32::MAX;
    applied(&mut s, &sb(1, &[0xFF; 4]), &sb(101, &[0xFF; 4]), expected);

    // The other values of each setting.
    for (code, bits) in [
        (5, DataBits::Five),
        (6, DataBits::Six),
        (8, DataBits::Eight),
    ] {
        expected.data_bits = bits;
        applied(&mut s, &sb(2, &[code]), &sb(102, &[code]), expected);
    }
    for (code, parity) in [
        (1, Parity::None),
        (2, Parity::Odd),
        (4, Parity::Mark),
        (5, Parity::Space),
    ] {
        expected.parity = parity;
        applied(&mut s, &sb(3, &[code]), &sb(103, &[code]), expected);
    }
    for (code, bits) in [(1, StopBits::One), (3, StopBits::OnePointFive)] {
        expected.stop_bits = bits;
        applied(&mut s, &sb(4, &[code]), &sb(104, &[code]), expected);
    }
    for (code, flow) in [
        (17, FlowControl::Dcd),
        (19, FlowControl::Dsr),
        (2, FlowControl::XonXoff),
        (1, FlowControl::None),
    ] {
        expected.flow_control = flow;
        applied(&mut s, &sb(5, &[code]), &sb(105, &[code]), expected);
    }
}

#[test]
fn server_answers_control_queries() {
    let settings = PortSettings {
        flow_control: FlowControl::XonXoff,
        ..PortSettings::default()
    };
    let mut s = active(Server::new(settings));
    // Flow control as configured, break off, DTR on, RTS on.
    for (query, answer) in [(0, 2), (4, 6), (7, 8), (10, 11)] {
        assert!(s.feed(&sb(5, &[query])).is_empty());
        assert_eq!(s.out(), sb(105, &[answer]), "answer to {query}");
    }
    assert!(s.events().is_empty());

    // The client changes all four; the queries report the new states.
    assert!(
        s.feed(&[5, 9, 12, 3].map(|v| sb(5, &[v])).concat())
            .is_empty()
    );
    let _ = s.out();
    assert_eq!(s.events().len(), 4);
    for (query, answer) in [(0, 3), (4, 5), (7, 9), (10, 12)] {
        assert!(s.feed(&sb(5, &[query])).is_empty());
        assert_eq!(s.out(), sb(105, &[answer]), "answer to {query}");
    }
    assert!(s.events().is_empty());
    assert_eq!(s.settings().flow_control, FlowControl::RtsCts);
}

#[test]
fn server_break_dtr_rts_events() {
    let mut s = active(Server::new(PortSettings::default()));
    for (value, event) in [
        (5, ServerEvent::Break(true)),
        (6, ServerEvent::Break(false)),
        (9, ServerEvent::Dtr(false)),
        (8, ServerEvent::Dtr(true)),
        (12, ServerEvent::Rts(false)),
        (11, ServerEvent::Rts(true)),
        // Setting a line to the state it has is still reported.
        (11, ServerEvent::Rts(true)),
        (6, ServerEvent::Break(false)),
    ] {
        assert!(s.feed(&sb(5, &[value])).is_empty());
        assert_eq!(s.out(), sb(105, &[value]), "echo of {value}");
        assert_eq!(s.events(), vec![event]);
    }
    assert_eq!(s.settings(), PortSettings::default());
}

#[test]
fn inbound_flow_is_answered_from_outbound() {
    for (outbound, inbound) in [
        (FlowControl::None, 14),
        (FlowControl::XonXoff, 15),
        (FlowControl::RtsCts, 16),
        (FlowControl::Dcd, 14),
        (FlowControl::Dsr, 14),
    ] {
        let settings = PortSettings {
            flow_control: outbound,
            ..PortSettings::default()
        };
        let mut s = active(Server::new(settings));
        // The inbound query and every inbound value: nothing is applied.
        for value in [13, 14, 15, 16, 18] {
            assert!(s.feed(&sb(5, &[value])).is_empty());
            assert_eq!(s.out(), sb(105, &[inbound]), "{outbound:?}, value {value}");
        }
        assert!(s.events().is_empty());
        assert_eq!(s.settings(), settings);
    }

    // The answer follows the outbound setting as the client changes it.
    let mut s = active(Server::new(PortSettings::default()));
    assert!(s.feed(&sb(5, &[3])).is_empty());
    let _ = s.out();
    let _ = s.events();
    assert!(s.feed(&sb(5, &[13])).is_empty());
    assert_eq!(s.out(), sb(105, &[16]));
    assert!(s.events().is_empty());
}

#[test]
fn server_echoes_masks_escaped() {
    let mut s = active(Server::new(PortSettings::default()));
    assert!(s.feed(&sb(10, &[0xFF])).is_empty());
    assert_eq!(s.out(), hex("FF FA 2C 6E FF FF FF F0"));
    assert!(s.feed(&sb(11, &[0xFF])).is_empty());
    assert_eq!(s.out(), hex("FF FA 2C 6F FF FF FF F0"));
    for mask in [0x00, 0x08, 0x30] {
        assert!(s.feed(&sb(10, &[mask])).is_empty());
        assert_eq!(s.out(), sb(110, &[mask]));
        assert!(s.feed(&sb(11, &[mask])).is_empty());
        assert_eq!(s.out(), sb(111, &[mask]));
    }
    assert!(s.events().is_empty());
    assert_eq!(s.settings(), PortSettings::default());
}

#[test]
fn server_invalid_values_leave_settings_unchanged() {
    let settings = non_default();
    let mut s = active(Server::new(settings));
    // A query (0) or an unassigned code is answered with the value in
    // effect.
    for (request, reply) in [
        (sb(2, &[0]), sb(102, &[7])),
        (sb(2, &[4]), sb(102, &[7])),
        (sb(2, &[9]), sb(102, &[7])),
        (sb(2, &[0xFF]), sb(102, &[7])),
        (sb(3, &[0]), sb(103, &[3])),
        (sb(3, &[6]), sb(103, &[3])),
        (sb(4, &[0]), sb(104, &[2])),
        (sb(4, &[4]), sb(104, &[2])),
        (sb(5, &[20]), sb(105, &[3])),
        (sb(5, &[0x80]), sb(105, &[3])),
        (sb(5, &[0xFF]), sb(105, &[3])),
    ] {
        assert!(s.feed(&request).is_empty());
        assert_eq!(s.out(), reply, "reply to {request:02X?}");
    }
    // A purge value other than 1-3 is echoed without an event.
    for value in [0, 4, 0xFF] {
        assert!(s.feed(&sb(12, &[value])).is_empty());
        assert_eq!(s.out(), sb(112, &[value]));
    }
    assert!(s.events().is_empty());
    assert_eq!(s.settings(), settings);
}

#[test]
fn server_wrong_length_set_gets_no_reply() {
    let mut s = active(Server::new(PortSettings::default()));
    // A rate followed by 96 zero bytes: the parser keeps 64 payload bytes
    // and the true length (102), so it is never read as 9600.
    let mut long_rate = hex("00 00 25 80");
    long_rate.resize(100, 0);
    for request in [
        sb(1, &[]),
        sb(1, &hex("00 25 80")),
        sb(1, &hex("00 00 25 80 00")),
        sb(1, &hex("00 00 25 80 00 00 00 01")),
        sb(1, &long_rate),
        sb(2, &[]),
        sb(2, &[7, 7]),
        sb(3, &[]),
        sb(3, &[3, 3]),
        sb(4, &[]),
        sb(4, &[2, 2]),
        sb(5, &[]),
        sb(5, &[9, 9]),
        sb(10, &[]),
        sb(10, &[1, 2]),
        sb(11, &[]),
        sb(11, &[1, 2]),
        sb(12, &[]),
        sb(12, &[1, 2]),
    ] {
        assert!(s.feed(&request).is_empty());
        assert!(s.out().is_empty(), "reply to {request:02X?}");
    }
    assert!(s.events().is_empty());
    assert_eq!(s.settings(), PortSettings::default());

    // The server is still in step.
    assert!(s.feed(&sb(2, &[7])).is_empty());
    assert_eq!(s.out(), sb(102, &[7]));
}

#[test]
fn server_ignores_notifications_and_unknown_commands() {
    let mut s = active(Server::new(PortSettings::default()));
    let mut ignored = vec![
        // Server-to-client notifications: a line state, a modem state, and
        // a modem state poll while no modem state is known.
        sb(6, &[]),
        sb(6, &[0x10]),
        sb(7, &[0x30]),
        sb(7, &[]),
        // COM-PORT-OPTION without a command.
        hex("FF FA 2C FF F0"),
    ];
    // Unassigned codes, and server replies echoed back (100-112).
    ignored.extend((13..=255).map(|code| sb(code, &[1])));
    for request in ignored {
        assert!(s.feed(&request).is_empty());
        assert!(s.out().is_empty(), "reply to {request:02X?}");
    }
    assert!(s.events().is_empty());
    assert_eq!(s.settings(), PortSettings::default());
}

#[test]
fn settings_changed_once_per_set() {
    let mut s = active(Server::new(PortSettings::default()));
    // A full round in one read, every value new: one event per request,
    // each with the settings applied so far.
    let round = requests(115_200, [7, 3, 2, 3]);
    assert!(s.feed(&round).is_empty());
    assert_eq!(s.out(), replies(115_200, [7, 3, 2, 3]));
    let mut expected = PortSettings::default();
    let mut events = Vec::new();
    expected.baud_rate = 115_200;
    events.push(ServerEvent::SettingsChanged(expected));
    expected.data_bits = DataBits::Seven;
    events.push(ServerEvent::SettingsChanged(expected));
    expected.parity = Parity::Even;
    events.push(ServerEvent::SettingsChanged(expected));
    expected.stop_bits = StopBits::Two;
    events.push(ServerEvent::SettingsChanged(expected));
    expected.flow_control = FlowControl::RtsCts;
    events.push(ServerEvent::SettingsChanged(expected));
    assert_eq!(s.events(), events);
    assert_eq!(s.settings(), expected);

    // The same round again changes nothing.
    assert!(s.feed(&round).is_empty());
    assert_eq!(s.out(), replies(115_200, [7, 3, 2, 3]));
    assert!(s.events().is_empty());

    // A round that changes one setting: one event.
    assert!(s.feed(&requests(115_200, [8, 3, 2, 3])).is_empty());
    assert_eq!(s.out(), replies(115_200, [8, 3, 2, 3]));
    expected.data_bits = DataBits::Eight;
    assert_eq!(s.events(), vec![ServerEvent::SettingsChanged(expected)]);
}

#[test]
fn server_query_does_not_emit_settings_changed() {
    let settings = PortSettings {
        baud_rate: 4800,
        data_bits: DataBits::Seven,
        parity: Parity::Odd,
        stop_bits: StopBits::OnePointFive,
        flow_control: FlowControl::Dsr,
    };
    let mut s = active(Server::new(settings));
    let rate = sb(101, &4800u32.to_be_bytes());
    for (query, answer) in [
        (sb(1, &[0; 4]), rate.clone()),
        (sb(1, &[0; 8]), rate),
        (sb(2, &[0]), sb(102, &[7])),
        (sb(3, &[0]), sb(103, &[2])),
        (sb(4, &[0]), sb(104, &[3])),
        (sb(5, &[0]), sb(105, &[19])),
    ] {
        assert!(s.feed(&query).is_empty());
        assert_eq!(s.out(), answer, "answer to {query:02X?}");
    }
    assert!(s.events().is_empty());
    assert_eq!(s.settings(), settings);
}

#[test]
fn server_ignores_client_flow_suspend() {
    let mut s = active(Server::new(PortSettings::default()));
    for request in [sb(8, &[]), sb(9, &[]), sb(8, &[1]), sb(9, &[0xFF])] {
        assert!(s.feed(&request).is_empty());
        assert!(s.out().is_empty(), "reply to {request:02X?}");
    }
    assert!(s.events().is_empty());

    // Data and requests go on as before.
    assert_eq!(s.feed(b"x"), b"x");
    assert!(s.feed(&sb(5, &[0])).is_empty());
    assert_eq!(s.out(), sb(105, &[1]));
}

#[test]
fn modem_state_snapshot_on_activation_and_mask_change() {
    let mut s = server();
    // Supplied before activation: stored, nothing sent.
    assert_eq!(
        s.try_set_modem_state(ModemState::DSR | ModemState::CTS),
        Ok(())
    );
    assert!(s.out().is_empty());

    // The client's WILL is accepted (DO): the event, then the levels.
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.out(), [hex("FF FD 2C"), sb(107, &[0x30])].concat());
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);

    // A mask change: the echo, then the levels through the new mask, sent
    // even when that leaves 0.
    for (mask, levels) in [(0x10, 0x10), (0x80, 0x00), (0xFF, 0x30)] {
        assert!(s.feed(&sb(11, &[mask])).is_empty());
        assert_eq!(
            s.out(),
            [sb(111, &[mask]), sb(107, &[levels])].concat(),
            "mask {mask:#04x}"
        );
    }
    // A mask of 0 asks for nothing: no snapshot, also not when the client
    // enables the option again (our DO answers its WILL).
    assert!(s.feed(&sb(11, &[0])).is_empty());
    assert_eq!(s.out(), sb(111, &[0]));
    assert!(s.feed(&hex("FF FC 2C FF FB 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FE 2C FF FD 2C"));

    // The mask persists across a withdrawal, and levels supplied meanwhile
    // are sent when the client enables the option again.
    assert!(s.feed(&sb(11, &[0xA0])).is_empty());
    assert_eq!(s.out(), [sb(111, &[0xA0]), sb(107, &[0x20])].concat());
    assert!(s.feed(&hex("FF FC 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FE 2C"));
    assert_eq!(
        s.try_set_modem_state(ModemState::CD | ModemState::CTS),
        Ok(())
    );
    assert!(s.out().is_empty());
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.out(), [hex("FF FD 2C"), sb(107, &[0x80])].concat());
    assert_eq!(
        s.events(),
        vec![ServerEvent::ComPortActive, ServerEvent::ComPortActive]
    );

    // Without a modem state there is nothing to report.
    let mut s = active(Server::new(PortSettings::default()));
    assert!(s.feed(&sb(11, &[0x30])).is_empty());
    assert_eq!(s.out(), sb(111, &[0x30]));
    assert!(s.events().is_empty());
}

#[test]
fn modem_poll_answered_only_with_state() {
    let mut s = active(Server::new(PortSettings::default()));
    // pyserial's poll, an empty NOTIFY-MODEMSTATE: no state, no answer.
    assert!(s.feed(&sb(7, &[])).is_empty());
    assert!(s.out().is_empty());

    assert_eq!(s.try_set_modem_state(ModemState::CTS), Ok(()));
    assert_eq!(s.out(), sb(107, &[0x10]));
    assert_eq!(
        s.try_set_modem_state(ModemState::DSR | ModemState::CTS),
        Ok(())
    );
    assert_eq!(s.out(), sb(107, &[0x32]));
    // The answer is the levels, without change bits.
    assert!(s.feed(&sb(7, &[])).is_empty());
    assert_eq!(s.out(), sb(107, &[0x30]));
    // Through the mask, and even when the mask is 0.
    for (mask, levels) in [(0x20, 0x20), (0x00, 0x00)] {
        assert!(s.feed(&sb(11, &[mask])).is_empty());
        let _ = s.out();
        assert!(s.feed(&sb(7, &[])).is_empty());
        assert_eq!(s.out(), sb(107, &[levels]), "mask {mask:#04x}");
    }
    // A NOTIFY-MODEMSTATE with a value is not a poll.
    assert!(s.feed(&sb(7, &[0x30])).is_empty());
    assert!(s.out().is_empty());

    // Polls are ignored while the option is not enabled, like every request.
    assert!(s.feed(&hex("FF FC 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FE 2C"));
    assert!(s.feed(&sb(7, &[])).is_empty());
    assert!(s.out().is_empty());
    assert!(s.events().is_empty());
}

#[test]
fn set_modem_state_computes_deltas() {
    let mut s = active(Server::new(PortSettings::default()));
    for (state, sent) in [
        // The first state as it is: its levels.
        (ModemState::DSR | ModemState::DELTA_DSR, Some(0x20)),
        // CTS rises: the levels and DELTA_CTS.
        (ModemState::DSR | ModemState::CTS, Some(0x31)),
        // No change, nothing sent.
        (ModemState::DSR | ModemState::CTS, None),
        // CTS and DSR fall; RI rises, which is no change bit.
        (ModemState::RI, Some(0x43)),
        // RI falls: TRAILING_EDGE_RI.
        (ModemState::default(), Some(0x04)),
        (ModemState::CD, Some(0x88)),
        // A rising RI alone reports nothing.
        (ModemState::CD | ModemState::RI, None),
        (ModemState::CD, Some(0x84)),
        // Change bits in the state are added: a pulse too short to show in
        // the levels.
        (ModemState::CD | ModemState::DELTA_CTS, Some(0x81)),
        // Only the levels were stored.
        (ModemState::CD, None),
    ] {
        assert_eq!(s.try_set_modem_state(state), Ok(()));
        let expected = sent.map_or_else(Vec::new, |value| sb(107, &[value]));
        assert_eq!(s.out(), expected, "{state:?}");
    }
    assert_eq!(
        s.try_set_modem_state(ModemState::CD | ModemState::DELTA_DSR),
        Ok(())
    );
    assert_eq!(s.out(), sb(107, &[0x82]));
    assert!(s.feed(&sb(7, &[])).is_empty());
    assert_eq!(s.out(), sb(107, &[0x80]), "the levels only");
    assert!(s.events().is_empty());
}

#[test]
fn masked_out_change_is_not_sent() {
    let mut s = active(Server::new(PortSettings::default()));
    // The client wants CTS only: its level and its change bit.
    assert!(s.feed(&sb(11, &[0x11])).is_empty());
    assert_eq!(s.out(), sb(111, &[0x11]));
    assert_eq!(s.try_set_modem_state(ModemState::default()), Ok(()));
    assert_eq!(s.out(), sb(107, &[0x00]));
    // DSR changes: masked out.
    assert_eq!(s.try_set_modem_state(ModemState::DSR), Ok(()));
    assert!(s.out().is_empty());
    // CTS changes: sent through the mask.
    assert_eq!(
        s.try_set_modem_state(ModemState::DSR | ModemState::CTS),
        Ok(())
    );
    assert_eq!(s.out(), sb(107, &[0x11]));
    // A DSR change while CTS is on: RFC 2217 sends the new state through
    // the mask whenever that is not 0, so the CTS level goes out.
    assert_eq!(s.try_set_modem_state(ModemState::CTS), Ok(()));
    assert_eq!(s.out(), sb(107, &[0x10]));

    // A mask of 0 lets nothing through, but the levels are still stored:
    // the next mask reports them.
    assert!(s.feed(&sb(11, &[0])).is_empty());
    assert_eq!(s.out(), sb(111, &[0]));
    assert_eq!(s.try_set_modem_state(ModemState::CD), Ok(()));
    assert!(s.out().is_empty());
    assert!(s.feed(&sb(11, &[0xF0])).is_empty());
    assert_eq!(s.out(), [sb(111, &[0xF0]), sb(107, &[0x80])].concat());
    assert!(s.events().is_empty());

    // The same for the first state supplied: a mask of 0 suppresses its
    // snapshot, and the next mask reports it.
    let mut s = active(Server::new(PortSettings::default()));
    assert!(s.feed(&sb(11, &[0])).is_empty());
    assert_eq!(s.out(), sb(111, &[0]));
    assert_eq!(s.try_set_modem_state(ModemState::CTS), Ok(()));
    assert!(s.out().is_empty());
    assert!(s.feed(&sb(11, &[0xF0])).is_empty());
    assert_eq!(s.out(), [sb(111, &[0xF0]), sb(107, &[0x10])].concat());
}

#[test]
fn notifications_wait_for_activation() {
    let mut s = server();
    assert_eq!(s.try_set_modem_state(ModemState::CTS), Ok(()));
    let line = LineState::BREAK_DETECT;
    assert_eq!(s.try_notify_line_state(line), Err(CommandError::Inactive));
    assert!(s.out().is_empty(), "nothing before activation");
    // The client's line-state mask is ignored too.
    assert!(s.feed(&sb(10, &[0xFF])).is_empty());
    assert!(s.out().is_empty());
    assert_eq!(s.try_notify_line_state(line), Err(CommandError::Inactive));

    // After activation the snapshot reports CTS.
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.out(), [hex("FF FD 2C"), sb(107, &[0x10])].concat());
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
    // The mask sent before activation was not applied.
    assert_eq!(s.try_notify_line_state(line), Ok(()));
    assert!(s.out().is_empty());

    // Withdrawn: modem states are stored again, line states refused; the
    // stored levels are reported once the option is enabled again.
    assert!(s.feed(&hex("FF FC 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FE 2C"));
    assert_eq!(s.try_set_modem_state(ModemState::DSR), Ok(()));
    assert_eq!(s.try_notify_line_state(line), Err(CommandError::Inactive));
    assert!(s.out().is_empty());
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.out(), [hex("FF FD 2C"), sb(107, &[0x20])].concat());
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);

    // Not active, a state is stored even with the output full: there is
    // nothing to send.
    let mut s = server();
    while s.try_write_data(b"data").is_ok() {}
    assert_eq!(s.try_set_modem_state(ModemState::CD), Ok(()));
    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert!(s.out().ends_with(&sb(107, &[0x80])));
}

#[test]
fn line_state_needs_a_mask() {
    let mut s = active(Server::new(PortSettings::default()));
    let state = LineState::BREAK_DETECT | LineState::DATA_READY;
    // The default mask is 0: nothing is sent.
    assert_eq!(s.try_notify_line_state(state), Ok(()));
    assert!(s.out().is_empty());
    // Nothing to send needs no room: Ok even with the write budget used up.
    while s.try_write_data(b"data").is_ok() {}
    let pending = s.output().to_vec();
    assert_eq!(s.try_notify_line_state(state), Ok(()));
    assert_eq!(s.output(), pending);
    let _ = s.out();

    assert!(s.feed(&sb(10, &[0x10])).is_empty());
    assert_eq!(s.out(), sb(110, &[0x10]));
    assert_eq!(s.try_notify_line_state(state), Ok(()));
    assert_eq!(s.out(), sb(106, &[0x10]));
    // Not stored: the same state is sent again.
    assert_eq!(s.try_notify_line_state(state), Ok(()));
    assert_eq!(s.out(), sb(106, &[0x10]));
    // Masked out entirely: nothing.
    assert_eq!(s.try_notify_line_state(LineState::DATA_READY), Ok(()));
    assert!(s.out().is_empty());

    // Every bit, 0xFF escaped.
    assert!(s.feed(&sb(10, &[0xFF])).is_empty());
    assert_eq!(s.out(), hex("FF FA 2C 6E FF FF FF F0"));
    let all = LineState::from_bits(0xFF);
    assert_eq!(s.try_notify_line_state(all), Ok(()));
    assert_eq!(s.out(), hex("FF FA 2C 6A FF FF FF F0"));
    // The mask persists across a withdrawal.
    assert!(s.feed(&hex("FF FC 2C FF FB 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FE 2C FF FD 2C"));
    assert_eq!(s.try_notify_line_state(LineState::TIMEOUT_ERROR), Ok(()));
    assert_eq!(s.out(), sb(106, &[0x80]));
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
}

#[test]
fn suspend_resume_are_gated() {
    let mut s = server();
    assert_eq!(s.try_suspend_flow(), Err(CommandError::Inactive));
    assert_eq!(s.try_resume_flow(), Err(CommandError::Inactive));
    assert!(s.out().is_empty());
    // Not active comes first, whatever the room.
    while s.try_write_data(b"data").is_ok() {}
    assert_eq!(s.try_suspend_flow(), Err(CommandError::Inactive));
    let _ = s.out();

    assert!(s.feed(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FD 2C"), "no modem state to report");
    assert_eq!(s.try_suspend_flow(), Ok(()));
    assert_eq!(s.out(), hex("FF FA 2C 6C FF F0"));
    assert_eq!(s.try_resume_flow(), Ok(()));
    assert_eq!(s.out(), hex("FF FA 2C 6D FF F0"));
    // Sent every time: the server keeps no flow state.
    assert_eq!(s.try_suspend_flow(), Ok(()));
    assert_eq!(s.try_suspend_flow(), Ok(()));
    assert_eq!(s.out(), hex("FF FA 2C 6C FF F0").repeat(2));

    // Withdrawn: refused again.
    assert!(s.feed(&hex("FF FC 2C")).is_empty());
    assert_eq!(s.out(), hex("FF FE 2C"));
    assert_eq!(s.try_resume_flow(), Err(CommandError::Inactive));
    assert_eq!(s.try_suspend_flow(), Err(CommandError::Inactive));
    assert!(s.out().is_empty());
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
}

#[test]
fn server_notifications_roundtrip_to_client() {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    let mut s = Wire::new(Server::new(PortSettings::default()));
    // The bridge's port has DSR on before the client connects.
    assert_eq!(s.try_set_modem_state(ModemState::DSR), Ok(()));
    let _ = pump(&mut c, &mut s, 4);
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(
        c.events(),
        vec![
            Event::ComPortActive,
            Event::ModemState(ModemState::DSR),
            Event::BaudRate(9600),
            Event::DataBits(DataBits::Eight),
            Event::Parity(Parity::None),
            Event::StopBits(StopBits::One),
            Event::FlowControl(FlowControl::None),
        ]
    );
    assert_eq!(c.modem_state(), Some(ModemState::DSR));

    // The client's line-state mask (the client has no call for it in 0.1).
    assert!(s.feed(&sb(10, &[0xFF])).is_empty());
    let modem = ModemState::DSR | ModemState::CTS;
    assert_eq!(s.try_set_modem_state(modem), Ok(()));
    let line = LineState::BREAK_DETECT | LineState::FRAMING_ERROR;
    assert_eq!(s.try_notify_line_state(line), Ok(()));
    assert_eq!(s.try_suspend_flow(), Ok(()));
    let _ = pump(&mut c, &mut s, 1);
    let changed = modem | ModemState::DELTA_CTS;
    assert_eq!(
        c.events(),
        vec![
            Event::ModemState(changed),
            Event::LineState(line),
            Event::FlowSuspended,
        ]
    );
    assert_eq!(c.modem_state(), Some(changed));
    assert!(c.flow_suspended());

    // A poll's answer and a resume.
    assert!(s.feed(&sb(7, &[])).is_empty());
    assert_eq!(s.try_resume_flow(), Ok(()));
    let _ = pump(&mut c, &mut s, 1);
    assert_eq!(
        c.events(),
        vec![Event::ModemState(modem), Event::FlowResumed]
    );
    assert_eq!(c.modem_state(), Some(modem));
    assert!(!c.flow_suspended());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(s.events(), vec![ServerEvent::ComPortActive]);
}

#[test]
fn server_with_empty_signature_keeps_default() {
    let mut s = active(Server::new(PortSettings::default()).with_signature(""));
    assert!(s.feed(&sb(0, &[])).is_empty());
    assert_eq!(s.out(), sb(100, DEFAULT_SIGNATURE.as_bytes()));
    assert!(s.events().is_empty());
}

#[test]
fn server_client_signature_event_is_bytes() {
    // Raw bytes, an escaped IAC included: never decoded as text.
    let text = b"pyserial \xFF\x80\x00 3.5";
    let mut s = active(Server::new(PortSettings::default()));
    assert!(s.feed(&sb(0, text)).is_empty());
    assert_eq!(
        s.events(),
        vec![ServerEvent::ClientSignature(InlineBytes::new(text))]
    );
    assert!(s.out().is_empty(), "a client signature is not answered");
}

#[test]
fn server_with_signature_reply() {
    let mut s = active(Server::new(PortSettings::default()).with_signature(b"bridge \xFF-1"));
    assert!(s.feed(&sb(0, &[])).is_empty());
    // The reply escapes the IAC in the signature.
    assert_eq!(s.out(), sb(100, b"bridge \xFF-1"));
    assert!(s.events().is_empty());
}

#[test]
fn server_long_signature_keeps_64_bytes() {
    let long: Vec<u8> = (0..100).map(|i| b'a' + i % 26).collect();
    let mut s = active(Server::new(PortSettings::default()).with_signature(&long));
    assert!(s.feed(&sb(0, &[])).is_empty());
    assert_eq!(s.out(), sb(100, &long[..64]));

    // A long client signature is kept the same way, with its length.
    assert!(s.feed(&sb(0, &long)).is_empty());
    let events = s.events();
    let [ServerEvent::ClientSignature(signature)] = &events[..] else {
        panic!("expected one ClientSignature event, got {events:?}")
    };
    assert_eq!(&signature[..], &long[..64]);
    assert_eq!(signature.original_len(), 100);
    assert!(signature.is_truncated());
}

#[test]
fn full_session_with_client() {
    let wanted = PortSettings {
        baud_rate: 4800,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::One,
        flow_control: FlowControl::RtsCts,
    };
    let mut c = Wire::new(Client::new(wanted));
    let mut s = Wire::new(Server::new(PortSettings::default()));
    let _ = pump(&mut c, &mut s, 4);
    assert_eq!(c.readiness(), Ok(()));
    assert!(s.com_port_active());
    assert_eq!(s.settings(), wanted);
    assert_eq!(
        c.events(),
        vec![
            Event::ComPortActive,
            Event::BaudRate(4800),
            Event::DataBits(DataBits::Seven),
            Event::Parity(Parity::Even),
            Event::StopBits(StopBits::One),
            Event::FlowControl(FlowControl::RtsCts),
        ]
    );
    // One event per setting that changed (the stop bits did not).
    let mut model = PortSettings::default();
    let mut changes = vec![ServerEvent::ComPortActive];
    model.baud_rate = 4800;
    changes.push(ServerEvent::SettingsChanged(model));
    model.data_bits = DataBits::Seven;
    changes.push(ServerEvent::SettingsChanged(model));
    model.parity = Parity::Even;
    changes.push(ServerEvent::SettingsChanged(model));
    model.flow_control = FlowControl::RtsCts;
    changes.push(ServerEvent::SettingsChanged(model));
    assert_eq!(s.events(), changes);

    // Commands: the exact bytes both ways, and the events on both sides.
    assert_eq!(c.try_purge(Purge::Receive), Ok(()));
    assert_eq!(c.try_set_dtr(false), Ok(()));
    assert_eq!(c.try_request_signature(), Ok(()));
    let to_server = c.out();
    assert_eq!(to_server, [sb(12, &[1]), sb(5, &[9]), sb(0, &[])].concat());
    assert!(s.feed(&to_server).is_empty());
    assert_eq!(
        s.events(),
        vec![ServerEvent::Purge(Purge::Receive), ServerEvent::Dtr(false)]
    );
    let to_client = s.out();
    assert_eq!(
        to_client,
        [
            sb(112, &[1]),
            sb(105, &[9]),
            sb(100, DEFAULT_SIGNATURE.as_bytes())
        ]
        .concat()
    );
    assert!(c.feed(&to_client).is_empty());
    assert_eq!(
        c.events(),
        vec![
            Event::Purged(Purge::Receive),
            Event::Dtr(false),
            Event::Signature(InlineBytes::from(DEFAULT_SIGNATURE)),
        ]
    );
    assert!(c.out().is_empty());
    assert_eq!(c.readiness(), Ok(()));

    // Data with 0xFF bytes, both ways.
    let data = [0xFF, b'4', b'8', 0xFF, 0xFF, b'\r', b'\n'];
    assert_eq!(s.try_write_data(&data), Ok(data.len()));
    let wire = s.out();
    assert_eq!(wire, hex("FF FF 34 38 FF FF FF FF 0D 0A"));
    assert_eq!(c.feed(&wire), data);
    assert_eq!(c.try_write_data(b"in\xFF\r"), Ok(4));
    let wire = c.out();
    assert_eq!(wire, hex("69 6E FF FF 0D"));
    assert_eq!(s.feed(&wire), b"in\xFF\r");
    assert!(c.events().is_empty());
    assert!(s.events().is_empty());
}

#[test]
fn client_server_settle_quickly() {
    let wanted = non_default();
    let mut c = Wire::new(Client::new(wanted));
    let mut s = Wire::new(Server::new(PortSettings::default()));
    // N7: quiet within 4 rounds, with the client ready.
    let rounds = pump(&mut c, &mut s, 4);
    assert!(rounds <= 4, "{rounds}");
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(c.readiness(), Ok(()));
    assert!(s.com_port_active());
    assert_eq!(s.settings(), wanted);
}

#[test]
fn negotiation_settles_after_contradictory_burst() {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    let mut s = Wire::new(Server::new(PortSettings::default()));
    // Before the first exchange each half sees BINARY enabled and disabled
    // at once: the client offers and withdraws it toward the server, the
    // server asks for it and cancels it toward the client.
    assert!(s.feed(&hex("FF FB 00 FF FC 00")).is_empty());
    assert!(c.feed(&hex("FF FD 00 FF FE 00")).is_empty());
    let rounds = pump(&mut c, &mut s, 40);
    assert!(rounds <= 40, "{rounds}");
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(c.readiness(), Ok(()));
    assert!(s.com_port_active());
}
