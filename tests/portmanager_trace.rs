//! Replay of a real session against pyserial 3.5's `rfc2217.PortManager`
//! (the server of pyserial's `examples/rfc2217_server.py`, run on one end
//! of a socat pty pair, with simulated modem lines), copied from the
//! release audit's capture (`t1.log`): what the client sent (TX) and every
//! socket read (RX), one string per read. The capture was made with the
//! client's current 18-byte opening.
//!
//! PortManager answers one negotiation per read and offers WILL COM-PORT
//! before it answers our WILL with DO, acknowledges every setting
//! (including 1.5 stop bits) and every DTR, RTS and BREAK change with the
//! value set, does not answer SIGNATURE, and reports modem line changes as
//! it polls them. Drives the core API through the `common` driver, so it
//! runs in every configuration.

mod common;

use common::{Wire, hex, port, reports};
use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, ModemState, Parity,
    PortSettings, Purge, StopBits,
};

/// A core call the capture made.
type Call = fn(&mut Client) -> Result<(), CommandError>;

#[test]
fn pyserial_portmanager_session() {
    let mut c = Wire::new(Client::new(PortSettings::default()));
    assert_eq!(
        c.out(),
        hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C")
    );

    // PortManager's own WILL ECHO comes first and is refused.
    assert!(c.feed_reads(&["FF FB 01"]).is_empty());
    assert_eq!(c.out(), hex("FF FE 01"));

    // Its WILL COM-PORT, which answers our DO, comes before its DO
    // COM-PORT and does not activate the client (L1); neither do the
    // answers to our BINARY and SGA offers.
    assert!(
        c.feed_reads(&["FF FB 03", "FF FD 00", "FF FB 2C", "FF FB 00", "FF FD 03"])
            .is_empty()
    );
    assert_eq!(c.com_port_state(), ComPortState::Negotiating);
    assert!(c.out().is_empty());
    assert!(c.events().is_empty());

    // Its DO COM-PORT activates the client, which sends the round.
    assert!(c.feed_reads(&["FF FD 2C"]).is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(c.events(), [Event::ComPortActive]);
    assert_eq!(
        c.out(),
        hex(
            "FF FA 2C 01 00 00 25 80 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0"
        )
    );

    // Two NOTIFY-MODEMSTATE 0, then one read per settings reply.
    assert!(
        c.feed_reads(&["FF FA 2C 6B 00 FF F0", "FF FA 2C 6B 00 FF F0"])
            .is_empty()
    );
    assert_eq!(
        c.events(),
        vec![Event::ModemState(ModemState::from_bits(0)); 2]
    );
    assert_eq!(c.modem_state(), Some(ModemState::from_bits(0)));
    assert!(
        c.feed_reads(&[
            "FF FA 2C 65 00 00 25 80 FF F0",
            "FF FA 2C 66 08 FF F0",
            "FF FA 2C 67 01 FF F0",
            "FF FA 2C 68 01 FF F0",
            "FF FA 2C 69 01 FF F0",
        ])
        .is_empty()
    );
    assert!(c.out().is_empty());
    assert_eq!(c.events(), reports(PortSettings::default()));
    assert_eq!(c.readiness(), Ok(()));

    // Every further reconfiguration is acknowledged in full, 1.5 stop bits
    // (`68 03`) included, so the client is ready after each.
    let reconfigurations: [(PortSettings, &str, &[&str]); 5] = [
        (
            port(
                19_200,
                DataBits::Seven,
                Parity::Even,
                StopBits::Two,
                FlowControl::None,
            ),
            "FF FA 2C 01 00 00 4B 00 FF F0 FF FA 2C 02 07 FF F0 FF FA 2C 03 03 FF F0 FF FA 2C 04 02 FF F0 FF FA 2C 05 01 FF F0",
            &[
                "FF FA 2C 65 00 00 4B 00 FF F0",
                "FF FA 2C 66 07 FF F0",
                "FF FA 2C 67 03 FF F0",
                "FF FA 2C 68 02 FF F0",
                "FF FA 2C 69 01 FF F0",
            ],
        ),
        (
            port(
                1200,
                DataBits::Five,
                Parity::Odd,
                StopBits::OnePointFive,
                FlowControl::None,
            ),
            "FF FA 2C 01 00 00 04 B0 FF F0 FF FA 2C 02 05 FF F0 FF FA 2C 03 02 FF F0 FF FA 2C 04 03 FF F0 FF FA 2C 05 01 FF F0",
            &[
                "FF FA 2C 65 00 00 04 B0 FF F0",
                "FF FA 2C 66 05 FF F0",
                "FF FA 2C 67 02 FF F0",
                "FF FA 2C 68 03 FF F0",
                "FF FA 2C 69 01 FF F0",
            ],
        ),
        (
            port(
                57_600,
                DataBits::Six,
                Parity::Mark,
                StopBits::One,
                FlowControl::XonXoff,
            ),
            "FF FA 2C 01 00 00 E1 00 FF F0 FF FA 2C 02 06 FF F0 FF FA 2C 03 04 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 02 FF F0",
            &[
                "FF FA 2C 65 00 00 E1 00 FF F0",
                "FF FA 2C 66 06 FF F0",
                "FF FA 2C 67 04 FF F0",
                "FF FA 2C 68 01 FF F0",
                "FF FA 2C 69 02 FF F0",
            ],
        ),
        (
            port(
                115_200,
                DataBits::Eight,
                Parity::Space,
                StopBits::Two,
                FlowControl::RtsCts,
            ),
            "FF FA 2C 01 00 01 C2 00 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 05 FF F0 FF FA 2C 04 02 FF F0 FF FA 2C 05 03 FF F0",
            &[
                "FF FA 2C 65 00 01 C2 00 FF F0",
                "FF FA 2C 66 08 FF F0",
                "FF FA 2C 67 05 FF F0",
                "FF FA 2C 68 02 FF F0",
                "FF FA 2C 69 03 FF F0",
            ],
        ),
        (
            port(
                38_400,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
                FlowControl::None,
            ),
            "FF FA 2C 01 00 00 96 00 FF F0 FF FA 2C 02 08 FF F0 FF FA 2C 03 01 FF F0 FF FA 2C 04 01 FF F0 FF FA 2C 05 01 FF F0",
            &[
                "FF FA 2C 65 00 00 96 00 FF F0",
                "FF FA 2C 66 08 FF F0",
                "FF FA 2C 67 01 FF F0",
                "FF FA 2C 68 01 FF F0",
                "FF FA 2C 69 01 FF F0",
            ],
        ),
    ];
    for (request, tx, rx) in reconfigurations {
        let events = c.exchange(|c| c.try_set_port_settings(request), tx, rx);
        assert_eq!(events, reports(request), "{request:?}");
        assert_eq!(c.readiness(), Ok(()), "{request:?}");
    }

    // DTR, RTS and BREAK are echoed and reported as such (K2), purges
    // likewise; none of them touches the settings accounting. SIGNATURE is
    // not answered, so nothing is reported.
    let controls: [(Call, &str, &[&str], Option<Event>); 9] = [
        (
            |c| c.try_set_dtr(false),
            "FF FA 2C 05 09 FF F0",
            &["FF FA 2C 69 09 FF F0"],
            Some(Event::Dtr(false)),
        ),
        (
            |c| c.try_set_rts(false),
            "FF FA 2C 05 0C FF F0",
            &["FF FA 2C 69 0C FF F0"],
            Some(Event::Rts(false)),
        ),
        (
            |c| c.try_set_break(true),
            "FF FA 2C 05 05 FF F0",
            &["FF FA 2C 69 05 FF F0"],
            Some(Event::Break(true)),
        ),
        (
            |c| c.try_set_break(false),
            "FF FA 2C 05 06 FF F0",
            &["FF FA 2C 69 06 FF F0"],
            Some(Event::Break(false)),
        ),
        (
            |c| c.try_set_dtr(true),
            "FF FA 2C 05 08 FF F0",
            &["FF FA 2C 69 08 FF F0"],
            Some(Event::Dtr(true)),
        ),
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
        (
            |c| c.try_request_signature(),
            "FF FA 2C 00 FF F0",
            &[],
            None,
        ),
    ];
    for (call, tx, rx, reported) in controls {
        let events = c.exchange(call, tx, rx);
        assert_eq!(events, Vec::from_iter(reported), "{tx}");
        assert_eq!(c.readiness(), Ok(()), "after {tx}");
    }

    // All 256 byte values, written in more than one `try_write_data` call:
    // the wire carries them with the 0xFF escaped, as captured (`00` to
    // `FE`, then `FF FF`).
    let data: Vec<u8> = (0..=0xFF).collect();
    let mut wire: Vec<u8> = (0..0xFF).collect();
    wire.extend(hex("FF FF"));
    assert_eq!(c.write_all(&data), wire);

    // The capture's device end wrote them back, followed by `FF FF 00`,
    // and PortManager sent that escaped, in two reads: `00`, then `01` to
    // `FE` and `FF FF FF FF FF FF 00`.
    let mut second: Vec<u8> = (0x01..0xFF).collect();
    second.extend(hex("FF FF FF FF FF FF 00"));
    let mut payload = c.feed(&hex("00"));
    payload.extend(c.feed(&second));
    assert_eq!(payload, [data, hex("FF FF 00")].concat());
    assert!(c.out().is_empty());
    assert!(c.events().is_empty());

    // Modem line changes, as PortManager polled them: CTS rose, then DSR
    // and CD rose, then CTS fell; each notification replaces the cached
    // state (F2).
    for (read, bits) in [
        ("FF FA 2C 6B 11 FF F0", 0x11),
        ("FF FA 2C 6B BA FF F0", 0xBA),
        ("FF FA 2C 6B A1 FF F0", 0xA1),
    ] {
        assert!(c.feed_reads(&[read]).is_empty());
        let state = ModemState::from_bits(bits);
        assert_eq!(c.events(), [Event::ModemState(state)], "{read}");
        assert_eq!(c.modem_state(), Some(state));
    }
    assert!(c.out().is_empty());
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(
        c.reported_settings().complete(),
        Some(port(
            38_400,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
            FlowControl::None
        ))
    );
}
