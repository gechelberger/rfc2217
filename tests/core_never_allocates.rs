//! The core API never touches the heap, also with the `alloc` feature on
//! (O7): a counting global allocator watches a whole client/server session
//! driven through the core only.

#![cfg(feature = "alloc")]
// A global allocator is an `unsafe impl`; it only forwards to `System`.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use rfc2217::{
    Client, ComPortState, DataBits, Event, FlowControl, InlineBytes, LineState, ModemState, Parity,
    PortSettings, Purge, Server, ServerEvent, StopBits,
};

/// Allocations made by a thread while its `COUNTING` flag was set.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set by the test around the session; other threads are not counted.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn count() {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `System`, counting every allocation and reallocation.
struct CountingAllocator;

// SAFETY: every method forwards to `System` with the caller's arguments, so
// `System`'s guarantees carry over.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Payload received by one half, in a fixed array.
struct Sink {
    bytes: [u8; 256],
    len: usize,
}

impl Sink {
    const fn new() -> Self {
        Self {
            bytes: [0; 256],
            len: 0,
        }
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    /// Return what was received and forget it.
    fn take(&mut self) -> &[u8] {
        let len = core::mem::take(&mut self.len);
        &self.bytes[..len]
    }
}

/// Send everything `$from` has queued to `$to` through a 5-byte "socket"
/// and a 3-byte payload buffer, polling `$to`'s events into `$on_event`.
macro_rules! transfer {
    ($from:expr, $to:expr, $sink:expr, $on_event:expr) => {{
        while $from.has_output() {
            let mut segment = [0u8; 5];
            let n = $from.output().len().min(segment.len());
            segment[..n].copy_from_slice(&$from.output()[..n]);
            $from.consume_output(n);
            let mut rest = &segment[..n];
            while !rest.is_empty() {
                let mut buf = [0u8; 3];
                let p = $to.receive_into(rest, &mut buf);
                assert!(p.consumed > 0, "the receiver's output is full");
                rest = &rest[p.consumed..];
                $sink.extend(&buf[..p.payload_len]);
                while let Some(event) = $to.poll_event() {
                    $on_event(event);
                }
            }
        }
    }};
}

/// Events seen so far, by kind (no heap).
#[derive(Default)]
struct Seen {
    client: [usize; 8],
    server: [usize; 8],
}

impl Seen {
    fn client(&mut self, event: Event) {
        let kind = match event {
            Event::ComPortActive => 0,
            Event::BaudRate(_)
            | Event::DataBits(_)
            | Event::Parity(_)
            | Event::StopBits(_)
            | Event::FlowControl(_) => 1,
            Event::Dtr(_) | Event::Rts(_) | Event::Break(_) => 2,
            Event::Purged(Purge::Both) => 3,
            Event::Signature(signature) => {
                let expected = InlineBytes::from(concat!("rfc2217 ", env!("CARGO_PKG_VERSION")));
                assert_eq!(signature, expected);
                4
            }
            Event::ModemState(state) if state.bits() == 0x30 => 5,
            Event::LineState(state) if state.bits() == 0x60 => 5,
            Event::FlowSuspended | Event::FlowResumed => 6,
            _ => 7,
        };
        self.client[kind] += 1;
    }

    fn server(&mut self, event: ServerEvent) {
        let kind = match event {
            ServerEvent::ComPortActive => 0,
            ServerEvent::SettingsChanged(_) => 1,
            ServerEvent::Dtr(_) | ServerEvent::Rts(_) | ServerEvent::Break(_) => 2,
            ServerEvent::Purge(Purge::Both) => 3,
            _ => 7,
        };
        self.server[kind] += 1;
    }
}

#[test]
fn core_only_use_never_allocates_with_alloc_enabled() {
    let wanted = PortSettings {
        baud_rate: 115_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::RtsCts,
    };
    let mut client = Client::new(wanted);
    let mut server = Server::new(PortSettings::default());
    let (mut to_client, mut to_server) = (Sink::new(), Sink::new());
    let mut seen = Seen::default();

    COUNTING.with(|c| c.set(true));

    // Activation and the settings round.
    while client.has_output() || server.has_output() {
        transfer!(client, server, to_server, |e| seen.server(e));
        transfer!(server, client, to_client, |e| seen.client(e));
    }
    assert_eq!(client.com_port_state(), ComPortState::Active);
    assert!(client.is_ready());
    assert_eq!(server.settings(), wanted);

    // Every command, and the signature.
    assert_eq!(client.try_set_dtr(false), Ok(()));
    assert_eq!(client.try_set_rts(true), Ok(()));
    assert_eq!(client.try_set_break(true), Ok(()));
    assert_eq!(client.try_set_break(false), Ok(()));
    assert_eq!(client.try_purge(Purge::Both), Ok(()));
    assert_eq!(client.try_request_signature(), Ok(()));
    let other = PortSettings {
        baud_rate: 9600,
        ..wanted
    };
    assert_eq!(client.try_set_port_settings(other), Ok(()));

    // Data with 0xFF both ways.
    assert_eq!(client.try_write_data(&[0x41, 0xFF, 0x42]), Ok(3));
    assert_eq!(server.try_write_data(&[0xFF, 0x00, 0xFF]), Ok(3));

    // Notifications. The line-state mask a client would set (the client has
    // no call for it in 0.1): until then the server sends no line state.
    let mask = [0xFF, 0xFA, 0x2C, 10, 0xFF, 0xFF, 0xFF, 0xF0];
    let p = server.receive_into(&mask, &mut [0; 4]);
    assert_eq!(p.consumed, mask.len());
    let modem = ModemState::DSR | ModemState::CTS;
    assert_eq!(server.try_set_modem_state(modem), Ok(()));
    let line = LineState::SHIFT_REGISTER_EMPTY | LineState::HOLDING_REGISTER_EMPTY;
    assert_eq!(server.try_notify_line_state(line), Ok(()));
    assert_eq!(server.try_suspend_flow(), Ok(()));
    assert_eq!(server.try_resume_flow(), Ok(()));

    while client.has_output() || server.has_output() {
        transfer!(client, server, to_server, |e| seen.server(e));
        transfer!(server, client, to_client, |e| seen.client(e));
    }

    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0, "the core allocated");

    assert_eq!(to_server.take(), [0x41, 0xFF, 0x42]);
    assert_eq!(to_client.take(), [0xFF, 0x00, 0xFF]);
    assert!(client.is_ready());
    assert_eq!(server.settings(), other);
    assert!(!client.flow_suspended());
    // ComPortActive, 10 settings replies, 4 line replies, the purge, the
    // signature, 2 notifications and suspend/resume.
    assert_eq!(seen.client, [1, 10, 4, 1, 1, 2, 2, 0]);
    // ComPortActive, 5 + 1 settings changes, 4 line events, the purge.
    assert_eq!(seen.server, [1, 6, 4, 1, 0, 0, 0, 0]);
}
