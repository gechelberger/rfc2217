//! A simulated RFC 2217 bridge on the core API: one client at a time, in
//! front of a loopback "UART" that sends every byte it receives back to the
//! client. Events are applied at their position in the data (the README's
//! server loop), and printed to stderr.
//!
//! Usage: `sim_server [addr]` (default `127.0.0.1:2217`)
//!
//! Then connect a client, for example:
//!
//! ```text
//! cargo run --example blocking_client -- 127.0.0.1:2217 115200 hello
//! cargo run --example core_client -- 127.0.0.1:2217 115200 hello
//! ```

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use rfc2217::{ModemState, PortSettings, Purge, Server, ServerEvent};

fn main() -> ExitCode {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:2217".to_owned());
    match run(&addr) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sim_server: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(addr: &str) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("listening on {}", listener.local_addr()?);
    loop {
        let mut server = Server::new(PortSettings::default()).with_signature("rfc2217 sim_server");
        // The simulated port's modem lines. pyserial's client needs a modem
        // state; it is stored now and reported when the client enables
        // COM-PORT-OPTION.
        let lines = ModemState::CTS | ModemState::DSR | ModemState::CD;
        if let Err(e) = server.try_set_modem_state(lines) {
            unreachable!("a modem state is stored while inactive: {e}");
        }
        let (stream, peer) = listener.accept()?;
        eprintln!("{peer}: connected");
        match serve(&mut server, stream) {
            Ok(()) => eprintln!("{peer}: disconnected"),
            Err(e) => eprintln!("{peer}: {e}"),
        }
    }
}

/// Drive one connection until the client closes it.
fn serve(server: &mut Server, mut stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    // Non-blocking: a socket that takes no more output never keeps the loop
    // from reading, so a client that sends without reading cannot stall it.
    stream.set_nonblocking(true)?;
    let mut uart = LoopbackUart::default();
    let (mut rx, mut at, mut end) = ([0u8; 1024], 0, 0);
    let mut to_uart = [0u8; 256];
    loop {
        let mut busy = false;
        // UART → client, until the UART has nothing left or the socket takes
        // no more. The output also carries the opening and the replies.
        loop {
            if let Ok(n) = server.try_write_data(uart.received()) {
                uart.consume(n);
            }
            busy |= write_out(server, &mut stream)? > 0;
            if server.has_output() || uart.received().is_empty() {
                break;
            }
        }
        // Input, in order: read only once the last read is used up.
        if at == end {
            match stream.read(&mut rx) {
                Ok(0) => return Ok(()),
                Ok(n) => (at, end) = (0, n),
                Err(e) if retry_later(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if at < end {
            let p = server.receive_into(&rx[at..end], &mut to_uart);
            at += p.consumed;
            busy |= p.consumed > 0;
            uart.write(&to_uart[..p.payload_len]); // bytes before the event...
            while let Some(event) = server.poll_event() {
                // ...then the event, at its position.
                eprintln!("event: {event:?}");
                match event {
                    ServerEvent::SettingsChanged(settings) => uart.reconfigure(settings),
                    ServerEvent::Purge(purge) => uart.purge(purge),
                    ServerEvent::Break(on) => uart.set_break(on),
                    // DTR and RTS reach the application only as events.
                    ServerEvent::Dtr(on) => uart.set_dtr(on),
                    ServerEvent::Rts(on) => uart.set_rts(on),
                    _ => {}
                }
            }
        }
        if !busy {
            // Nothing moved either way: wait a little instead of spinning.
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Write out as much of the server's output as the socket takes now
/// (partial writes are fine); returns how many bytes it took.
fn write_out(server: &mut Server, stream: &mut TcpStream) -> io::Result<usize> {
    let mut written = 0;
    while server.has_output() {
        match stream.write(server.output()) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => {
                server.consume_output(n);
                written += n;
            }
            Err(e) if retry_later(&e) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(written)
}

/// The socket has nothing for now, or the call was interrupted: try again.
fn retry_later(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
}

/// The simulated serial port, with a loopback plug: every byte written to it
/// is received back. Its transmit and receive buffers are the same queue.
///
/// The queue is unbounded: a client that sends without reading makes it
/// grow. A real bridge would stop reading from the client while the UART's
/// transmit buffer is full, and let TCP push back.
#[derive(Default)]
struct LoopbackUart {
    looped: VecDeque<u8>,
}

impl LoopbackUart {
    fn write(&mut self, bytes: &[u8]) {
        self.looped.extend(bytes);
    }

    /// Bytes received from the line and not yet sent to the client.
    fn received(&mut self) -> &[u8] {
        self.looped.make_contiguous()
    }

    /// The first `n` received bytes were sent to the client.
    fn consume(&mut self, n: usize) {
        self.looped.drain(..n);
    }

    fn reconfigure(&mut self, settings: PortSettings) {
        eprintln!(
            "uart: {} baud, {:?} data bits, parity {:?}, {:?} stop bits, flow control {:?}",
            settings.baud_rate,
            settings.data_bits,
            settings.parity,
            settings.stop_bits,
            settings.flow_control
        );
    }

    fn purge(&mut self, purge: Purge) {
        eprintln!(
            "uart: purge {purge:?}: {} bytes discarded",
            self.looped.len()
        );
        self.looped.clear();
    }

    fn set_break(&mut self, on: bool) {
        eprintln!("uart: break {}", on_off(on));
    }

    fn set_dtr(&mut self, on: bool) {
        eprintln!("uart: DTR {}", on_off(on));
    }

    fn set_rts(&mut self, on: bool) {
        eprintln!("uart: RTS {}", on_off(on));
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}
