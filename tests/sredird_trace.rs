//! Replay of a real sredird 2.2.2 session (Debian, LP64, socat in front of a
//! pty), copied from the release audit's capture (`t6_sredird.log`): what
//! the client sent (TX) and every socket read (RX), one string per read, so
//! replies split mid-subnegotiation stay split. The capture was made with
//! the client's current 18-byte opening.
//!
//! sredird answers SET-BAUDRATE with 8 bytes (a C `long` on LP64), sends a
//! NOTIFY-MODEMSTATE burst after every exchange, reports 8 data bits and no
//! parity whatever was requested (the pty takes no other values), and
//! answers DTR and RTS with its flow control value (K5). Drives the core
//! API through the `common` driver, so it runs in every configuration.

mod common;

use common::{Wire, hex, port, reports, set};
use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, InlineBytes, ModemState,
    NotReady, Parity, PortSettings, Purge, Setting, StopBits,
};

/// The NOTIFY-MODEMSTATE burst sredird sends after every exchange.
fn modem_burst() -> Vec<Event> {
    [0x1E, 0xFE, 0xF0]
        .map(|bits| Event::ModemState(ModemState::from_bits(bits)))
        .to_vec()
}

/// One reconfiguration in the capture: the settings requested, the round
/// the client sent for them and sredird's answer (both as captured), the
/// values the answer reported, and the readiness it leaves.
struct Step {
    request: PortSettings,
    tx: &'static str,
    rx: &'static [&'static str],
    reported: PortSettings,
    readiness: Result<(), NotReady>,
}

/// The capture's reconfigurations after the opening, in order.
fn reconfigurations() -> Vec<Step> {
    let mismatched = |settings: &[Setting]| Err(NotReady::Mismatched(set(settings)));
    vec![
        // The reply is chunked mid-subnegotiation, as captured.
        Step {
            request: port(
                19_200,
                DataBits::Seven,
                Parity::Even,
                StopBits::Two,
                FlowControl::None,
            ),
            tx: "FF FA 2C 01 00 00 4B 00 FF F0 FF FA 2C 02 07 FF F0 FF FA 2C 03 03 FF F0 FF FA 2C 04 02 FF F0 FF FA 2C 05 01 FF F0",
            rx: &[
                "FF FA 2C 65",
                "00 00 4B 00 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 02 FF F0 FF",
                "FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0",
                "FF FA 2C",
                "6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            reported: port(
                19_200,
                DataBits::Eight,
                Parity::None,
                StopBits::Two,
                FlowControl::None,
            ),
            readiness: mismatched(&[Setting::DataBits, Setting::Parity]),
        },
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
                "FF FA 2C 65 00 00 04 B0 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA",
                "2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0 FF F0",
            ],
            reported: port(
                1200,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            readiness: mismatched(&[Setting::DataBits, Setting::Parity, Setting::StopBits]),
        },
        Step {
            request: port(
                57_600,
                DataBits::Six,
                Parity::Mark,
                StopBits::One,
                FlowControl::XonXoff,
            ),
            tx: "FF FA 2C 01 00 00 E1 00 FF F0 FF FA 2C 02 06 FF F0 FF FA 2C 03 04 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 02 FF F0",
            rx: &[
                "FF FA 2C 65",
                "00 00 E1 00 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA",
                "2C 69 02 FF F0 FF FA 2C 6B 1E FF F0",
                "FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            reported: port(
                57_600,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::XonXoff,
            ),
            readiness: mismatched(&[Setting::DataBits, Setting::Parity]),
        },
        Step {
            request: port(
                115_200,
                DataBits::Eight,
                Parity::Space,
                StopBits::Two,
                FlowControl::RtsCts,
            ),
            tx: "FF FA 2C 01 00 01 C2 00 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 05 FF F0 FF FA 2C 04 02 FF F0 FF FA 2C 05 03 FF F0",
            rx: &[
                "FF FA 2C 65 00 01 C2 00",
                "00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 02 FF F0 FF FA 2C 69 03 FF F0 FF FA 2C 6B 1E FF",
                "F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0",
                "FF F0",
            ],
            reported: port(
                115_200,
                DataBits::Eight,
                Parity::None,
                StopBits::Two,
                FlowControl::RtsCts,
            ),
            readiness: mismatched(&[Setting::Parity]),
        },
        Step {
            request: port(
                38_400,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            tx: "FF FA 2C 01 00 00 96 00 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0",
            rx: &[
                "FF FA 2C 65",
                "00 00 96 00 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0",
                "FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0",
                "FF FA 2C 6B",
                "FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            reported: port(
                38_400,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            readiness: Ok(()),
        },
        // sredird does not support 250000 baud: it sets 9600 instead and
        // reports that.
        Step {
            request: port(
                250_000,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            tx: "FF FA 2C 01 00 03 D0 90 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0",
            rx: &[
                "FF FA 2C 65 00 00 25 80 00 00",
                "00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E",
                "FF F0 FF FA",
                "2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            reported: port(
                9600,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            readiness: mismatched(&[Setting::BaudRate]),
        },
        // DCD and DSR flow control are answered with "none".
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
                "FF FA 2C 65 00 00 25 80 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0 FF F0",
            ],
            reported: port(
                9600,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            readiness: mismatched(&[Setting::FlowControl]),
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
                "FF FA 2C 65 00 00 25 80 00",
                "00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF",
                "FA 2C 6B 1E FF F0",
                "FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            reported: port(
                9600,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            readiness: mismatched(&[Setting::FlowControl]),
        },
        Step {
            request: PortSettings::default(),
            tx: "FF FA 2C 01 00 00 25 80 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0",
            rx: &[
                "FF FA 2C 65 00 00 25 80 00 00",
                "00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0 FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF",
                "FA 2C 6B 1E FF F0",
                "FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            reported: PortSettings::default(),
            readiness: Ok(()),
        },
    ]
}

/// A core call the capture made.
type Call = fn(&mut Client) -> Result<(), CommandError>;

#[test]
fn sredird_2_2_2_session() {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    assert_eq!(
        c.out(),
        hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C")
    );

    // sredird's answers to the opening include its own WILL ECHO, which is
    // refused; its DO COM-PORT activates the client, which sends the round.
    assert!(
        c.feed_reads(&["FF FB 00 FF FD 00 FF FB 01 FF FB 03 FF FD 03 FF FD 2C"])
            .is_empty()
    );
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(c.events(), [Event::ComPortActive]);
    assert_eq!(
        c.out(),
        hex(
            "FF FE 01 FF FA 2C 01 00 00 25 80 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0"
        )
    );

    // Its WILL COM-PORT answers our DO: nothing to say, still Active. Then
    // its first modem states.
    assert!(
        c.feed_reads(&[
            "FF FB 2C FF FA 2C 6B 88 FF F0 FF FA 2C 6B F7 FF F0",
            "FF FA 2C 6B F0 FF F0",
        ])
        .is_empty()
    );
    assert!(c.out().is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(
        c.events(),
        [0x88, 0xF7, 0xF0].map(|bits| Event::ModemState(ModemState::from_bits(bits)))
    );

    // The 8-byte SET-BAUDRATE reply decodes (D2); the stop bits and flow
    // control replies follow in the next read.
    assert!(
        c.feed_reads(&[
            "FF FA 2C 65 00 00 25 80 00 00 00 00 FF F0 FF FA 2C 66 08 FF F0 FF FA 2C 67 01 FF F0"
        ])
        .is_empty()
    );
    assert_eq!(
        c.readiness(),
        Err(NotReady::Pending(set(&[
            Setting::StopBits,
            Setting::FlowControl
        ])))
    );
    assert!(
        c.feed_reads(&[
            "FF FA 2C 68 01 FF F0 FF FA 2C 69 01 FF F0 FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0 FF FA 2C 6B F0 FF F0"
        ])
        .is_empty()
    );
    assert!(c.out().is_empty());
    assert_eq!(
        c.events(),
        [reports(PortSettings::default()), modem_burst()].concat()
    );
    assert_eq!(c.readiness(), Ok(()), "ready at 9600 8N1");
    assert_eq!(
        c.reported_settings().complete(),
        Some(PortSettings::default())
    );
    assert_eq!(c.modem_state(), Some(ModemState::from_bits(0xF0)));

    // Every further reconfiguration of the capture: sredird answers each
    // setting once, so every request is answered and the readiness says
    // whether it reported what was asked for (R4, R6, R7).
    for step in reconfigurations() {
        let request = step.request;
        let events = c.exchange(|c| c.try_set_port_settings(request), step.tx, step.rx);
        assert_eq!(
            events,
            [reports(step.reported), modem_burst()].concat(),
            "{request:?}"
        );
        assert_eq!(c.readiness(), step.readiness, "{request:?}");
        assert_eq!(
            c.reported_settings().complete(),
            Some(step.reported),
            "{request:?}"
        );
        assert_eq!(c.requested_settings(), request);
    }

    // DTR and RTS are answered with the flow control value: reported as
    // flow control, which matches the request, so the client stays ready.
    // BREAK is echoed. None of them touches the settings accounting.
    let controls: [(Call, &str, &[&str], Event); 9] = [
        (
            |c| c.try_set_dtr(false),
            "FF FA 2C 05 09 FF F0",
            &[
                "FF FA 2C 69 01 FF F0 FF FA",
                "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            Event::FlowControl(FlowControl::None),
        ),
        (
            |c| c.try_set_rts(false),
            "FF FA 2C 05 0C FF F0",
            &[
                "FF FA 2C 69 01 FF F0 FF FA",
                "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            Event::FlowControl(FlowControl::None),
        ),
        (
            |c| c.try_set_break(true),
            "FF FA 2C 05 05 FF F0",
            &[
                "FF FA 2C 69 05 FF F0 FF FA",
                "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            Event::Break(true),
        ),
        (
            |c| c.try_set_break(false),
            "FF FA 2C 05 06 FF F0",
            &[
                "FF FA 2C 69 06 FF F0 FF FA",
                "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0",
                "FF F0",
            ],
            Event::Break(false),
        ),
        (
            |c| c.try_set_dtr(true),
            "FF FA 2C 05 08 FF F0",
            &[
                "FF FA 2C 69 01 FF F0",
                "FF FA 2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            Event::FlowControl(FlowControl::None),
        ),
        (
            |c| c.try_set_rts(true),
            "FF FA 2C 05 0B FF F0",
            &[
                "FF FA 2C 69 01 FF F0 FF FA",
                "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            Event::FlowControl(FlowControl::None),
        ),
        (
            |c| c.try_purge(Purge::Receive),
            "FF FA 2C 0C 01 FF F0",
            &[
                "FF FA 2C 70 01 FF F0 FF FA",
                "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            Event::Purged(Purge::Receive),
        ),
        (
            |c| c.try_purge(Purge::Transmit),
            "FF FA 2C 0C 02 FF F0",
            &[
                "FF FA 2C 70 02 FF F0 FF FA",
                "2C 6B 1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C",
                "6B F0 FF F0",
            ],
            Event::Purged(Purge::Transmit),
        ),
        (
            |c| c.try_purge(Purge::Both),
            "FF FA 2C 0C 03 FF F0",
            &[
                "FF FA 2C 70 03 FF F0 FF FA 2C 6B",
                "1E FF F0 FF FA 2C 6B FE FF F0",
                "FF FA 2C 6B F0 FF F0",
            ],
            Event::Purged(Purge::Both),
        ),
    ];
    for (call, tx, rx, reported) in controls {
        let events = c.exchange(call, tx, rx);
        assert_eq!(events, [vec![reported], modem_burst()].concat(), "{tx}");
        assert_eq!(c.readiness(), Ok(()), "after {tx}");
    }

    // The signature arrives in two reads.
    let events = c.exchange(
        |c| c.try_request_signature(),
        "FF FA 2C 00 FF F0",
        &[
            "FF FA 2C 64 53 52 65 64",
            "69 72 64 20 32 2E 32 2E 32 20 2F 64 65 76 2F 74 74 79 56 30 FF F0 FF FA 2C 6B 1E FF F0",
            "FF FA",
            "2C 6B FE FF F0",
            "FF FA 2C 6B F0 FF F0",
        ],
    );
    let signature = Event::Signature(InlineBytes::from("SRedird 2.2.2 /dev/ttyV0"));
    assert_eq!(events, [vec![signature], modem_burst()].concat());

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
    assert_eq!(c.modem_state(), Some(ModemState::from_bits(0xF0)));
    assert!(c.events().is_empty());
}
