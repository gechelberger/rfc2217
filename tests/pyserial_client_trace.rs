//! Replays of pyserial 3.5's RFC 2217 client (`serial.rfc2217`) toward a
//! server, copied from the release audit's captures (`server.log`, and
//! `server_poll.log` with `?poll_modem`): each `<-` line is one read of the
//! client's bytes, and each `->` line the reply the capture's server sent.
//!
//! The captures were made against this crate's server before this release,
//! which sent no opening, so they hold pyserial's own offers and none of its
//! answers: a live re-capture against the new opening would also contain
//! pyserial's answers to WILL/DO BINARY. The server's replies differ from
//! the captured ones by design where noted (W8-W11: it queues the 12-byte
//! opening, which leaves COM-PORT to the client, refuses DO COM-PORT with
//! WONT, and reports the modem state it is given on activation and when
//! polled); every other reply is the captured one. Drives the core API
//! through the `common` driver, so it runs in every configuration.

mod common;

use common::{Wire, hex, port};
use rfc2217::{
    DataBits, FlowControl, ModemState, Parity, PortSettings, Purge, Server, ServerEvent, StopBits,
};

/// One read of the client's bytes, the server's reply to it and the events
/// it produced.
type Exchange = (&'static [u8], &'static [u8], Vec<ServerEvent>);

/// Feed the client's reads in order: none carries payload, and each gets
/// exactly its reply and events.
fn replay(s: &mut Wire<Server>, exchanges: Vec<Exchange>) {
    for (read, reply, events) in exchanges {
        assert!(s.feed(read).is_empty(), "payload in {read:?}");
        assert_eq!(s.out(), reply, "reply to {read:?}");
        assert_eq!(s.events(), events, "events of {read:?}");
    }
}

/// A new server, its opening checked and drained, that has been told the
/// port's modem lines (CTS and DSR on) before any client enabled
/// COM-PORT-OPTION: stored, nothing sent (M3).
fn server() -> Wire<Server> {
    let mut s = Wire::new(Server::new(PortSettings::default()));
    assert_eq!(s.out(), hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03"));
    assert_eq!(
        s.try_set_modem_state(ModemState::CTS | ModemState::DSR),
        Ok(())
    );
    assert!(s.out().is_empty());
    s
}

/// pyserial's opening, the same in both captures: DO ECHO, refused as
/// captured; then WILL/DO SGA, which answer the server's opening, DO
/// COM-PORT, refused, and WILL COM-PORT, which the server accepts with DO
/// (as the capture's server did) and which activates it, and the server
/// reports the modem state (M2). The capture's server answered the
/// second read with `[255, 253, 3, 255, 251, 3, 255, 251, 44, 255, 253, 44]`
/// and never sent a modem state, so pyserial's `cts` later failed with
/// "remote sends no NOTIFY_MODEMSTATE" (the client's log of `server.log`).
fn opening() -> Vec<Exchange> {
    vec![
        (&[255, 253, 1], &[255, 252, 1], vec![]),
        (
            &[255, 251, 3, 255, 253, 3, 255, 253, 44, 255, 251, 44],
            &[
                255, 252, 44, // WONT COM-PORT
                255, 253, 44, // DO COM-PORT
                255, 250, 44, 107, 0x30, 255, 240, // NOTIFY-MODEMSTATE CTS | DSR
            ],
            vec![ServerEvent::ComPortActive],
        ),
    ]
}

#[test]
fn pyserial_client_session() {
    let mut s = server();
    replay(&mut s, opening());

    // `serial_for_url(..., baudrate=19200, bytesize=7, parity="E",
    // stopbits=2, rtscts=True)`: one SettingsChanged per request, with the
    // intermediate settings (V4); then DTR on and both purges.
    let baud = PortSettings {
        baud_rate: 19_200,
        ..PortSettings::default()
    };
    let data_bits = PortSettings {
        data_bits: DataBits::Seven,
        ..baud
    };
    let parity = PortSettings {
        parity: Parity::Even,
        ..data_bits
    };
    let stop_bits = PortSettings {
        stop_bits: StopBits::Two,
        ..parity
    };
    let flow_control = PortSettings {
        flow_control: FlowControl::RtsCts,
        ..stop_bits
    };
    replay(
        &mut s,
        vec![
            (
                &[255, 250, 44, 1, 0, 0, 75, 0, 255, 240],
                &[255, 250, 44, 101, 0, 0, 75, 0, 255, 240],
                vec![ServerEvent::SettingsChanged(baud)],
            ),
            (
                &[
                    255, 250, 44, 2, 7, 255, 240, 255, 250, 44, 3, 3, 255, 240, 255, 250, 44, 4, 2,
                    255, 240,
                ],
                &[
                    255, 250, 44, 102, 7, 255, 240, 255, 250, 44, 103, 3, 255, 240, 255, 250, 44,
                    104, 2, 255, 240,
                ],
                vec![
                    ServerEvent::SettingsChanged(data_bits),
                    ServerEvent::SettingsChanged(parity),
                    ServerEvent::SettingsChanged(stop_bits),
                ],
            ),
            (
                &[255, 250, 44, 5, 3, 255, 240],
                &[255, 250, 44, 105, 3, 255, 240],
                vec![ServerEvent::SettingsChanged(flow_control)],
            ),
            (
                &[255, 250, 44, 5, 8, 255, 240],
                &[255, 250, 44, 105, 8, 255, 240],
                vec![ServerEvent::Dtr(true)],
            ),
            (
                &[255, 250, 44, 12, 1, 255, 240],
                &[255, 250, 44, 112, 1, 255, 240],
                vec![ServerEvent::Purge(Purge::Receive)],
            ),
            (
                &[255, 250, 44, 12, 2, 255, 240],
                &[255, 250, 44, 112, 2, 255, 240],
                vec![ServerEvent::Purge(Purge::Transmit)],
            ),
        ],
    );

    // `write(b"hello\xff\r\n")`: unescaped on the way in; the capture's
    // server echoed it, escaped on the way out.
    assert_eq!(
        s.feed(&[104, 101, 108, 108, 111, 255, 255, 13, 10]),
        b"hello\xFF\r\n"
    );
    assert_eq!(
        s.write_all(b"hello\xFF\r\n"),
        [104, 101, 108, 108, 111, 255, 255, 13, 10]
    );

    // `baudrate = 115200` resends every setting: only the rate changes.
    // Then DTR and RTS off, a BREAK pulse and a purge.
    let faster = PortSettings {
        baud_rate: 115_200,
        ..flow_control
    };
    replay(
        &mut s,
        vec![
            (
                &[255, 250, 44, 1, 0, 1, 194, 0, 255, 240],
                &[255, 250, 44, 101, 0, 1, 194, 0, 255, 240],
                vec![ServerEvent::SettingsChanged(faster)],
            ),
            (
                &[255, 250, 44, 2, 7, 255, 240, 255, 250, 44, 3, 3, 255, 240],
                &[
                    255, 250, 44, 102, 7, 255, 240, 255, 250, 44, 103, 3, 255, 240,
                ],
                vec![],
            ),
            (
                &[255, 250, 44, 4, 2, 255, 240],
                &[255, 250, 44, 104, 2, 255, 240],
                vec![],
            ),
            (
                &[255, 250, 44, 5, 3, 255, 240],
                &[255, 250, 44, 105, 3, 255, 240],
                vec![],
            ),
            (
                &[255, 250, 44, 5, 9, 255, 240],
                &[255, 250, 44, 105, 9, 255, 240],
                vec![ServerEvent::Dtr(false)],
            ),
            (
                &[255, 250, 44, 5, 12, 255, 240],
                &[255, 250, 44, 105, 12, 255, 240],
                vec![ServerEvent::Rts(false)],
            ),
            (
                &[255, 250, 44, 5, 5, 255, 240],
                &[255, 250, 44, 105, 5, 255, 240],
                vec![ServerEvent::Break(true)],
            ),
            (
                &[255, 250, 44, 5, 6, 255, 240],
                &[255, 250, 44, 105, 6, 255, 240],
                vec![ServerEvent::Break(false)],
            ),
            (
                &[255, 250, 44, 12, 1, 255, 240],
                &[255, 250, 44, 112, 1, 255, 240],
                vec![ServerEvent::Purge(Purge::Receive)],
            ),
        ],
    );
    assert_eq!(
        s.settings(),
        port(
            115_200,
            DataBits::Seven,
            Parity::Even,
            StopBits::Two,
            FlowControl::RtsCts
        )
    );
    assert!(s.com_port_active());
}

#[test]
fn pyserial_client_poll_session() {
    let mut s = server();
    replay(&mut s, opening());

    // The default settings: acknowledged, nothing changed. Then DTR and RTS
    // on and both purges.
    replay(
        &mut s,
        vec![
            (
                &[255, 250, 44, 1, 0, 0, 37, 128, 255, 240],
                &[255, 250, 44, 101, 0, 0, 37, 128, 255, 240],
                vec![],
            ),
            (
                &[
                    255, 250, 44, 2, 8, 255, 240, 255, 250, 44, 3, 1, 255, 240, 255, 250, 44, 4, 1,
                    255, 240,
                ],
                &[
                    255, 250, 44, 102, 8, 255, 240, 255, 250, 44, 103, 1, 255, 240, 255, 250, 44,
                    104, 1, 255, 240,
                ],
                vec![],
            ),
            (
                &[255, 250, 44, 5, 1, 255, 240],
                &[255, 250, 44, 105, 1, 255, 240],
                vec![],
            ),
            (
                &[255, 250, 44, 5, 8, 255, 240],
                &[255, 250, 44, 105, 8, 255, 240],
                vec![ServerEvent::Dtr(true)],
            ),
            (
                &[255, 250, 44, 5, 11, 255, 240],
                &[255, 250, 44, 105, 11, 255, 240],
                vec![ServerEvent::Rts(true)],
            ),
            (
                &[255, 250, 44, 12, 1, 255, 240],
                &[255, 250, 44, 112, 1, 255, 240],
                vec![ServerEvent::Purge(Purge::Receive)],
            ),
            (
                &[255, 250, 44, 12, 2, 255, 240],
                &[255, 250, 44, 112, 2, 255, 240],
                vec![ServerEvent::Purge(Purge::Transmit)],
            ),
        ],
    );

    // `?poll_modem`: an empty NOTIFY-MODEMSTATE asks for the modem state,
    // which the server reports (M2). The capture's server sent nothing.
    replay(
        &mut s,
        vec![(
            &[255, 250, 44, 7, 255, 240],
            &[255, 250, 44, 107, 0x30, 255, 240],
            vec![],
        )],
    );
    assert_eq!(s.settings(), PortSettings::default());
    assert!(s.com_port_active());
}
