//! The alloc layer: `Vec`-based methods that never push back, the held queue
//! of calls made while negotiating, and mixing it with the core on one
//! instance.

#![cfg(feature = "alloc")]

mod common;

use common::{hex, sb};
use rfc2217::{
    Client, ComPortState, Event, InlineBytes, LineState, ModemState, PortSettings, Purge, Server,
    ServerEvent,
};

/// The client's opening.
fn opening() -> Vec<u8> {
    hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C")
}

/// The settings round for `PortSettings::default()` (9600 8N1, no flow
/// control).
fn round() -> Vec<u8> {
    [
        sb(1, &[0x00, 0x00, 0x25, 0x80]),
        sb(2, &[8]),
        sb(3, &[1]),
        sb(4, &[1]),
        sb(5, &[1]),
    ]
    .concat()
}

fn events(c: &mut Client) -> Vec<Event> {
    std::iter::from_fn(|| c.poll_event()).collect()
}

/// Everything pending, read through the core's `output`/`consume_output`
/// in steps of at most `step` bytes.
fn read_core(c: &mut Client, step: usize) -> Vec<u8> {
    let mut wire = Vec::new();
    while c.has_output() {
        let n = c.output().len().min(step);
        assert!(n > 0, "has_output() but output() is empty");
        wire.extend_from_slice(&c.output()[..n]);
        c.consume_output(n);
    }
    assert!(c.output().is_empty());
    wire
}

/// A new client whose opening was taken.
fn negotiating() -> Client {
    let mut c = Client::new(PortSettings::default());
    assert_eq!(c.take_output(), opening());
    c
}

/// An active client whose opening, round and event were taken.
fn active() -> Client {
    let mut c = negotiating();
    assert!(c.receive(&hex("FF FD 2C")).is_empty());
    assert_eq!(c.take_output(), round());
    assert_eq!(events(&mut c), vec![Event::ComPortActive]);
    c
}

/// A client refused by the peer (`DONT 44`, `WONT 44`), output and events
/// taken.
fn refused() -> Client {
    let mut c = negotiating();
    assert!(c.receive(&hex("FF FE 2C FF FC 2C")).is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Refused);
    assert_eq!(events(&mut c), vec![Event::ComPortRefused]);
    assert!(c.take_output().is_empty());
    c
}

#[test]
fn alloc_layer_never_pushes_back() {
    let mut c = active();
    for i in 0..1000 {
        c.set_dtr(i % 2 == 0);
    }
    let expected: Vec<u8> = (0..1000)
        .flat_map(|i| sb(5, &[if i % 2 == 0 { 8 } else { 9 }]))
        .collect();
    assert_eq!(c.take_output(), expected);

    // One receive producing 3000 bytes of replies and 1000 events.
    let input: Vec<u8> = (0..1000)
        .flat_map(|i| [hex("FF FD 18"), sb(106, &[i as u8])].concat())
        .collect();
    assert!(c.receive(&input).is_empty());
    assert_eq!(c.take_output(), hex("FF FC 18").repeat(1000));
    let expected: Vec<Event> = (0..1000)
        .map(|i| Event::LineState(LineState::from_bits(i as u8)))
        .collect();
    assert_eq!(events(&mut c), expected);

    // The server's notifications spill the same way: 1000 of each kind in a
    // row, so that every kind fills the fixed buffer, without take_output.
    let mut s = Server::new(PortSettings::default());
    assert!(
        s.receive(&[hex("FF FB 2C"), sb(10, &[0xFF])].concat())
            .is_empty()
    );
    let _ = s.take_output();
    let mut expected = Vec::new();
    for i in 0..1000 {
        // The first state as it is, then CTS changing each time.
        let (state, sent) = match i {
            0 => (ModemState::CTS, 0x10),
            _ if i % 2 == 0 => (ModemState::CTS, 0x11),
            _ => (ModemState::default(), 0x01),
        };
        s.set_modem_state(state);
        expected.extend(sb(107, &[sent]));
    }
    for _ in 0..1000 {
        s.notify_line_state(LineState::DATA_READY);
        expected.extend(sb(106, &[1]));
    }
    for _ in 0..1000 {
        s.suspend_flow();
        expected.extend(sb(108, &[]));
    }
    for _ in 0..1000 {
        s.resume_flow();
        expected.extend(sb(109, &[]));
    }
    assert_eq!(s.take_output(), expected);
}

#[test]
fn commands_and_data_wait_for_activation_in_call_order() {
    let mut c = Client::new(PortSettings::default());
    c.set_dtr(false);
    c.write_data(b"X");
    c.set_dtr(true);
    assert_eq!(c.take_output(), opening(), "only the opening");
    assert!(!c.has_output());

    assert!(c.receive(&hex("FF FD 2C")).is_empty());
    assert_eq!(
        c.take_output(),
        [round(), sb(5, &[9]), b"X".to_vec(), sb(5, &[8])].concat()
    );
    assert_eq!(events(&mut c), vec![Event::ComPortActive]);
}

#[test]
fn held_items_follow_the_activating_byte_exactly() {
    // The activating DO 44, then more input that causes output of its own.
    let input = [
        hex("FF FD 2C"),
        hex("FF FD 18"),
        b"ab".to_vec(),
        sb(101, &[0x00, 0x00, 0x25, 0x80]),
        hex("FF FB 01"),
    ]
    .concat();
    let expected = [
        round(),
        sb(5, &[9]),
        b"X".to_vec(),
        sb(12, &[3]),
        hex("FF FC 18"),
        hex("FF FE 01"),
    ]
    .concat();
    for split in 0..=input.len() {
        let mut c = negotiating();
        c.set_dtr(false);
        c.write_data(b"X");
        c.purge(Purge::Both);
        let mut payload = c.receive(&input[..split]);
        let mut output = c.take_output();
        payload.extend(c.receive(&input[split..]));
        output.extend(c.take_output());
        assert_eq!(output, expected, "split at {split}");
        assert_eq!(payload, b"ab");
        assert_eq!(
            events(&mut c),
            vec![Event::ComPortActive, Event::BaudRate(9600)]
        );
    }
}

#[test]
fn held_items_are_released_by_core_receive_into() {
    let mut c = negotiating();
    c.set_dtr(false);
    c.write_data(b"X");
    let p = c.receive_into(&hex("FF FD 2C"), &mut [0; 8]);
    assert_eq!((p.consumed, p.payload_len), (3, 0));
    assert_eq!(c.poll_event(), Some(Event::ComPortActive));
    assert_eq!(
        read_core(&mut c, 5),
        [round(), sb(5, &[9]), b"X".to_vec()].concat()
    );
}

#[test]
fn held_data_is_released_on_refusal_and_commands_dropped() {
    let mut c = negotiating();
    c.write_data(b"A");
    c.set_dtr(false);
    c.request_signature();
    c.write_data(b"B\xFF");
    c.set_break(true);
    assert!(!c.has_output());

    assert!(c.receive(&hex("FF FE 2C FF FC 2C")).is_empty());
    assert_eq!(c.com_port_state(), ComPortState::Refused);
    assert_eq!(events(&mut c), vec![Event::ComPortRefused]);
    assert_eq!(c.take_output(), hex("41 42 FF FF"));

    // The same through the core's receive_into. The DONT refuses the option,
    // so the call stops right after it; the WONT then changes nothing.
    let mut c = negotiating();
    c.set_rts(true);
    c.write_data(b"C");
    let input = hex("FF FE 2C FF FC 2C");
    let p = c.receive_into(&input, &mut [0; 8]);
    assert_eq!(p.consumed, 3);
    assert_eq!(c.poll_event(), Some(Event::ComPortRefused));
    assert_eq!(read_core(&mut c, 1), b"C");
    let p = c.receive_into(&input[3..], &mut [0; 8]);
    assert_eq!(p.consumed, 3);
    assert_eq!(c.poll_event(), None);
    assert!(!c.has_output());

    // Nothing is held while Active, so a withdrawal releases nothing: calls
    // made while Active were written at once.
    let mut c = active();
    c.write_data(b"D");
    c.set_dtr(false);
    assert!(c.receive(&hex("FF FE 2C")).is_empty());
    assert_eq!(events(&mut c), vec![Event::ComPortRefused]);
    assert_eq!(
        c.take_output(),
        [b"D".to_vec(), sb(5, &[9]), hex("FF FC 2C")].concat()
    );
}

#[test]
fn commands_after_refusal_are_dropped_but_data_flows() {
    let mut c = refused();
    c.set_dtr(true);
    c.set_rts(false);
    c.set_break(true);
    c.purge(Purge::Both);
    c.request_signature();
    let other = PortSettings {
        baud_rate: 300,
        ..PortSettings::default()
    };
    c.set_port_settings(other);
    assert!(!c.has_output(), "no subnegotiation for a refused option");
    assert_eq!(c.requested_settings(), other, "the request is replaced");

    c.write_data(b"A\xFF");
    assert_eq!(c.take_output(), hex("41 FF FF"));
}

#[test]
fn signature_request_held_until_active() {
    let mut c = negotiating();
    c.request_signature();
    assert!(!c.has_output(), "held while negotiating");
    // A held request is not outstanding yet: an empty SIGNATURE now is the
    // server asking for ours, not an answer.
    assert!(c.receive(&sb(100, &[])).is_empty());
    assert!(events(&mut c).is_empty());
    assert!(c.take_output().is_empty(), "not answered");

    // Written after the round at activation; from then on it is outstanding,
    // so an empty answer is reported (ser2net 4.6.4 answers that way).
    assert!(c.receive(&hex("FF FD 2C")).is_empty());
    assert_eq!(c.take_output(), [round(), sb(0, &[])].concat());
    assert_eq!(events(&mut c), vec![Event::ComPortActive]);
    assert!(c.receive(&sb(100, &[])).is_empty());
    assert_eq!(events(&mut c), vec![Event::Signature(InlineBytes::EMPTY)]);

    // Answered once: the next empty SIGNATURE is a request again.
    assert!(c.receive(&sb(100, &[])).is_empty());
    assert!(events(&mut c).is_empty());

    // While Active the request is written at once and counted the same way.
    c.request_signature();
    assert_eq!(c.take_output(), sb(0, &[]));
    assert!(c.receive(&sb(100, b"ser2net")).is_empty());
    assert_eq!(
        events(&mut c),
        vec![Event::Signature(InlineBytes::new(b"ser2net"))]
    );

    // A request held when the peer refuses is dropped, never counted.
    let mut c = negotiating();
    c.request_signature();
    assert!(c.receive(&hex("FF FE 2C")).is_empty());
    assert_eq!(events(&mut c), vec![Event::ComPortRefused]);
    assert!(c.take_output().is_empty());
    assert!(c.receive(&sb(100, &[])).is_empty());
    assert!(events(&mut c).is_empty());
}

#[test]
fn mixing_layers_keeps_call_order() {
    for step in [1, 3, 7] {
        let mut c = active();
        // More than the fixed buffer holds: part of it spills.
        c.write_data(&[0x41; 600]);
        assert_eq!(c.try_set_dtr(false), Ok(()));
        c.write_data(b"B");
        assert_eq!(c.try_set_rts(true), Ok(()));
        let expected = [vec![0x41; 600], sb(5, &[9]), b"B".to_vec(), sb(5, &[0x0B])].concat();
        assert_eq!(read_core(&mut c, step), expected, "{step}-byte steps");
        assert!(!c.has_output());
    }

    // Buffered data first, then a core command.
    let mut c = active();
    c.write_data(b"A");
    assert_eq!(c.try_set_dtr(false), Ok(()));
    assert_eq!(read_core(&mut c, 64), [b"A".to_vec(), sb(5, &[9])].concat());

    // take_output sees what the core queued, after the spill.
    let mut c = active();
    c.write_data(&[0x41; 300]);
    assert_eq!(c.try_write_data(b"Z"), Ok(1));
    assert_eq!(c.take_output(), [vec![0x41; 300], b"Z".to_vec()].concat());
    assert!(!c.has_output());
}

#[test]
fn poll_event_sees_alloc_and_core_events_in_order() {
    let mut c = active();
    let mut payload = [0; 4];

    // Three events queued by the alloc layer, then one in the core's slot.
    let _ = c.receive(&[sb(107, &[1]), sb(107, &[2]), sb(107, &[3])].concat());
    let p = c.receive_into(&sb(107, &[4]), &mut payload);
    assert_eq!(p.consumed, 7);
    // The slot is full: the core takes no more input.
    assert_eq!(c.receive_into(&sb(107, &[5]), &mut payload).consumed, 0);
    // An alloc receive queues the slot's event before its own.
    let _ = c.receive(&sb(107, &[5]));
    let mut queued = Vec::new();
    assert!(c.receive_until_event(&sb(107, &[6]), &mut queued) > 0);
    let p = c.receive_into(&sb(107, &[7]), &mut payload);
    assert_eq!(p.consumed, 7);
    assert_eq!(
        events(&mut c),
        (1..=7)
            .map(|bits| Event::ModemState(ModemState::from_bits(bits)))
            .collect::<Vec<_>>()
    );

    // The server's poll_event reads the same way.
    let mut s = Server::new(PortSettings::default());
    let _ = s.receive(&hex("FF FB 2C"));
    let p = s.receive_into(&sb(5, &[8]), &mut payload);
    assert_eq!(p.consumed, 7);
    let _ = s.receive(&sb(5, &[11]));
    let polled: Vec<_> = std::iter::from_fn(|| s.poll_event()).collect();
    assert_eq!(
        polled,
        vec![
            ServerEvent::ComPortActive,
            ServerEvent::Dtr(true),
            ServerEvent::Rts(true)
        ]
    );
}

#[test]
fn alloc_server_notifications_are_dropped_when_inactive() {
    let mut s = Server::new(PortSettings::default());
    assert_eq!(s.take_output().len(), 12, "the opening");
    // Not active: the modem state is stored, the rest is dropped.
    s.set_modem_state(ModemState::CTS);
    s.notify_line_state(LineState::BREAK_DETECT);
    s.suspend_flow();
    s.resume_flow();
    assert!(!s.has_output());

    // Activation (DO 44) reports the stored modem state, and nothing else.
    assert!(s.receive(&hex("FF FB 2C")).is_empty());
    assert_eq!(
        s.take_output(),
        [hex("FF FD 2C"), sb(107, &[0x10])].concat()
    );
    assert_eq!(s.poll_event(), Some(ServerEvent::ComPortActive));

    // While active everything is sent, line states through the mask.
    assert!(s.receive(&sb(10, &[0xFF])).is_empty());
    s.notify_line_state(LineState::BREAK_DETECT);
    s.suspend_flow();
    s.resume_flow();
    assert_eq!(
        s.take_output(),
        [
            sb(110, &[0xFF]),
            sb(106, &[0x10]),
            sb(108, &[]),
            sb(109, &[])
        ]
        .concat()
    );

    // Withdrawn: dropped again, and the modem state stored for the next
    // activation.
    assert!(s.receive(&hex("FF FC 2C")).is_empty());
    s.set_modem_state(ModemState::DSR);
    s.notify_line_state(LineState::BREAK_DETECT);
    s.suspend_flow();
    s.resume_flow();
    assert_eq!(s.take_output(), hex("FF FE 2C"));
    assert!(s.receive(&hex("FF FB 2C")).is_empty());
    assert_eq!(
        s.take_output(),
        [hex("FF FD 2C"), sb(107, &[0x20])].concat()
    );
    assert_eq!(s.poll_event(), Some(ServerEvent::ComPortActive));
    assert_eq!(s.poll_event(), None);
}

#[test]
fn receive_until_event_marks_event_positions() {
    let mut s = Server::new(PortSettings::default());
    assert!(s.receive(&hex("FF FB 2C")).is_empty());
    assert_eq!(s.poll_event(), Some(ServerEvent::ComPortActive));
    let _ = s.take_output();

    let input = [
        b"AT".to_vec(),
        sb(1, &hex("00 00 4B 00")),
        sb(12, &[2]),
        b"\r".to_vec(),
    ]
    .concat();
    let mut payload = Vec::new();
    let mut steps = Vec::new();
    let mut events = Vec::new();
    let mut at = 0;
    while at < input.len() {
        let n = s.receive_until_event(&input[at..], &mut payload);
        at += n;
        steps.push((n, payload.len()));
        events.extend(std::iter::from_fn(|| s.poll_event()).map(|e| (payload.len(), e)));
    }
    // Each call returns right after the byte that produced an event: "AT"
    // precedes the settings change, "\r" follows the purge.
    assert_eq!(steps, [(2 + 10, 2), (7, 2), (1, 3)]);
    let changed = PortSettings {
        baud_rate: 19_200,
        ..PortSettings::default()
    };
    assert_eq!(
        events,
        [
            (2, ServerEvent::SettingsChanged(changed)),
            (2, ServerEvent::Purge(Purge::Transmit)),
        ]
    );
    assert_eq!(payload, b"AT\r");
    assert_eq!(
        s.take_output(),
        [sb(101, &hex("00 00 4B 00")), sb(112, &[2])].concat()
    );
}

#[test]
fn receive_until_event_on_empty_input_returns_zero() {
    let mut c = Client::new(PortSettings::default());
    let mut payload = vec![1, 2];
    assert_eq!(c.receive_until_event(&[], &mut payload), 0);
    assert_eq!(payload, [1, 2]);
    assert_eq!(c.poll_event(), None);

    let mut s = Server::new(PortSettings::default());
    assert_eq!(s.receive_until_event(&[], &mut payload), 0);
    assert_eq!(payload, [1, 2]);
    assert_eq!(s.poll_event(), None);
}
