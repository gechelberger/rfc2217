# rfc2217

Sans-IO [RFC 2217](https://www.rfc-editor.org/rfc/rfc2217) (Telnet COM Port
Control) client and server state machines for serial-over-TCP bridges such as
ser2net.

- An allocation-free `no_std` core with fixed memory (about half a KiB per
  connection) that pushes back instead of losing anything.
- A `Vec`-based convenience layer behind the default `alloc` feature.
- Zero dependencies by default.

Tested with ser2net 4.6.4 and 4.3.11, sredird 2.2.2, pyserial 3.5
(PortManager, and its client against `Server`). Designed for other RFC 2217
bridges such as Moxa NPort, Lantronix and Digi (untested).

Serial-to-Ethernet bridges usually expose a serial port in two ways: a raw TCP
socket that carries only bytes, and an RFC 2217 Telnet service that also lets
the client set the baud rate, data bits, parity, stop bits and flow control on
the far side. This crate implements the RFC 2217 service as two pure state
machines:

- **`Client`**: the side your program uses to connect to a bridge. It handles
  Telnet option negotiation, IAC escaping, and the COM-PORT-OPTION
  subnegotiations that configure the remote port, and it tells you whether the
  bridge confirmed your settings.
- **`Server`**: the bridge side. Use it for simulators, tests, or a bridge of
  your own.

Neither one opens a socket, spawns a thread or reads a clock. You pass in the
bytes you read and get back the bytes to write, so the same code works with
blocking `std::net`, tokio, an embedded network stack, or an in-memory test
harness.

## Install

```sh
cargo add rfc2217
```

or, in `Cargo.toml`:

```toml
[dependencies]
rfc2217 = "0.1"
```

For firmware, without the `alloc` feature:

```sh
cargo add rfc2217 --no-default-features
```

```toml
[dependencies]
rfc2217 = { version = "0.1", default-features = false }
```

| Feature | Default | What it adds |
| --- | --- | --- |
| `alloc` | on | The alloc layer: `Vec`-based methods (`receive`, `take_output`, `write_data`, infallible commands). The final binary needs a global allocator. |
| `tracing` | off | Debug logs through `tracing` for protocol oddities the crate ignores, and a trace of every decoded reply and request. Needs a target with atomic compare-and-swap. |

The minimum supported Rust version is 1.85.

## Quick start

A blocking client over `std::net::TcpStream`, on the alloc layer. It gives up
at once when waiting cannot help, and reports why at its deadline:

```rust,no_run
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use rfc2217::{Client, Event, NotReady, Parity, PortSettings};

fn main() -> io::Result<()> {
    let mut sock = TcpStream::connect("192.168.1.50:2217")?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut client = Client::new(PortSettings {
        baud_rate: 19_200,
        parity: Parity::Even,
        ..Default::default()
    });
    let (mut buf, mut serial_rx) = ([0u8; 4096], Vec::new());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        sock.write_all(&client.take_output())?;
        match client.readiness() {
            Ok(()) => break,
            Err(e @ (NotReady::Refused | NotReady::Mismatched(_))) => {
                return Err(io::Error::other(e));
            }
            Err(e) if Instant::now() >= deadline => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, e));
            }
            Err(_) => {}
        }
        match sock.read(&mut buf) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            // Payload may arrive while negotiating: keep it.
            Ok(n) => serial_rx.extend(client.receive(&buf[..n])),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
        while let Some(event) = client.poll_event() {
            if let Event::BaudRate(b) = event {
                println!("bridge at {b} baud");
            }
        }
    }
    client.set_dtr(true); // infallible; held while negotiating, dropped if refused
    client.write_data(b"*IDN?\r\n");
    sock.write_all(&client.take_output())?;
    Ok(())
}
```

`examples/blocking_client.rs` is this loop as a command-line tool, and
`examples/sim_server.rs` a simulated bridge to run it against:

```sh
cargo run --example sim_server
cargo run --example blocking_client -- 127.0.0.1:2217 115200 hello
```

## Driving model

Both halves are driven the same way, through either of two layers. The core
exists in every configuration, never allocates and pushes back when its fixed
buffers are full; the alloc layer is a loop over it that never pushes back.

| | Core (every configuration) | Alloc layer (feature `alloc`) |
| --- | --- | --- |
| Feed bytes read from the socket | `receive_into(&input, &mut payload) -> Progress` | `receive(&bytes) -> Vec<u8>`, `receive_until_event(&input, &mut Vec<u8>) -> usize` |
| Bytes to write to the socket | `output() -> &[u8]`, then `consume_output(n)` | `take_output() -> Vec<u8>` |
| Anything to write? | `has_output()` | `has_output()` |
| What the peer did | `poll_event()` | `poll_event()` |
| Send serial data (`0xFF` is escaped) | `try_write_data(&data) -> Result<usize, CommandError>` | `write_data(&data)` |
| Commands | `try_set_dtr(on) -> Result<(), CommandError>`, ... | `set_dtr(on)`, ... (infallible) |

The rules:

- Write the output after construction (the opening is queued at once), after
  every receive call and after every call.
- After every receive call, use the payload, then call `poll_event` until it
  returns `None`.
- With the core, pass input that `receive_into` did not consume again, in
  order.
- One instance per TCP connection: after EOF or a reconnect, create a new one.
  The crate has no clock, so deadlines are yours.
- Keep reading while you write. A loop that writes everything before it reads
  again deadlocks against a peer that does the same (a bridge in front of a
  device that echoes, for example) once a transfer outgrows the socket
  buffers: for large transfers, read and write from separate threads or
  tasks, or write without blocking.

The payload, the events, the output and the state do not depend on how the
input was split, on the size of your buffers, or on how many bytes each socket
write took. `receive_into` returns right after every byte that produced an
event, so the payload it returned precedes the event. Pick one layer per
instance; mixing them on one instance is defined but not recommended.

## Readiness and timeouts

`Client::is_ready()` is strict: it is true only when COM-PORT-OPTION is active
and the bridge has answered the latest request for every setting with exactly
the requested value. It may never become true, so always apply a deadline
(pyserial waits 3 s per step). `Client::readiness()` returns the reason as a
`NotReady`, whose `Display` is a ready-made diagnostic:

| `NotReady` | Likely cause | What to do |
| --- | --- | --- |
| `Negotiating` | Nothing answered the COM-PORT-OPTION offer: the bridge's raw TCP port, a wrong port, or ESP-Link (not supported in 0.1) | Check the port; give up at your deadline |
| `Refused` | A plain Telnet server without RFC 2217 | Fail fast; the connection still carries data |
| `Mismatched(settings)` | The bridge applied other values, for example sredird on a pty reports 8N for 7E | Inspect `reported_settings()`; accept the values or fail |
| `Pending(settings)` | No answer yet; some bridges skip replies, for example ser2net for 1.5 stop bits or DCD/DSR flow control | Wait until your deadline; a later request for the same setting settles it |
| `Pending(settings)`, with `Event::Unknown` copies of your own requests | A raw TCP port in front of a device that echoes what it receives (your `DO COM-PORT-OPTION` came back, so the client went active) | Check the port |

Nothing in the crate waits for readiness: data and commands behave the same
whether or not the client is ready. `reported_settings()` holds the last value
the bridge reported for each setting, for callers that tolerate other values.
A baud rate of 0 is the RFC 2217 query: the client asks for the bridge's
current rate, and any rate the bridge reports answers it.

## Client commands

| Alloc layer | Core | Effect |
| --- | --- | --- |
| `set_port_settings(settings)` | `try_set_port_settings` | Replace the requested settings; all five are sent now if COM-PORT-OPTION is active, else once it becomes active. |
| `set_dtr(on)`, `set_rts(on)`, `set_break(on)` | `try_set_dtr`, `try_set_rts`, `try_set_break` | Drive the control lines. Replies (`Event::Dtr`, `Rts`, `Break`) may never arrive. |
| `purge(Purge::Receive \| Transmit \| Both)` | `try_purge` | Ask the bridge to discard buffered data; answered with `Event::Purged`. |
| `request_signature()` | `try_request_signature` | Ask for the bridge's signature; answered with `Event::Signature`. |

While COM-PORT-OPTION is still negotiating, the alloc layer holds data and
commands in one ordered queue and sends them right after the settings round;
if the peer refuses the option, held data is sent and held commands are
dropped. While it is refused, alloc-layer commands are dropped and data still
flows. The core holds nothing: its calls (all but `try_set_port_settings`,
which only replaces the request) return `CommandError::Negotiating` while
negotiating; its commands return `CommandError::Inactive` while refused; and
a call returns `CommandError::OutputFull` when the output buffer lacks room
for it (write out the output, then retry). Data flows while refused, a plain
Telnet data channel.

Replies to DTR, RTS and BREAK may never arrive (ser2net on a pty), and sredird
answers DTR and RTS with its flow-control value (`Event::FlowControl`, which
can make flow control read as mismatched while a flow-control change is
outstanding). Flow control is advisory: after `FLOWCONTROL-SUSPEND`, writes
still go out; check `flow_suspended()` and pause yourself.

### Coming from pyserial

| pyserial | This crate |
| --- | --- |
| `serial_for_url("rfc2217://host:port")` | `Client::new(settings)` plus your loop (see the quick start) |
| "Remote does not seem to support RFC2217" | `NotReady::Negotiating` or `NotReady::Refused` |
| "remote rejected value" | `NotReady::Mismatched` |
| 3 s timeout per parameter | `NotReady::Pending` at your deadline |
| `s.dtr = on`, `s.rts = on`, `s.break_condition = on` | `set_dtr(on)`, `set_rts(on)`, `set_break(on)` |
| `reset_input_buffer()`, `reset_output_buffer()` | `purge(Purge::Receive)`, `purge(Purge::Transmit)` |
| `s.cts`, `s.dsr`, `s.ri`, `s.cd` | `modem_state().map(\|m\| m.cts())`, `.dsr()`, `.ri()`, `.cd()` |
| `?ign_set_control` | not needed: SET-CONTROL replies are classified by value |
| `?poll_modem` | the `Server` answers it |

## Embedded and `no_std`

The core loop on fixed buffers: no `Vec`, no allocation, any payload buffer of
at least one byte, any partial write. It is written against a two-method
non-blocking socket trait, which maps directly onto smoltcp's
`send_slice`/`recv_slice`, embassy-net's `write_with`/`read_with`, lwIP, or a
std socket in non-blocking mode:

```rust
use rfc2217::{Client, Event};

/// Your stack's non-blocking TCP socket.
pub trait Socket {
    /// Queue up to `bytes.len()` bytes; how many were taken (0 when the send buffer is full).
    fn send(&mut self, bytes: &[u8]) -> usize;
    /// Copy what has arrived into `buf`; how many bytes (0 when nothing is there). 0 also
    /// stands for the end of the stream and errors: report those out of band.
    fn recv(&mut self, buf: &mut [u8]) -> usize;
}

/// One connection: the client (520 bytes) plus a 256-byte receive buffer.
pub struct Link {
    pub client: Client,
    rx: [u8; 256],
    at: usize,
    end: usize,
}

impl Link {
    pub fn new(client: Client) -> Self {
        Link { client, rx: [0; 256], at: 0, end: 0 }
    }

    /// Call whenever the socket may have made progress (smoltcp: after every `iface.poll`)
    /// and whenever you add to `tx`, the serial data still to send; it is advanced past
    /// what was taken. Returns once it is blocked: all input processed, and `tx` empty
    /// or refused, or the socket full.
    pub fn poll(
        &mut self,
        sock: &mut impl Socket,
        tx: &mut &[u8],
        uart: &mut impl FnMut(&[u8]),
        on_event: &mut impl FnMut(Event),
    ) {
        let mut payload = [0u8; 64];
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
                let p = self.client.receive_into(&self.rx[self.at..self.end], &mut payload);
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
            // 2. serial data; refused while COM-PORT-OPTION is negotiating or the output
            //    is full, and what was not taken stays in `tx`
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
```

`poll` sends `tx` as soon as COM-PORT-OPTION has stopped negotiating (the
settings round goes out first), so when to start passing data is your policy:
for example once `readiness()` is `Ok`, or once you have accepted the values
`reported_settings()` shows. Deadlines are yours, exactly as in the quick
start: check `link.client.readiness()` against your timer.
`examples/core_client.rs` runs this `Link` over a non-blocking `TcpStream`,
and keeps the end of the stream and socket errors in its socket wrapper.

Every store is fixed; nothing in the core grows with the input:

| Store | Capacity |
| --- | --- |
| Output buffer | 256 bytes; the client keeps 45 free for the replies input causes, the server 19 (up to 134 with a 64-byte signature of `0xFF` bytes) |
| Events | 1 slot: input stops until the event is polled |
| Subnegotiation | 66 bytes kept; given up after 1024 bytes, and the rest arrives as data |
| Signatures and unknown replies (`InlineBytes`) | 64 bytes, plus their original length |
| Telnet options | 3 slots (BINARY, SUPPRESS-GO-AHEAD, COM-PORT-OPTION) |
| Unanswered requests per setting | 8 |

| `size_of` | x86_64 | x86_64, `alloc` | thumbv6m-none-eabi | thumbv6m-none-eabi, `alloc` |
| --- | --- | --- | --- | --- |
| `Client` | 520 | 528 | 520 | 524 |
| `Server` | 496 | 504 | 496 | 500 |
| `Event`, `ServerEvent` | 68 | 68 | 68 | 68 |

Firmware, and every crate in its dependency graph that depends on `rfc2217`,
must use `default-features = false`. If any of them enables `alloc`, a binary
without a `#[global_allocator]` fails to link ("no global memory allocator
found but one is required"), even if it calls only the core.

Async: one task owns the client and its socket. Do not await a write while
input may be waiting if the peer may do the same: two "flush everything, then
read" loops deadlock once the socket buffers are smaller than one protocol
burst or one transfer. Drive `Link::poll` from readiness notifications (with
embassy-net, `read_with` and `write_with` inside a select on read and write
readiness), or split reading and writing so that input is always consumed.
With the alloc layer and `select!`, put only cancel-safe operations inside the
`select!`: append `take_output()` to a buffer of your own after it, and write
that buffer from a branch of the `select!` with a single `write` (which is
cancel-safe), never with an awaited `write_all` while input may be waiting.

To keep the alloc layer's methods out of firmware code even when another crate
enables the feature, list them in the firmware crate's `clippy.toml`:

```toml
# clippy.toml of a firmware crate: keep rfc2217's heap-backed methods out
disallowed-methods = [
  { path = "rfc2217::Client::receive", reason = "allocates; use receive_into" },
  { path = "rfc2217::Client::receive_until_event", reason = "allocates; use receive_into" },
  { path = "rfc2217::Client::take_output", reason = "allocates; use output and consume_output" },
  { path = "rfc2217::Client::write_data", reason = "allocates; use try_write_data" },
  { path = "rfc2217::Client::set_port_settings", reason = "allocates; use try_set_port_settings" },
  { path = "rfc2217::Client::set_dtr", reason = "allocates; use try_set_dtr" },
  { path = "rfc2217::Client::set_rts", reason = "allocates; use try_set_rts" },
  { path = "rfc2217::Client::set_break", reason = "allocates; use try_set_break" },
  { path = "rfc2217::Client::purge", reason = "allocates; use try_purge" },
  { path = "rfc2217::Client::request_signature", reason = "allocates; use try_request_signature" },
  { path = "rfc2217::Server::receive", reason = "allocates; use receive_into" },
  { path = "rfc2217::Server::receive_until_event", reason = "allocates; use receive_into" },
  { path = "rfc2217::Server::take_output", reason = "allocates; use output and consume_output" },
  { path = "rfc2217::Server::write_data", reason = "allocates; use try_write_data" },
  { path = "rfc2217::Server::set_modem_state", reason = "allocates; use try_set_modem_state" },
  { path = "rfc2217::Server::notify_line_state", reason = "allocates; use try_notify_line_state" },
  { path = "rfc2217::Server::suspend_flow", reason = "allocates; use try_suspend_flow" },
  { path = "rfc2217::Server::resume_flow", reason = "allocates; use try_resume_flow" },
]
```

## Server

A bridge on the core applies each event at its byte position in the data:
`receive_into` returns right after the byte that produced it, so the payload
before it goes to the UART first.

```rust
use std::io::{self, Write};

use rfc2217::{ModemState, PortSettings, Purge, Server, ServerEvent};

/// Your UART.
pub trait Uart: Write {
    fn modem_lines(&self) -> ModemState;
    fn reconfigure(&mut self, settings: PortSettings);
    fn discard_tx(&mut self);
    fn set_break(&mut self, on: bool);
    fn set_dtr(&mut self, on: bool);
    fn set_rts(&mut self, on: bool);
}

/// One pass over the bytes read from the client (`rx[*at..]`) and those the UART
/// received (`from_uart[*sent..]`). Write out `server.output()` after every pass.
pub fn bridge(
    server: &mut Server,
    uart: &mut impl Uart,
    rx: &[u8],
    at: &mut usize,
    from_uart: &[u8],
    sent: &mut usize,
) -> io::Result<()> {
    // The UART's modem lines: stored before the client enables COM-PORT-OPTION
    // (pyserial's client needs them), and sent when they change. On OutputFull
    // nothing is stored, and the next pass sends them.
    let _ = server.try_set_modem_state(uart.modem_lines());

    let mut to_uart = [0u8; 64];
    let p = server.receive_into(&rx[*at..], &mut to_uart);
    *at += p.consumed;
    uart.write_all(&to_uart[..p.payload_len])?; // bytes before the event...
    while let Some(ev) = server.poll_event() {   // ...then the event, at its position
        match ev {
            ServerEvent::SettingsChanged(s) => uart.reconfigure(s),
            ServerEvent::Purge(Purge::Transmit | Purge::Both) => uart.discard_tx(),
            ServerEvent::Break(on) => uart.set_break(on),
            // DTR and RTS reach the application only as events.
            ServerEvent::Dtr(on) => uart.set_dtr(on),
            ServerEvent::Rts(on) => uart.set_rts(on),
            _ => {}
        }
    }

    // UART → client, as far as it fits.
    if let Ok(n) = server.try_write_data(&from_uart[*sent..]) {
        *sent += n;
    }
    Ok(())
}
```

Call `bridge` until `rx` is used up and whenever the UART has received more,
writing out the output after every call: a pass that consumed nothing is
waiting for that.

- `Server::new` queues the opening (`WILL`/`DO` BINARY and SUPPRESS-GO-AHEAD;
  12 bytes): write it out as soon as the client connects. COM-PORT-OPTION is
  the client's to offer and perform: the server accepts its `WILL` with `DO`,
  refuses `DO COM-PORT-OPTION` (`WONT`), and ignores requests until the
  client's `WILL` is accepted. It never asks for the option itself, because
  an unsolicited `DO COM-PORT-OPTION` trips a race in pyserial 3.5's client.
- Every request is acknowledged automatically with the value now in effect:
  the server suits simulators, tests, and bridges whose port accepts every
  value RFC 2217 can encode. The application cannot refuse a value in 0.1.
- `ServerEvent::SettingsChanged` comes once per request that changed one of
  the five settings (baud rate, data bits, parity, stop bits, flow control),
  never coalesced: a client that sets all five causes up to five events, each
  with the settings applied so far. DTR, RTS and BREAK requests produce
  `ServerEvent::Dtr`, `Rts` and `Break` every time, also when nothing changed,
  and never `SettingsChanged`; the server has no accessor for these lines, so
  a bridge must handle the events. Mask changes produce no event.
- The client's masks are applied with their RFC defaults: no line-state bits
  until the client sets a line-state mask, every modem-state bit. The server
  sends the modem state it was given when the client enables COM-PORT-OPTION,
  changes the mask or polls, and computes the change bits itself; it never
  invents a state. RFC 2217 has no change bit for RI turning on, so a rising
  RI alone sends nothing: the client sees it in the next notification or
  poll.
- pyserial's client needs an initial modem state (supply one before or on
  `ComPortActive`, or reading `cts` fails), ignores `suspend_flow` (it expects
  SUSPEND as code 8, not 108), and may poll the modem state
  (`?poll_modem`), which is answered.

`examples/sim_server.rs` is a complete simulated bridge whose UART echoes the
data back.

## Protocol coverage

| RFC 2217 command | Client sends | Client decodes the reply as | Server |
| --- | --- | --- | --- |
| `SIGNATURE` (0) | `request_signature()` | `Event::Signature` (an empty one nobody asked for is the server asking for ours: no event, not answered in 0.1) | Answers a request with its signature; a client's signature becomes `ServerEvent::ClientSignature` |
| `SET-BAUDRATE` (1) | in the settings round (0 = query) | `Event::BaudRate` (4 bytes, or sredird's 8) | Applies and acknowledges; a query is answered with the current rate |
| `SET-DATASIZE` (2) | in the settings round | `Event::DataBits` | Applies and acknowledges |
| `SET-PARITY` (3) | in the settings round | `Event::Parity` | Applies and acknowledges |
| `SET-STOPSIZE` (4) | in the settings round | `Event::StopBits` | Applies and acknowledges |
| `SET-CONTROL` (5) | flow control in the round; `set_dtr`, `set_rts`, `set_break` | `FlowControl`, `Dtr`, `Rts`, `Break`, classified by value | Outbound flow control, BREAK, DTR, RTS and their queries; inbound flow-control requests are answered with the inbound value the outbound setting implies |
| `NOTIFY-LINESTATE` (6) | n/a | `Event::LineState(LineState)` | `try_notify_line_state`, through the client's line-state mask |
| `NOTIFY-MODEMSTATE` (7) | n/a | `Event::ModemState(ModemState)`, kept in `modem_state()` | `try_set_modem_state`, through the client's modem-state mask; an empty one (pyserial's poll) is answered |
| `FLOWCONTROL-SUSPEND` / `-RESUME` (8/9) | not in 0.1 | `FlowSuspended` / `FlowResumed` on each change, `flow_suspended()` | `try_suspend_flow` / `try_resume_flow`; the client's own are ignored |
| `SET-LINESTATE-MASK` / `SET-MODEMSTATE-MASK` (10/11) | not in 0.1 | acknowledgements ignored | Applied (RFC defaults `0x00` and `0xFF`) and echoed |
| `PURGE-DATA` (12) | `purge()` | `Event::Purged` | `ServerEvent::Purge`, then echoed |

Supported values: `DataBits` 5 to 8; `Parity` None, Odd, Even, Mark and Space;
`StopBits` 1, 2 and 1.5; `FlowControl` None, XON/XOFF, RTS/CTS, DCD and DSR
(outbound). A COM-PORT-OPTION reply the client cannot decode becomes
`Event::Unknown`, and counts as a mismatched answer when it answers a setting.

`LineState` and `ModemState` are the RFC 2217 bytes, with a named constant per
bit:

| Bit | `LineState` | `ModemState` |
| --- | --- | --- |
| `0x80` | `TIMEOUT_ERROR` | `CD` (carrier detect) |
| `0x40` | `SHIFT_REGISTER_EMPTY` | `RI` (ring indicator) |
| `0x20` | `HOLDING_REGISTER_EMPTY` | `DSR` (data set ready) |
| `0x10` | `BREAK_DETECT` | `CTS` (clear to send) |
| `0x08` | `FRAMING_ERROR` | `DELTA_CD` |
| `0x04` | `PARITY_ERROR` | `TRAILING_EDGE_RI` |
| `0x02` | `OVERRUN_ERROR` | `DELTA_DSR` |
| `0x01` | `DATA_READY` | `DELTA_CTS` |

## Known bridge quirks

| Peer | Behaviour | Effect with this crate |
| --- | --- | --- |
| ser2net 4.6.4 | no reply to 1.5 stop bits or DCD/DSR flow control; no DTR/RTS reply on a pty; empty SIGNATURE reply; `DO 44` twice then `WONT 44`; crashes on REQUEST-DTR/RTS with a pty | `Pending` at the deadline (a later request settles it); no `Dtr`/`Rts` events; an empty `Signature`; the client never sends those queries |
| ser2net 4.3.11 | no reply to 1.5 stop bits; answers DCD/DSR flow control with 14; answers BREAK-OFF with BREAK-ON; no DTR/RTS reply on a pty | `Pending({StopBits})`; `Mismatched({FlowControl})`; `Break(true)` reported for both |
| sredird 2.2.2 | 8-byte baud reply; DTR/RTS answered with the flow-control value; on a pty acks 8N for a 7E request; answers values it lacks (250000 baud, DCD/DSR flow control) with what it set instead (9600, none); a NOTIFY-MODEMSTATE burst after every exchange; device data with many `0xFF` bytes can be lost or corrupted when it arrives in a burst (one 2000-byte burst is enough; a bug in its ring buffer) | decoded; `FlowControl` events after `set_dtr` (flow control can read as mismatched while a flow change is outstanding); `Mismatched({DataBits, Parity})`, `Mismatched({BaudRate})`, `Mismatched({FlowControl})`; the client delivers what arrives on the wire |
| pyserial PortManager (on a pty) | offers `WILL 44` before `DO 44`; no SIGNATURE reply; no reply to DCD/DSR flow control | harmless; no event; `Pending({FlowControl})` |
| pyserial client (against `Server`) | needs an initial NOTIFY-MODEMSTATE; expects SUSPEND as code 8; optional modem polls | set the modem state before or on `ComPortActive`; it ignores `suspend_flow`; polls are answered |
| ESP-Link | any `IAC DO` derails its parser; acknowledges nothing (emulator evidence) | **not supported in 0.1** |

## Caveats

- **Binary transparency.** Both halves are 8-bit transparent in both
  directions, whatever the BINARY option's state: `0xFF` is doubled on send and
  `IAC IAC` collapsed on receive, with no NVT `CR NUL` / `CR LF` translation.
- **Flow control is advisory.** `FLOWCONTROL-SUSPEND` neither refuses nor
  buffers writes.
- **The core's buffers are fixed and push back.** An unpolled event or output
  that is never written stalls input; nothing is lost.
- **The alloc layer grows until drained.** Its output, event and held-call
  queues live on the heap until `take_output`, `poll_event` or activation
  drains them, and its output buffer keeps its largest size until
  `take_output` hands the bytes out.
- **Signatures and unknown replies keep 64 bytes**, plus their original
  length (`InlineBytes::original_len`, `is_truncated`).
- **A stray `IAC SB` swallows at most 1 KiB of data**: a subnegotiation is
  given up after 1024 bytes, and the bytes after that arrive as data.
- **A raw, non-Telnet peer's `0xFF` swallows the next byte**, as Telnet
  requires.
- **A wrong-length SET gets no reply from the `Server`.**
- **The client decodes COM-PORT-OPTION replies before activation** too, as
  unsolicited reports; the server ignores requests until the client enables
  the option.
- **Bare Telnet commands such as BRK and AYT are ignored**, and options other
  than BINARY, SUPPRESS-GO-AHEAD and COM-PORT-OPTION are refused. An option
  that the peer enables more than 8 times is refused from then on, which ends
  negotiation loops.

## Stability

The crate follows semantic versioning: while it is 0.x, a minor release may
break the API and a patch release never does. The MSRV (1.85) is raised only
in a minor release. Changes to wire behaviour and to the fixed capacities are
listed in the [CHANGELOG](CHANGELOG.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

## Development

```sh
cargo test && cargo test --no-default-features   # unit tests, interop replays, doctests
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps --open
```
