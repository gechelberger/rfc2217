//! The documented core loops over loopback TCP: Loop 2's `Link` drives a
//! client and Loop 3's shape drives a bridge on a core server, each on a
//! non-blocking socket that takes at most 3 bytes per send, with 8-byte
//! receive buffers and 2-byte payload buffers. Then the same `Link` over a
//! scripted socket: one call returns only once it is blocked, so a driver
//! woken only by socket readiness (edge-triggered) never strands input or
//! data. Core API only, so it runs in every configuration.

mod common;

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use common::{hex, replies, requests, set};
use rfc2217::{
    Client, ComPortState, DataBits, Event, FlowControl, NotReady, Parity, PortSettings, Server,
    ServerEvent, Setting, StopBits,
};

/// Most bytes one send takes.
const SEND_LIMIT: usize = 3;
/// Receive buffer of each loop.
const RX: usize = 8;
/// Payload buffer of each loop.
const PAYLOAD: usize = 2;

/// Your stack's non-blocking TCP socket (Loop 2).
trait Socket {
    /// Queue up to `bytes.len()` bytes; how many were taken (0 when the send
    /// buffer is full).
    fn send(&mut self, bytes: &[u8]) -> usize;
    /// Copy what has arrived into `buf`; how many bytes (0 when nothing is
    /// there). 0 also stands for the end of the stream and errors: report
    /// those out of band.
    fn recv(&mut self, buf: &mut [u8]) -> usize;
}

/// A non-blocking loopback stream that takes at most [`SEND_LIMIT`] bytes
/// per send.
struct Loopback(TcpStream);

impl Socket for Loopback {
    fn send(&mut self, bytes: &[u8]) -> usize {
        match self.0.write(&bytes[..bytes.len().min(SEND_LIMIT)]) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => 0,
            Err(e) => panic!("send failed: {e}"),
        }
    }

    fn recv(&mut self, buf: &mut [u8]) -> usize {
        match self.0.read(buf) {
            Ok(0) => panic!("the peer closed the connection"),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => 0,
            Err(e) => panic!("recv failed: {e}"),
        }
    }
}

/// One connection: Loop 2's `Link`, with an [`RX`]-byte receive buffer and a
/// [`PAYLOAD`]-byte payload buffer.
struct Link {
    client: Client,
    rx: [u8; RX],
    at: usize,
    end: usize,
}

impl Link {
    fn new(client: Client) -> Self {
        Link {
            client,
            rx: [0; RX],
            at: 0,
            end: 0,
        }
    }

    /// Call whenever the socket may have made progress and whenever you add
    /// to `tx`, the serial data still to send; it is advanced past what was
    /// taken. Returns once it is blocked: all input processed, and `tx`
    /// empty or refused, or the socket full.
    fn poll(
        &mut self,
        sock: &mut impl Socket,
        tx: &mut &[u8],
        uart: &mut impl FnMut(&[u8]),
        on_event: &mut impl FnMut(Event),
    ) {
        let mut payload = [0u8; PAYLOAD];
        loop {
            let mut progress = false;
            // 1. input, in order
            loop {
                if self.at == self.end {
                    self.end = sock.recv(&mut self.rx);
                    self.at = 0;
                    if self.end == 0 {
                        break;
                    }
                }
                let p = self
                    .client
                    .receive_into(&self.rx[self.at..self.end], &mut payload);
                self.at += p.consumed;
                uart(&payload[..p.payload_len]); // payload before the event...
                while let Some(e) = self.client.poll_event() {
                    on_event(e); // ...then the event
                }
                if p.consumed == 0 {
                    break; // the output must be written first: step 3
                }
                progress = true;
            }
            // 2. serial data; refused while COM-PORT-OPTION is negotiating or
            //    the output is full, and what was not taken stays in `tx`
            if let Ok(n) = self.client.try_write_data(tx) {
                *tx = &tx[n..];
                progress |= n > 0;
            }
            // 3. output; partial sends are fine
            while self.client.has_output() {
                let n = sock.send(self.client.output());
                if n == 0 {
                    return; // the socket is full: poll again once it takes more
                }
                self.client.consume_output(n);
                progress = true;
            }
            if !progress {
                return;
            }
        }
    }
}

/// A bridge on the core server (Loop 3) whose UART loops back: every payload
/// byte the client sends is written back to it.
struct Bridge {
    server: Server,
    rx: [u8; RX],
    at: usize,
    end: usize,
    /// What the UART received, and so has to send back.
    uart: Vec<u8>,
    /// How much of `uart` went back to the client.
    sent: usize,
    events: Vec<ServerEvent>,
}

impl Bridge {
    fn new(server: Server) -> Self {
        Bridge {
            server,
            rx: [0; RX],
            at: 0,
            end: 0,
            uart: Vec::new(),
            sent: 0,
            events: Vec::new(),
        }
    }

    fn poll(&mut self, sock: &mut impl Socket) {
        let mut to_uart = [0u8; PAYLOAD];
        loop {
            if self.at == self.end {
                self.end = sock.recv(&mut self.rx);
                self.at = 0;
                if self.end == 0 {
                    break;
                }
            }
            let p = self
                .server
                .receive_into(&self.rx[self.at..self.end], &mut to_uart);
            self.at += p.consumed;
            self.uart.extend_from_slice(&to_uart[..p.payload_len]); // bytes before the event...
            while let Some(event) = self.server.poll_event() {
                // ...then the event, at its position (a real bridge applies
                // SettingsChanged, Purge and Break to its UART here).
                self.events.push(event);
            }
            if p.consumed == 0 {
                break;
            }
        }
        // UART → client
        if let Ok(n) = self.server.try_write_data(&self.uart[self.sent..]) {
            self.sent += n;
        }
        while self.server.has_output() {
            let n = sock.send(self.server.output());
            if n == 0 {
                break;
            }
            self.server.consume_output(n);
        }
    }
}

#[test]
fn core_loops_over_loopback_tcp_with_three_byte_writes() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let client_end = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
    let (server_end, _) = listener.accept().expect("accept");
    for end in [&client_end, &server_end] {
        end.set_nonblocking(true).expect("non-blocking");
        end.set_nodelay(true).expect("no delay");
    }
    let (mut client_sock, mut server_sock) = (Loopback(client_end), Loopback(server_end));

    let wanted = PortSettings {
        baud_rate: 115_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::RtsCts,
    };
    let mut link = Link::new(Client::new(wanted));
    let mut bridge = Bridge::new(Server::new(PortSettings::default()));
    let data: Vec<u8> = (0..=255).collect();
    let mut tx = &data[..];
    let (mut received, mut events) = (Vec::new(), Vec::new());

    let deadline = Instant::now() + Duration::from_secs(20);
    while received.len() < data.len() {
        assert!(
            Instant::now() < deadline,
            "no echo after 20 s: {:?}, {} of {} bytes back",
            link.client.readiness(),
            received.len(),
            data.len()
        );
        link.poll(
            &mut client_sock,
            &mut tx,
            &mut |bytes: &[u8]| received.extend_from_slice(bytes),
            &mut |event| events.push(event),
        );
        bridge.poll(&mut server_sock);
        std::thread::yield_now();
    }

    assert_eq!(received, data, "every byte value, 0xFF included, echoed");
    assert!(tx.is_empty());
    assert_eq!(link.client.readiness(), Ok(()));
    assert_eq!(link.client.reported_settings().complete(), Some(wanted));
    assert_eq!(events.first(), Some(&Event::ComPortActive));
    assert_eq!(bridge.server.settings(), wanted);
    assert_eq!(bridge.uart, data);
    assert_eq!(bridge.events.first(), Some(&ServerEvent::ComPortActive));
    assert!(
        bridge
            .events
            .contains(&ServerEvent::SettingsChanged(wanted))
    );
}

/// A socket whose peer has already sent everything in `incoming`, and that
/// takes at most `room` more bytes: what an edge-triggered driver sees after
/// one readiness notification.
struct Scripted {
    incoming: VecDeque<u8>,
    sent: Vec<u8>,
    room: usize,
}

impl Scripted {
    fn new(incoming: &[u8]) -> Self {
        Scripted {
            incoming: incoming.iter().copied().collect(),
            sent: Vec::new(),
            room: usize::MAX,
        }
    }

    /// The peer sends `bytes`.
    fn deliver(&mut self, bytes: &[u8]) {
        self.incoming.extend(bytes);
    }

    /// Everything sent since the last call.
    fn take_sent(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.sent)
    }
}

impl Socket for Scripted {
    fn send(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(self.room);
        self.room -= n;
        self.sent.extend_from_slice(&bytes[..n]);
        n
    }

    fn recv(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.incoming.len());
        for (slot, byte) in buf.iter_mut().zip(self.incoming.drain(..n)) {
            *slot = byte;
        }
        n
    }
}

/// The client's opening (N4).
fn client_opening() -> Vec<u8> {
    hex("FF FB 00 FF FD 00 FF FB 03 FF FD 03 FF FB 2C FF FD 2C")
}

#[test]
fn link_poll_returns_only_when_blocked() {
    let mut link = Link::new(Client::new(PortSettings::default()));
    let (mut received, mut events) = (Vec::new(), Vec::new());
    let mut uart = |bytes: &[u8]| received.extend_from_slice(bytes);
    let mut on_event = |event: Event| events.push(event);

    // Data passed while COM-PORT-OPTION is negotiating waits in `tx`.
    let mut sock = Scripted::new(&[]);
    let early = b"early".to_vec();
    let mut tx = &early[..];
    link.poll(&mut sock, &mut tx, &mut uart, &mut on_event);
    assert_eq!(sock.take_sent(), client_opening());
    assert_eq!(tx, b"early");

    // The bridge accepts the option, then sends 80 options to refuse, which
    // fill the output halfway through the burst, and then data. One call
    // processes all of it, then sends the data waiting in `tx`.
    let burst = [
        hex("FF FD 2C"),
        hex("FF FD 18").repeat(80),
        b"HELLO".to_vec(),
    ]
    .concat();
    sock.deliver(&burst);
    link.poll(&mut sock, &mut tx, &mut uart, &mut on_event);
    assert!(sock.incoming.is_empty(), "every byte read");
    assert_eq!(link.at, link.end, "every byte processed");
    assert!(tx.is_empty());
    assert!(!link.client.has_output());
    let round = requests(9600, [8, 1, 1, 1]);
    assert_eq!(
        sock.take_sent(),
        [round, hex("FF FC 18").repeat(80), b"early".to_vec()].concat()
    );

    // More data than one write budget (211 bytes) goes out in one call.
    let data: Vec<u8> = (0..5000).map(|i| b'a' + (i % 26) as u8).collect();
    let mut tx = &data[..];
    link.poll(&mut sock, &mut tx, &mut uart, &mut on_event);
    assert!(tx.is_empty());
    assert_eq!(sock.take_sent(), data);

    // A full socket stops the call; the next one goes on where it stopped.
    sock.room = 100;
    let mut tx = &data[..];
    link.poll(&mut sock, &mut tx, &mut uart, &mut on_event);
    assert_eq!(sock.sent.len(), 100);
    assert!(link.client.has_output());
    assert!(!tx.is_empty());
    sock.room = usize::MAX;
    link.poll(&mut sock, &mut tx, &mut uart, &mut on_event);
    assert!(tx.is_empty());
    assert!(!link.client.has_output());
    assert_eq!(sock.take_sent(), data);

    assert_eq!(received, b"HELLO");
    assert_eq!(events, [Event::ComPortActive]);
}

#[test]
fn link_sends_data_after_a_mismatch_is_accepted() {
    // The bridge answers 115200 with 9600: the caller accepts that and
    // sends data, which the Link does not hold back for readiness.
    let wanted = PortSettings {
        baud_rate: 115_200,
        ..PortSettings::default()
    };
    let mut link = Link::new(Client::new(wanted));
    let mut sock = Scripted::new(&[]);
    let mut none: &[u8] = &[];
    let (mut uart, mut on_event) = (|_: &[u8]| {}, |_: Event| {});
    link.poll(&mut sock, &mut none, &mut uart, &mut on_event);
    sock.deliver(&[hex("FF FD 2C"), replies(9600, [8, 1, 1, 1])].concat());
    link.poll(&mut sock, &mut none, &mut uart, &mut on_event);
    assert_eq!(link.client.com_port_state(), ComPortState::Active);
    assert_eq!(
        link.client.readiness(),
        Err(NotReady::Mismatched(set(&[Setting::BaudRate])))
    );
    let _ = sock.take_sent();

    let mut tx = &b"data"[..];
    link.poll(&mut sock, &mut tx, &mut uart, &mut on_event);
    assert!(tx.is_empty());
    assert_eq!(sock.take_sent(), b"data");
}
