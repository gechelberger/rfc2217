//! RFC 2217 (Telnet COM Port Control): serial ports over TCP, without doing
//! any I/O.
//!
//! Serial-to-Ethernet bridges (ser2net, sredird, Moxa NPort, Lantronix,
//! Digi, ...) usually expose a serial port in two ways: a raw TCP socket that
//! carries only the bytes, and an
//! [RFC 2217](https://www.rfc-editor.org/rfc/rfc2217) Telnet service that also
//! lets the client set the baud rate, data bits, parity, stop bits and flow
//! control of the far port, drive its DTR, RTS and BREAK, and hear about its
//! modem and line state. This crate implements that service as two pure
//! state machines, one instance per TCP connection:
//!
//! * [`Client`]: the program that connects to a bridge. It negotiates the
//!   Telnet options, configures the remote port, tracks what the bridge
//!   confirmed ([`Client::readiness`]), escapes and unescapes the serial data,
//!   and reports what the bridge says as [`Event`]s.
//! * [`Server`]: the bridge side, for simulators, tests, and bridges whose
//!   port accepts every value RFC 2217 can encode. It applies and acknowledges
//!   every request, and reports it as a [`ServerEvent`].
//!
//! Neither opens a socket, spawns a thread or reads a clock: you pass in the
//! bytes you read and write out the bytes they hand back, so the same code
//! runs over blocking `std::net`, an async runtime, an embedded network stack
//! or an in-memory test. The crate is `no_std` and has no dependencies by
//! default.
//!
//! # Two layers
//!
//! The **core** exists in every configuration and never allocates. Each
//! instance has fixed memory (a 256-byte output buffer and one event slot,
//! about half a KiB in all), and when that is full it pushes back instead of
//! losing anything: [`receive_into`](Client::receive_into) stops before a byte
//! it cannot complete, and the `try_*` calls return a [`CommandError`] and
//! change nothing.
//!
//! The **alloc layer** (feature `alloc`, on by default) adds `Vec`-based
//! methods to the same types. They never push back: what does not fit the
//! fixed buffers moves to the heap until you drain it. While the client is
//! still negotiating, its data and commands wait in an ordered queue and are
//! sent right after the settings round.
//!
//! | Core (every configuration) | Alloc layer (feature `alloc`) |
//! |---|---|
//! | [`receive_into`](Client::receive_into) | `receive`, `receive_until_event` |
//! | [`output`](Client::output) and [`consume_output`](Client::consume_output) | `take_output` |
//! | [`has_output`](Client::has_output), [`poll_event`](Client::poll_event) | the same methods |
//! | [`try_write_data`](Client::try_write_data) | `write_data` |
//! | [`Client::try_set_port_settings`] | `set_port_settings` |
//! | [`Client::try_set_dtr`], [`try_set_rts`](Client::try_set_rts), [`try_set_break`](Client::try_set_break) | `set_dtr`, `set_rts`, `set_break` |
//! | [`Client::try_purge`], [`Client::try_request_signature`] | `purge`, `request_signature` |
//! | [`Server::try_set_modem_state`], [`Server::try_notify_line_state`] | `set_modem_state`, `notify_line_state` |
//! | [`Server::try_suspend_flow`], [`Server::try_resume_flow`] | `suspend_flow`, `resume_flow` |
//!
//! Pick one style per instance. Mixing them on one instance is defined (the
//! core reads the alloc layer's backlog first, so bytes leave in call order
//! and events in the order they happened) but not recommended.
//!
//! # Driving contract
//!
//! Both halves are driven the same way:
//!
//! - One instance per TCP connection. After EOF or a reconnect, create a new
//!   one.
//! - Write out the output after construction (the opening is queued at once),
//!   after every receive call and after every accepted call.
//! - After every receive call, use the payload, then call `poll_event` until it
//!   returns `None`.
//! - Pass input that was not consumed again, in order.
//! - Deadlines are yours: the crate has no clock.
//! - Keep reading while you write. A loop that writes everything before it
//!   reads again deadlocks against a peer that does the same (a bridge in
//!   front of a device that echoes, for example) once a transfer outgrows the
//!   socket buffers: for large transfers, read and write from separate
//!   threads or tasks, or write without blocking.
//!
//! The exact driving rules and what they guarantee are on
//! [`Client::receive_into`] and [`Server::receive_into`]. In short: the payload,
//! the events with their positions in it, every byte of output that input
//! produces, and all state depend only on the bytes received and on where
//! your own calls were accepted. They never depend on how the input was split,
//! on the size of the payload buffer (any length of at least one byte), on how
//! many bytes each socket write took, or on when the output was written.
//! `receive_into` returns right after every byte that produced an event, so the
//! payload it returned precedes the event: a bridge applies a new baud rate, a
//! purge or a break at its exact position in the data.
//!
//! Ignoring events (`_ => {}`) loses none of the state the crate keeps: the
//! client keeps [`com_port_state`](Client::com_port_state),
//! [`reported_settings`](Client::reported_settings),
//! [`modem_state`](Client::modem_state) and
//! [`flow_suspended`](Client::flow_suspended), the server
//! [`settings`](Server::settings) and
//! [`com_port_active`](Server::com_port_active). The server reports the
//! client's DTR, RTS and BREAK requests only as events
//! ([`ServerEvent::Dtr`], [`ServerEvent::Rts`], [`ServerEvent::Break`]): a
//! bridge must handle them.
//!
//! A blocking client on the alloc layer (the README's quick start), failing
//! fast when waiting cannot help and reporting why at its deadline:
//!
//! ```no_run
//! use std::io::{self, Read, Write};
//! use std::net::TcpStream;
//! use std::time::{Duration, Instant};
//!
//! use rfc2217::{Client, Event, NotReady, Parity, PortSettings};
//!
//! # #[cfg(feature = "alloc")]
//! fn main() -> io::Result<()> {
//!     let mut sock = TcpStream::connect("192.168.1.50:2217")?;
//!     sock.set_read_timeout(Some(Duration::from_millis(200)))?;
//!     let mut client = Client::new(PortSettings {
//!         baud_rate: 19_200,
//!         parity: Parity::Even,
//!         ..Default::default()
//!     });
//!     let (mut buf, mut serial_rx) = ([0u8; 4096], Vec::new());
//!     let deadline = Instant::now() + Duration::from_secs(3);
//!     loop {
//!         sock.write_all(&client.take_output())?;
//!         match client.readiness() {
//!             Ok(()) => break,
//!             Err(e @ (NotReady::Refused | NotReady::Mismatched(_))) => {
//!                 return Err(io::Error::other(e));
//!             }
//!             Err(e) if Instant::now() >= deadline => {
//!                 return Err(io::Error::new(io::ErrorKind::TimedOut, e));
//!             }
//!             Err(_) => {}
//!         }
//!         match sock.read(&mut buf) {
//!             Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
//!             // Payload may arrive while negotiating: keep it.
//!             Ok(n) => serial_rx.extend(client.receive(&buf[..n])),
//!             Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
//!             Err(e) => return Err(e),
//!         }
//!         while let Some(event) = client.poll_event() {
//!             if let Event::BaudRate(b) = event {
//!                 println!("bridge at {b} baud");
//!             }
//!         }
//!     }
//!     client.set_dtr(true); // infallible; held while negotiating, dropped if refused
//!     client.write_data(b"*IDN?\r\n");
//!     sock.write_all(&client.take_output())?;
//!     Ok(())
//! }
//! # #[cfg(not(feature = "alloc"))]
//! # fn main() {}
//! ```
//!
//! # Readiness and timeouts
//!
//! [`Client::is_ready`] is strict: it is true only while COM-PORT-OPTION is
//! active and the bridge has answered the latest request for every setting
//! with exactly the requested value. [`Client::readiness`] says why not:
//!
//! - [`NotReady::Negotiating`]: nothing has answered the COM-PORT-OPTION
//!   offer. A bridge's raw TCP port never will (nor will ESP-Link, which 0.1
//!   does not support): check the port, and give up at your deadline.
//! - [`NotReady::Refused`]: the peer is a Telnet server without RFC 2217.
//!   Fail fast; data still flows.
//! - [`NotReady::Mismatched`]: the bridge answered with other values (sredird
//!   on a pty reports 8 data bits and no parity whatever was requested).
//!   [`Client::reported_settings`] shows what it reported: accept that, or
//!   fail.
//! - [`NotReady::Pending`]: no answer yet. Some bridges skip some replies
//!   (ser2net: 1.5 stop bits, DCD and DSR flow control). Wait until your
//!   deadline; a later request for the same setting settles it.
//!
//! Readiness may never come, so always apply a deadline: pyserial waits 3 s
//! per step, which is a good default. Nothing in the crate waits for it: data
//! and commands behave the same whether or not the client is ready. A
//! caller that tolerates other values can check
//! [`reported_settings`](Client::reported_settings) instead. A baud rate of 0
//! is the RFC 2217 query: the client asks for the bridge's rate, and any rate
//! the bridge reports answers it.
//!
//! # Embedded and `no_std`
//!
//! Firmware depends on the crate without its default features, and so must
//! every crate in the firmware's dependency graph that depends on it:
//!
//! ```toml
//! rfc2217 = { version = "0.1", default-features = false }
//! ```
//!
//! If any crate in the graph enables `alloc`, a binary without a
//! `#[global_allocator]` fails to link ("no global memory allocator found but
//! one is required"), even if it calls only the core. The README has a clippy
//! `disallowed-methods` recipe that keeps the alloc layer's methods out of
//! firmware code.
//!
//! The core loop, in short: input in order, payload before events, then
//! output with partial writes, until it is blocked. The README's version
//! runs over a two-method non-blocking socket trait that maps onto smoltcp,
//! embassy-net or lwIP.
//!
//! ```
//! use rfc2217::{Client, Event};
//!
//! /// One pass: `rx` holds what the socket delivered and keeps what was not
//! /// consumed; `send` writes bytes to the socket and returns how many it took.
//! /// Returns once `rx` is used up and the output written, or the socket is
//! /// full.
//! fn poll(
//!     client: &mut Client,
//!     rx: &mut &[u8],
//!     send: &mut impl FnMut(&[u8]) -> usize,
//!     uart: &mut impl FnMut(&[u8]),
//!     on_event: &mut impl FnMut(Event),
//! ) {
//!     let mut payload = [0u8; 64]; // any length of at least one byte
//!     loop {
//!         loop {
//!             let p = client.receive_into(rx, &mut payload);
//!             *rx = &rx[p.consumed..];
//!             uart(&payload[..p.payload_len]); // payload before the event...
//!             while let Some(e) = client.poll_event() {
//!                 on_event(e); // ...then the event
//!             }
//!             if p.consumed == 0 {
//!                 break; // all consumed, or the output must be written first
//!             }
//!         }
//!         while client.has_output() {
//!             let n = send(client.output());
//!             if n == 0 {
//!                 return; // the socket is full: call again once it takes more
//!             }
//!             client.consume_output(n);
//!         }
//!         if rx.is_empty() {
//!             return;
//!         }
//!         // The input stopped for output room, which is free again: go on.
//!     }
//! }
//! # // The bridge accepts COM-PORT-OPTION and sends "x"; a socket that takes
//! # // 3 bytes per write gets the opening and the settings round.
//! # let mut client = Client::new(rfc2217::PortSettings::default());
//! # let mut rx = &[0xFF, 0xFD, 0x2C, b'x'][..];
//! # let (mut sent, mut serial, mut events) = (0, [0u8; 1], 0);
//! # let mut send = |b: &[u8]| { sent += b.len().min(3); b.len().min(3) };
//! # let mut uart = |b: &[u8]| if let [x] = b { serial[0] = *x };
//! # poll(&mut client, &mut rx, &mut send, &mut uart, &mut |_: Event| events += 1);
//! # assert_eq!((rx.len(), serial, events, sent), (0, [b'x'], 1, 18 + 38));
//! # // 80 refused options in one read fill the output halfway through: one
//! # // call still processes all of it.
//! # let mut client = Client::new(rfc2217::PortSettings::default());
//! # let input = [[0xFF, 0xFD, 0x18].repeat(80), b"y".to_vec()].concat();
//! # let (mut rx, mut sent, mut serial) = (&input[..], 0, [0u8; 1]);
//! # let mut send = |b: &[u8]| { sent += b.len(); b.len() };
//! # let mut uart = |b: &[u8]| if let [x] = b { serial[0] = *x };
//! # poll(&mut client, &mut rx, &mut send, &mut uart, &mut |_: Event| {});
//! # assert_eq!((rx.len(), serial, sent), (0, [b'y'], 18 + 80 * 3));
//! # assert!(!client.has_output());
//! ```
//!
//! # Memory and backpressure
//!
//! | Store | Capacity |
//! |---|---|
//! | Output buffer | 256 bytes ([`Client::OUTPUT_CAPACITY`], [`Server::OUTPUT_CAPACITY`]), part of it kept free for the replies input causes |
//! | Events | 1 slot |
//! | Subnegotiation | 66 bytes kept; given up after 1024 bytes, and the rest arrives as data |
//! | Signatures and unknown replies | 64 bytes ([`InlineBytes`]) plus their original length |
//! | Telnet options | 3 slots (BINARY, SGA, COM-PORT-OPTION) |
//! | Unanswered setting requests | 8 per setting |
//!
//! `size_of::<Client>()` is 520 bytes and `size_of::<Server>()` 496, on
//! x86_64 and on thumbv6m-none-eabi alike; the `alloc` feature adds one
//! pointer to each (528 and 504 bytes on x86_64). [`Event`] and
//! [`ServerEvent`] are 68 bytes.
//!
//! What happens when a store is full:
//!
//! - Input stops before a byte it cannot complete: an event that has not
//!   been polled, a full payload buffer, or too little output room for the
//!   replies the byte may cause. The bytes it did not consume stay with you;
//!   pass them again. With the output written out and the events polled, a
//!   call with non-empty input always makes progress, so a call that
//!   consumed nothing means the input was empty, output is still waiting to
//!   be written, or an event is waiting.
//! - Commands and notifications are atomic: they write everything or return
//!   an error and change nothing. `try_write_data` takes the longest prefix
//!   of the data that fits and returns its length (`OutputFull` only when
//!   not one byte fits; a client also returns `Negotiating` until
//!   COM-PORT-OPTION is settled). No call uses the room kept for the replies to input, so
//!   writing cannot starve the protocol, and with the output drained every
//!   call the state allows succeeds.
//! - Nothing is ever dropped silently. Overflowing a fixed store would be a
//!   crate bug, and panics. The two deliberate limits are visible: long
//!   signatures and unknown replies keep 64 bytes plus their true length, and
//!   a subnegotiation longer than 1024 bytes is given up and the rest of it
//!   arrives as data.
//! - The alloc layer never pushes back: its output, its events and the calls
//!   it holds while negotiating grow on the heap until you drain them.
//!
//! # Async
//!
//! One task owns the client (or server) and its socket. Do not await a write
//! while input may be waiting if the peer may do the same: two "flush
//! everything, then read" loops deadlock once the socket buffers are smaller
//! than one protocol burst or one transfer. Drive the core loop from
//! readiness notifications (with embassy-net, `read_with` and `write_with`
//! inside a select on read and write readiness), or split reading and
//! writing so that input is always consumed. With the alloc layer and
//! `select!`, put only cancel-safe operations inside the `select!`: append
//! `take_output()` to a buffer of your own after it, and write that buffer
//! from a branch of the `select!` with a single `write` (which is
//! cancel-safe), never with an awaited `write_all` while input may be
//! waiting.
//!
//! # Feature flags
//!
//! - `alloc` (on by default): the alloc layer's `Vec`-based methods. It adds
//!   methods only; the core is the same without it. The final binary then
//!   needs a global allocator.
//! - `tracing` (off by default): debug logs through
//!   [`tracing`](https://docs.rs/tracing) for protocol oddities the crate
//!   ignores, and a trace of every decoded reply and request. Needs a target
//!   with atomic compare-and-swap.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Kept on one line, so that a grep for the crate import shows its feature gate.
#[rustfmt::skip]
#[cfg(feature = "alloc")] extern crate alloc;

#[cfg(feature = "alloc")]
mod buffered;
mod bytes;
mod client;
mod io;
mod lines;
mod log;
mod negotiation;
mod server;
mod settings;
mod telnet;

pub use bytes::InlineBytes;
pub use client::{Client, ComPortState, Event, NotReady};
pub use io::{CommandError, Progress};
pub use lines::{LineState, ModemState};
pub use server::{Server, ServerEvent};
pub use settings::{
    DataBits, FlowControl, Parity, PartialSettings, PortSettings, Purge, Setting, SettingSet,
    StopBits,
};

/// Compiles every code block of the README as a doctest.
#[cfg(all(doctest, feature = "alloc"))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
