//! Test driver shared by the behaviour tests and the interop replays: byte
//! and settings helpers, and `Wire`, which feeds a half its input and
//! collects its payload, output and events. Tests use only this interface,
//! so they stay the same when the driver's implementation changes.

#![allow(dead_code, unreachable_pub)]

use std::fmt::Debug;
use std::ops::{Deref, DerefMut};

use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, Parity, PortSettings,
    Progress, Server, ServerEvent, Setting, SettingSet, StopBits,
};

/// Bytes from hex digit pairs; whitespace is ignored (`"FF FA 2C"`).
pub fn hex(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert!(digits.len() % 2 == 0, "odd number of hex digits in {s:?}");
    digits
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII");
            u8::from_str_radix(pair, 16).unwrap_or_else(|_| panic!("bad hex {pair:?} in {s:?}"))
        })
        .collect()
}

/// One COM-PORT-OPTION subnegotiation on the wire:
/// `FF FA 2C <code> <payload> FF F0`, with every FF in code and payload doubled.
pub fn sb(code: u8, payload: &[u8]) -> Vec<u8> {
    let mut wire = vec![0xFF, 0xFA, 0x2C];
    for &b in std::iter::once(&code).chain(payload) {
        wire.push(b);
        if b == 0xFF {
            wire.push(0xFF);
        }
    }
    wire.extend([0xFF, 0xF0]);
    wire
}

/// A settings round as the client sends it: `SET-BAUDRATE` with `baud`
/// (4 bytes, big-endian), then `SET-DATASIZE`, `SET-PARITY`, `SET-STOPSIZE`
/// and `SET-CONTROL` with the wire codes `[data, parity, stop, flow]`.
pub fn requests(baud: u32, codes: [u8; 4]) -> Vec<u8> {
    round(0, baud, codes)
}

/// A server's replies to a settings round (codes 101 to 105), carrying
/// `baud` and the wire codes `[data, parity, stop, flow]`.
pub fn replies(baud: u32, codes: [u8; 4]) -> Vec<u8> {
    round(100, baud, codes)
}

fn round(offset: u8, baud: u32, [data, parity, stop, flow]: [u8; 4]) -> Vec<u8> {
    [
        sb(offset + 1, &baud.to_be_bytes()),
        sb(offset + 2, &[data]),
        sb(offset + 3, &[parity]),
        sb(offset + 4, &[stop]),
        sb(offset + 5, &[flow]),
    ]
    .concat()
}

/// `PortSettings` from its five fields.
pub fn port(
    baud_rate: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
) -> PortSettings {
    PortSettings {
        baud_rate,
        data_bits,
        parity,
        stop_bits,
        flow_control,
    }
}

/// These settings, as a set.
pub fn set(settings: &[Setting]) -> SettingSet {
    settings.iter().copied().collect()
}

/// The events a client reports for a bridge's answer to a whole settings
/// round that carries `settings`: one value event per setting, in RFC order.
pub fn reports(settings: PortSettings) -> Vec<Event> {
    vec![
        Event::BaudRate(settings.baud_rate),
        Event::DataBits(settings.data_bits),
        Event::Parity(settings.parity),
        Event::StopBits(settings.stop_bits),
        Event::FlowControl(settings.flow_control),
    ]
}

/// Everything a real ser2net 4.6.4 sent the client in one session, as
/// `tests/ser2net_trace.rs` replays it piece by piece: its answers to the
/// opening (with an unsolicited SET-MODEMSTATE-MASK acknowledgement and
/// NOTIFY-MODEMSTATE 0), the five settings replies for 9600 8N1, a purge
/// reply, an empty signature and serial data.
pub fn ser2net_stream() -> Vec<u8> {
    let mut stream: Vec<u8> = [
        "FF FB 03 FF FD 03 FF FB 01 FF FE 01 FF FD 00 FF FB 00 FF FD 2C FF FD 2C FF FC 2C",
        "FF FA 2C 6F FF FF FF F0 FF FA 2C 6B 00 FF F0",
        "FF FA 2C 65 00 00 25 80 FF F0",
        "FF FA 2C 66 08 FF F0",
        "FF FA 2C 67 01 FF F0",
        "FF FA 2C 68 01 FF F0",
        "FF FA 2C 69 01 FF F0",
        "FF FA 2C 70 01 FF F0",
        "FF FA 2C 64 FF F0",
    ]
    .iter()
    .flat_map(|h| hex(h))
    .collect();
    stream.extend_from_slice(b"JULABO HT30-M1 VERSION 4.00\r\n");
    stream
}

/// What the driver needs from a half (`Client` or `Server`): the core API,
/// under names of its own so that an impl can never call itself.
pub trait Half {
    /// The half's event type.
    type Event: Debug;
    /// One `receive_into` call.
    fn receive_step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress;
    /// The oldest pending output (`output`).
    fn pending_output(&self) -> &[u8];
    /// The first `n` bytes of `pending_output` were written (`consume_output`).
    fn written(&mut self, n: usize);
    /// The next pending event (`poll_event`).
    fn next_event(&mut self) -> Option<Self::Event>;
    /// One `try_write_data` call.
    fn queue_data(&mut self, data: &[u8]) -> Result<usize, CommandError>;
}

impl Half for Client {
    type Event = Event;

    fn receive_step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress {
        self.receive_into(input, payload)
    }

    fn pending_output(&self) -> &[u8] {
        self.output()
    }

    fn written(&mut self, n: usize) {
        self.consume_output(n);
    }

    fn next_event(&mut self) -> Option<Event> {
        self.poll_event()
    }

    fn queue_data(&mut self, data: &[u8]) -> Result<usize, CommandError> {
        self.try_write_data(data)
    }
}

impl Half for Server {
    type Event = ServerEvent;

    fn receive_step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress {
        self.receive_into(input, payload)
    }

    fn pending_output(&self) -> &[u8] {
        self.output()
    }

    fn written(&mut self, n: usize) {
        self.consume_output(n);
    }

    fn next_event(&mut self) -> Option<ServerEvent> {
        self.poll_event()
    }

    fn queue_data(&mut self, data: &[u8]) -> Result<usize, CommandError> {
        self.try_write_data(data)
    }
}

/// A half plus what has been collected from it. Derefs to the half, so its
/// own methods can be called directly (`wire.com_port_state()`).
///
/// It drives the half through the core API only: `feed` calls
/// `receive_into` with a 7-byte payload buffer, polls events after every
/// call, and moves pending output into its own collection whenever a call
/// consumed nothing.
#[derive(Debug)]
pub struct Wire<H: Half> {
    half: H,
    events: Vec<H::Event>,
    output: Vec<u8>,
}

impl<H: Half> Wire<H> {
    /// Wrap a freshly constructed half; its opening (if any) is pending output.
    pub fn new(half: H) -> Self {
        Self {
            half,
            events: Vec::new(),
            output: Vec::new(),
        }
    }

    /// Feed bytes read from the socket; returns the payload they carried.
    /// Events are collected for [`events`](Self::events).
    pub fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        let mut buf = [0; 7];
        let mut rest = input;
        while !rest.is_empty() {
            let p = self.half.receive_step(rest, &mut buf);
            rest = &rest[p.consumed..];
            payload.extend_from_slice(&buf[..p.payload_len]);
            self.collect_events();
            if p.consumed == 0 {
                assert!(
                    self.collect_output() > 0,
                    "receive_into consumed nothing with the output empty"
                );
            }
        }
        payload
    }

    /// All output produced so far and not yet returned.
    pub fn out(&mut self) -> Vec<u8> {
        self.collect_output();
        std::mem::take(&mut self.output)
    }

    /// All events produced so far and not yet returned, in order.
    pub fn events(&mut self) -> Vec<H::Event> {
        self.collect_events();
        std::mem::take(&mut self.events)
    }

    /// Feed socket reads given as hex, one `feed` per read, the way a
    /// capture logged them; returns the payload they carried.
    pub fn feed_reads(&mut self, reads: &[&str]) -> Vec<u8> {
        reads
            .iter()
            .flat_map(|read| self.feed(&hex(read)))
            .collect()
    }

    /// Queue all of `data` with `try_write_data`, writing out the output
    /// before every call (DL4), so a write larger than the buffer's budget
    /// takes several calls. Returns all output not yet returned, as `out`.
    pub fn write_all(&mut self, data: &[u8]) -> Vec<u8> {
        let mut rest = data;
        while !rest.is_empty() {
            self.collect_output();
            let taken = self
                .half
                .queue_data(rest)
                .expect("drained output accepts data (BP6)");
            assert!(taken > 0, "drained output took no data (BP6)");
            rest = &rest[taken..];
        }
        self.out()
    }

    /// One exchange of a captured session: `call`, a core call the capture
    /// made, is accepted and writes exactly `tx` (hex); the reads that
    /// followed it (`rx`, hex, one string per read) carry no payload and
    /// need no answer. Returns the events they produced.
    pub fn exchange(
        &mut self,
        call: impl FnOnce(&mut H) -> Result<(), CommandError>,
        tx: &str,
        rx: &[&str],
    ) -> Vec<H::Event> {
        assert_eq!(call(&mut self.half), Ok(()), "the call that sent {tx}");
        assert_eq!(self.out(), hex(tx));
        assert!(self.feed_reads(rx).is_empty(), "payload in {rx:?}");
        assert!(self.out().is_empty(), "an answer to {rx:?}");
        self.events()
    }

    fn collect_events(&mut self) {
        while let Some(event) = self.half.next_event() {
            self.events.push(event);
        }
    }

    /// Move all pending output to the collection; returns how many bytes.
    fn collect_output(&mut self) -> usize {
        let before = self.output.len();
        loop {
            let pending = self.half.pending_output();
            if pending.is_empty() {
                break;
            }
            let n = pending.len();
            self.output.extend_from_slice(pending);
            self.half.written(n);
        }
        self.output.len() - before
    }
}

impl<H: Half> Deref for Wire<H> {
    type Target = H;

    fn deref(&self) -> &H {
        &self.half
    }
}

impl<H: Half> DerefMut for Wire<H> {
    fn deref_mut(&mut self) -> &mut H {
        &mut self.half
    }
}

/// Exchange output between a client and a server until neither has anything
/// to send; returns the number of rounds that carried bytes. Panics if they
/// are still talking after `max_rounds` rounds. Events stay collected in each
/// `Wire`; payload carried during the pump is discarded (feed data directly
/// to check it).
pub fn pump(client: &mut Wire<Client>, server: &mut Wire<Server>, max_rounds: usize) -> usize {
    let mut rounds = 0;
    loop {
        let to_server = client.out();
        let to_client = server.out();
        if to_server.is_empty() && to_client.is_empty() {
            return rounds;
        }
        assert!(
            rounds < max_rounds,
            "still talking after {max_rounds} rounds: client sends {to_server:02X?}, server sends {to_client:02X?}"
        );
        let _ = server.feed(&to_server);
        let _ = client.feed(&to_client);
        rounds += 1;
    }
}

/// Activate COM-PORT-OPTION on a client (`DO 44`, `WILL 44`), then drain its
/// output (opening and settings round) and events.
pub fn activate(client: &mut Wire<Client>) {
    let _ = client.feed(&hex("FF FD 2C FF FB 2C"));
    assert_eq!(client.com_port_state(), ComPortState::Active);
    let _ = client.out();
    let _ = client.events();
}

/// Activate COM-PORT-OPTION on a server (the client's `WILL 44`, which the
/// server accepts with `DO 44`), then drain its output (the opening, unless
/// already drained, and the `DO 44`) and events.
pub fn activate_server(server: &mut Wire<Server>) {
    let _ = server.feed(&hex("FF FB 2C"));
    assert!(server.com_port_active());
    let _ = server.out();
    let _ = server.events();
}
