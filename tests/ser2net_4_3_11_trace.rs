//! Replay of a real ser2net 4.3.11 session (Debian bookworm, `telnet(rfc2217)`
//! accepter in front of a pty), copied from the release audit's capture
//! (`t7_s2n43.log`): what the client sent (TX) and every socket read (RX),
//! one string per read. The capture was made with the client's current
//! 18-byte opening.
//!
//! ser2net 4.3.11 sends a NOTIFY-MODEMSTATE before refusing our DO
//! COM-PORT, drops the reply for 1.5 stop bits, answers DCD and DSR flow
//! control with 14 (an inbound value), never answers DTR or RTS on a pty,
//! and answers BREAK-OFF with BREAK-ON. Drives the core API through the
//! `common` driver, so it runs in every configuration.

mod common;

use common::{Wire, hex, port, reports, set};
use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, InlineBytes, ModemState,
    NotReady, Parity, PortSettings, Purge, Setting, StopBits,
};

/// One reconfiguration in the capture: the settings requested, the round
/// the client sent for them and ser2net's answer (both as captured), the
/// events the answer produced, and the readiness it leaves.
struct Step {
    request: PortSettings,
    tx: &'static str,
    rx: &'static [&'static str],
    events: Vec<Event>,
    readiness: Result<(), NotReady>,
}

/// A reconfiguration that ser2net answered with exactly the values requested.
fn confirmed(request: PortSettings, tx: &'static str, rx: &'static [&'static str]) -> Step {
    Step {
        request,
        tx,
        rx,
        events: reports(request),
        readiness: Ok(()),
    }
}

/// The capture's reconfigurations after the opening, in order.
fn reconfigurations() -> Vec<Step> {
    // ser2net's answer to DCD and DSR flow control: 14, an inbound value,
    // which the client cannot decode as a flow control setting (K4).
    let inbound_flow = || {
        vec![
            Event::BaudRate(9600),
            Event::DataBits(DataBits::Eight),
            Event::Parity(Parity::None),
            Event::StopBits(StopBits::One),
            Event::Unknown(InlineBytes::new(&[44, 105, 14])),
        ]
    };
    vec![
        confirmed(
            port(
                19_200,
                DataBits::Seven,
                Parity::Even,
                StopBits::Two,
                FlowControl::None,
            ),
            "FF FA 2C 01 00 00 4B 00 FF F0 FF FA 2C 02 07 FF F0 FF FA 2C 03 03 FF F0 FF FA 2C 04 02 FF F0 FF FA 2C 05 01 FF F0",
            &[
                "FF FA 2C 65 00 00 4B 00 FF F0 FF FA 2C 66 07 FF F0 FF FA 2C 67 03 FF F0 FF FA 2C 68 02 FF F0 FF FA 2C 69 01 FF F0",
            ],
        ),
        // No reply for 1.5 stop bits: the client waits for it.
        Step {
            request: port(
                1200,
                DataBits::Five,
                Parity::Odd,
                StopBits::OnePointFive,
                FlowControl::None,
            ),
            tx: "FF FA 2C 01 00 00 04 B0 FF F0 FF FA 2C 02 05 FF F0 FF FA 2C 03 02 FF F0 FF FA 2C 04 03 FF F0 FF FA 2C 05 01 FF F0",
            rx: &[
                "FF FA 2C 65 00 00 04 B0 FF F0 FF FA 2C 66 05 FF F0 FF FA 2C 67 02 FF F0 FF FA 2C 69 01 FF F0",
            ],
            events: vec![
                Event::BaudRate(1200),
                Event::DataBits(DataBits::Five),
                Event::Parity(Parity::Odd),
                Event::FlowControl(FlowControl::None),
            ],
            readiness: Err(NotReady::Pending(set(&[Setting::StopBits]))),
        },
        // The next request asks for 1 stop bit: its reply settles the
        // unanswered request too (R4).
        confirmed(
            port(
                57_600,
                DataBits::Six,
                Parity::Mark,
                StopBits::One,
                FlowControl::XonXoff,
            ),
            "FF FA 2C 01 00 00 E1 00 FF F0 FF FA 2C 02 06 FF F0 FF FA 2C 03 04 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 02 FF F0",
            &[
                "FF FA 2C 65 00 00 E1 00 FF F0 FF FA 2C 66 06 FF F0 FF FA 2C 67 04 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 02 FF F0",
            ],
        ),
        confirmed(
            port(
                115_200,
                DataBits::Eight,
                Parity::Space,
                StopBits::Two,
                FlowControl::RtsCts,
            ),
            "FF FA 2C 01 00 01 C2 00 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 05 FF F0 FF FA 2C 04 02 FF F0 FF FA 2C 05 03 FF F0",
            &[
                "FF FA 2C 65 00 01 C2 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 05 FF F0 FF FA 2C 68 02 FF F0 FF FA 2C 69 03 FF F0",
            ],
        ),
        confirmed(
            port(
                38_400,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            "FF FA 2C 01 00 00 96 00 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0",
            &[
                "FF FA 2C 65 00 00 96 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0",
            ],
        ),
        // The answer's first read is a lone IAC.
        confirmed(
            port(
                250_000,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            "FF FA 2C 01 00 03 D0 90 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0",
            &[
                "FF",
                "FA 2C 65 00 03 D0 90 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0",
            ],
        ),
        Step {
            request: port(
                9600,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::Dcd,
            ),
            tx: "FF FA 2C 01 00 00 25 80 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 11 FF F0",
            rx: &[
                "FF FA 2C 65 00 00 25 80 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 0E FF F0",
            ],
            events: inbound_flow(),
            readiness: Err(NotReady::Mismatched(set(&[Setting::FlowControl]))),
        },
        Step {
            request: port(
                9600,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::Dsr,
            ),
            tx: "FF FA 2C 01 00 00 25 80 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 13 FF F0",
            rx: &[
                "FF FA 2C 65 00 00 25 80 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 0E FF F0",
            ],
            events: inbound_flow(),
            readiness: Err(NotReady::Mismatched(set(&[Setting::FlowControl]))),
        },
        confirmed(
            PortSettings::default(),
            "FF FA 2C 01 00 00 25 80 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0",
            &[
                "FF FA 2C 65 00 00 25 80 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0",
            ],
        ),
    ]
}

/// A core call the capture made.
type Call = fn(&mut Client) -> Result<(), CommandError>;

#[test]
fn ser2net_4_3_11_session() {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    assert_eq!(
        c.out(),
        hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C")
    );

    // ser2net's answers to the opening, in one read: its WILL ECHO is
    // refused, its DO COM-PORT activates the client, which sends the round.
    // A NOTIFY-MODEMSTATE comes before its WONT COM-PORT, which only answers
    // our DO and leaves the client Active (L1).
    assert!(
        c.feed_reads(&[
            "FF FB 03 FF FD 03 FF FB 01 FF FE 01 FF FD 00 FF FB 00 FF FD 2C FF FD 03 FF FA 2C 6B 00 FF F0 FF FC 2C"
        ])
        .is_empty()
    );
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(
        c.events(),
        [
            Event::ComPortActive,
            Event::ModemState(ModemState::from_bits(0))
        ]
    );
    assert_eq!(c.modem_state(), Some(ModemState::from_bits(0)));
    assert_eq!(
        c.out(),
        hex(
            "FF FE 01 FF FA 2C 01 00 00 25 80 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0"
        )
    );
    assert!(
        c.feed_reads(&[
            "FF FA 2C 65 00 00 25 80 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0"
        ])
        .is_empty()
    );
    assert!(c.out().is_empty());
    assert_eq!(c.events(), reports(PortSettings::default()));
    assert_eq!(c.readiness(), Ok(()), "ready at 9600 8N1");

    for step in reconfigurations() {
        let request = step.request;
        let events = c.exchange(|c| c.try_set_port_settings(request), step.tx, step.rx);
        assert_eq!(events, step.events, "{request:?}");
        assert_eq!(c.readiness(), step.readiness, "{request:?}");
    }
    assert_eq!(
        c.reported_settings().complete(),
        Some(PortSettings::default())
    );

    // DTR and RTS get no reply on a pty; BREAK-OFF is answered with
    // BREAK-ON. Neither touches the settings accounting, so the client
    // stays ready.
    let controls: [(Call, &str, &[&str], Option<Event>); 9] = [
        (|c| c.try_set_dtr(false), "FF FA 2C 05 09 FF F0", &[], None),
        (|c| c.try_set_rts(false), "FF FA 2C 05 0C FF F0", &[], None),
        (
            |c| c.try_set_break(true),
            "FF FA 2C 05 05 FF F0",
            &["FF FA 2C 69 05 FF F0"],
            Some(Event::Break(true)),
        ),
        (
            |c| c.try_set_break(false),
            "FF FA 2C 05 06 FF F0",
            &["FF FA 2C 69 05 FF F0"],
            Some(Event::Break(true)),
        ),
        (|c| c.try_set_dtr(true), "FF FA 2C 05 08 FF F0", &[], None),
        (|c| c.try_set_rts(true), "FF FA 2C 05 0B FF F0", &[], None),
        (
            |c| c.try_purge(Purge::Receive),
            "FF FA 2C 0C 01 FF F0",
            &["FF FA 2C 70 01 FF F0"],
            Some(Event::Purged(Purge::Receive)),
        ),
        (
            |c| c.try_purge(Purge::Transmit),
            "FF FA 2C 0C 02 FF F0",
            &["FF FA 2C 70 02 FF F0"],
            Some(Event::Purged(Purge::Transmit)),
        ),
        (
            |c| c.try_purge(Purge::Both),
            "FF FA 2C 0C 03 FF F0",
            &["FF FA 2C 70 03 FF F0"],
            Some(Event::Purged(Purge::Both)),
        ),
    ];
    for (call, tx, rx, reported) in controls {
        let events = c.exchange(call, tx, rx);
        assert_eq!(events, Vec::from_iter(reported), "{tx}");
        assert_eq!(c.readiness(), Ok(()), "after {tx}");
    }

    let events = c.exchange(
        |c| c.try_request_signature(),
        "FF FA 2C 00 FF F0",
        &["FF FA 2C 64 73 65 72 32 6E 65 74 FF F0"],
    );
    assert_eq!(events, [Event::Signature(InlineBytes::from("ser2net"))]);

    // Every byte value, two more 0xFF and line ends go out escaped; the
    // capture's device received the 262 bytes intact. (The capture logs the
    // device's echo only as "client received 262 bytes, equal=True".)
    let data: Vec<u8> = (0..=0xFF)
        .chain([0xFF, 0xFF, 0x0D, 0x0A, 0x0D, 0x00])
        .collect();
    let mut wire: Vec<u8> = (0..0xFF).collect();
    wire.extend(hex("FF FF FF FF FF FF 0D 0A 0D 00"));
    assert_eq!(c.write_all(&data), wire);
    assert_eq!(c.readiness(), Ok(()));
    assert!(c.events().is_empty());
}
