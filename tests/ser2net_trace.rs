//! Replay of a real ser2net 4.6.4 session (Debian trixie, `telnet(rfc2217)`
//! accepter in front of a pty), captured byte for byte with a raw socket.
//! Keeps the client honest about ser2net's actual behaviour without needing
//! the container. Drives the alloc layer: it is the regression for the
//! simple `Vec`-based path.

#![cfg(feature = "alloc")]

use rfc2217::{
    Client, ComPortState, DataBits, Event, FlowControl, InlineBytes, ModemState, NotReady, Parity,
    PartialSettings, PortSettings, Purge, Setting, SettingSet, StopBits,
};

// ser2net's reply to our six option requests: WILL/DO SGA, WILL ECHO + DONT
// ECHO, DO/WILL BINARY, DO COM-PORT (twice), WONT COM-PORT (it never takes
// the client role itself), then two unsolicited subnegotiations: a
// modem-state-mask acknowledgement (111, 0xFF escaped) and NOTIFY-MODEMSTATE 0.
const SER2NET_OPTIONS: &[u8] = &[
    0xff, 0xfb, 0x03, 0xff, 0xfd, 0x03, 0xff, 0xfb, 0x01, 0xff, 0xfe, 0x01, 0xff, 0xfd, 0x00, 0xff,
    0xfb, 0x00, 0xff, 0xfd, 0x2c, 0xff, 0xfd, 0x2c, 0xff, 0xfc, 0x2c, 0xff, 0xfa, 0x2c, 0x6f, 0xff,
    0xff, 0xff, 0xf0, 0xff, 0xfa, 0x2c, 0x6b, 0x00, 0xff, 0xf0,
];
const SER2NET_BAUD: &[u8] = &[0xff, 0xfa, 0x2c, 0x65, 0x00, 0x00, 0x25, 0x80, 0xff, 0xf0];
const SER2NET_DATASIZE: &[u8] = &[0xff, 0xfa, 0x2c, 0x66, 0x08, 0xff, 0xf0];
const SER2NET_PARITY: &[u8] = &[0xff, 0xfa, 0x2c, 0x67, 0x01, 0xff, 0xf0];
const SER2NET_STOPSIZE: &[u8] = &[0xff, 0xfa, 0x2c, 0x68, 0x01, 0xff, 0xf0];
const SER2NET_CONTROL: &[u8] = &[0xff, 0xfa, 0x2c, 0x69, 0x01, 0xff, 0xf0];
const SER2NET_PURGE: &[u8] = &[0xff, 0xfa, 0x2c, 0x70, 0x01, 0xff, 0xf0];
/// ser2net 4.6.4 answers an empty SIGNATURE request with an empty signature.
const SER2NET_SIGNATURE: &[u8] = &[0xff, 0xfa, 0x2c, 0x64, 0xff, 0xf0];
const SER2NET_DATA: &[u8] = b"JULABO HT30-M1 VERSION 4.00\r\n";

const IAC: u8 = 0xff;
const WILL: u8 = 0xfb;
const DO: u8 = 0xfd;
const DONT: u8 = 0xfe;
const SB: u8 = 0xfa;
const SE: u8 = 0xf0;

fn events(c: &mut Client) -> Vec<Event> {
    std::iter::from_fn(|| c.poll_event()).collect()
}

#[test]
fn ser2net_4_6_4_session() {
    let settings = PortSettings {
        baud_rate: 9600,
        data_bits: DataBits::Eight,
        parity: Parity::None,
        stop_bits: StopBits::One,
        flow_control: FlowControl::None,
    };
    let mut c = Client::new(settings);
    // The opening the capture was made with: WILL and DO for BINARY, SGA
    // and COM-PORT.
    assert_eq!(
        c.take_output(),
        [
            [IAC, WILL, 0],
            [IAC, DO, 0],
            [IAC, WILL, 3],
            [IAC, DO, 3],
            [IAC, WILL, 44],
            [IAC, DO, 44]
        ]
        .concat()
    );

    assert!(
        c.receive(SER2NET_OPTIONS).is_empty(),
        "no payload in the negotiation"
    );
    assert_eq!(c.com_port_state(), ComPortState::Active);
    assert_eq!(
        events(&mut c),
        [
            Event::ComPortActive,
            Event::ModemState(ModemState::from_bits(0))
        ]
    );

    // The only negotiation reply we owe is a refusal of ECHO; then the five settings.
    let out = c.take_output();
    let expected: Vec<u8> = [
        vec![IAC, DONT, 1],
        vec![IAC, SB, 44, 1, 0, 0, 0x25, 0x80, IAC, SE],
        vec![IAC, SB, 44, 2, 8, IAC, SE],
        vec![IAC, SB, 44, 3, 1, IAC, SE],
        vec![IAC, SB, 44, 4, 1, IAC, SE],
        vec![IAC, SB, 44, 5, 1, IAC, SE],
    ]
    .concat();
    assert_eq!(out, expected);
    let all: SettingSet = [
        Setting::BaudRate,
        Setting::DataBits,
        Setting::Parity,
        Setting::StopBits,
        Setting::FlowControl,
    ]
    .into_iter()
    .collect();
    assert_eq!(c.readiness(), Err(NotReady::Pending(all)));
    assert_eq!(c.reported_settings(), PartialSettings::default());

    for chunk in [
        SER2NET_BAUD,
        SER2NET_DATASIZE,
        SER2NET_PARITY,
        SER2NET_STOPSIZE,
        SER2NET_CONTROL,
    ] {
        assert!(c.receive(chunk).is_empty());
    }
    assert_eq!(c.readiness(), Ok(()));
    assert_eq!(c.reported_settings().complete(), Some(settings));
    assert_eq!(
        events(&mut c),
        vec![
            Event::BaudRate(9600),
            Event::DataBits(DataBits::Eight),
            Event::Parity(Parity::None),
            Event::StopBits(StopBits::One),
            Event::FlowControl(FlowControl::None)
        ]
    );

    c.purge(Purge::Receive);
    c.request_signature();
    assert_eq!(
        c.take_output(),
        [
            vec![IAC, SB, 44, 12, 1, IAC, SE],
            vec![IAC, SB, 44, 0, IAC, SE]
        ]
        .concat()
    );
    assert!(c.receive(SER2NET_PURGE).is_empty());
    assert!(c.receive(SER2NET_SIGNATURE).is_empty());
    assert_eq!(
        events(&mut c),
        vec![
            Event::Purged(Purge::Receive),
            Event::Signature(InlineBytes::EMPTY)
        ]
    );

    c.write_data(b"version\r");
    assert_eq!(c.take_output(), b"version\r");
    assert_eq!(c.receive(SER2NET_DATA), SER2NET_DATA);
    assert!(
        c.take_output().is_empty(),
        "nothing to say after plain data"
    );
}
