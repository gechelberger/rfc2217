//! The same tool as `blocking_client`, driven through the core API: the
//! README's embedded loop (`Link`) over a `TcpStream` in non-blocking mode.
//! The driving code uses fixed buffers only, no `Vec` and no allocation, so it
//! builds without the `alloc` feature. Serial data goes to stdout, everything
//! else to stderr.
//!
//! Usage: `core_client <host:port> [baud] [text]`
//!
//! Against the simulated bridge (`examples/sim_server.rs`):
//!
//! ```text
//! cargo run --example sim_server
//! cargo run --example core_client -- 127.0.0.1:2217 115200 hello
//! ```

use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rfc2217::{Client, Event, NotReady, PortSettings};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (addr, baud, text) = match args.as_slice() {
        [addr] => (addr, None, None),
        [addr, baud] => (addr, Some(baud), None),
        [addr, baud, text] => (addr, Some(baud), Some(text.as_str())),
        _ => return usage(),
    };
    let baud_rate = match baud.map(|b| b.parse()) {
        None => PortSettings::default().baud_rate,
        Some(Ok(rate)) => rate,
        Some(Err(_)) => return usage(),
    };
    match run(addr, baud_rate, text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("core_client: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: core_client <host:port> [baud] [text]");
    ExitCode::from(2)
}

/// Your stack's non-blocking TCP socket.
trait Socket {
    /// Queue up to `bytes.len()` bytes; how many were taken (0 when the send
    /// buffer is full).
    fn send(&mut self, bytes: &[u8]) -> usize;
    /// Copy what has arrived into `buf`; how many bytes (0 when nothing is
    /// there). 0 also stands for the end of the stream and errors: report
    /// those out of band.
    fn recv(&mut self, buf: &mut [u8]) -> usize;
}

/// One connection: the client plus a 256-byte receive buffer.
struct Link {
    client: Client,
    rx: [u8; 256],
    at: usize,
    end: usize,
}

impl Link {
    fn new(client: Client) -> Self {
        Link {
            client,
            rx: [0; 256],
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

/// A `TcpStream` in non-blocking mode as a [`Socket`]: "nothing now" is 0,
/// and the end of the stream or an error is kept for the caller to check.
struct StdSocket {
    stream: TcpStream,
    closed: Option<io::Error>,
}

impl StdSocket {
    /// The end of the stream or the error that stopped the socket, if any.
    fn check(&mut self) -> io::Result<()> {
        self.closed.take().map_or(Ok(()), Err)
    }

    fn fail(&mut self, error: io::Error) -> usize {
        self.closed.get_or_insert(error);
        0
    }
}

impl Socket for StdSocket {
    fn send(&mut self, bytes: &[u8]) -> usize {
        match self.stream.write(bytes) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => 0,
            Err(e) => self.fail(e),
        }
    }

    fn recv(&mut self, buf: &mut [u8]) -> usize {
        match self.stream.read(buf) {
            Ok(0) => self.fail(ErrorKind::UnexpectedEof.into()),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => 0,
            Err(e) => self.fail(e),
        }
    }
}

/// Serial data from the bridge: straight to stdout.
fn uart(bytes: &[u8]) {
    let mut stdout = io::stdout().lock();
    // A closed stdout is not this example's concern.
    let _ = stdout.write_all(bytes).and_then(|()| stdout.flush());
}

fn run(addr: &str, baud_rate: u32, text: Option<&str>) -> io::Result<()> {
    let addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no address to connect to"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    let mut sock = StdSocket {
        stream,
        closed: None,
    };
    let mut link = Link::new(Client::new(PortSettings {
        baud_rate,
        ..PortSettings::default()
    }));
    let mut on_event = |event: Event| eprintln!("event: {event:?}");
    let pause = || std::thread::sleep(Duration::from_millis(1));

    // Negotiate: fail fast when waiting cannot help, and report why at the
    // deadline.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        link.poll(&mut sock, &mut &[][..], &mut uart, &mut on_event);
        sock.check()?;
        match link.client.readiness() {
            Ok(()) => break,
            Err(e @ (NotReady::Refused | NotReady::Mismatched(_))) => {
                return Err(io::Error::other(e));
            }
            Err(e) if Instant::now() >= deadline => {
                return Err(io::Error::new(ErrorKind::TimedOut, e));
            }
            Err(_) => pause(),
        }
    }
    eprintln!("ready: {:?}", link.client.reported_settings());

    // Ready: send the text, then the line end, and print what arrives
    // meanwhile. (The Link sends whatever `tx` holds once COM-PORT-OPTION has
    // stopped negotiating; passing data only now is this tool's policy.)
    let mut lines: [&[u8]; 2] = match text {
        Some(text) => [text.as_bytes(), b"\r\n"],
        None => [b"", b""],
    };
    let mut idle: &[u8] = &[];
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        let tx = lines.iter_mut().find(|part| !part.is_empty());
        link.poll(&mut sock, tx.unwrap_or(&mut idle), &mut uart, &mut on_event);
        match sock.check() {
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                eprintln!("the bridge closed the connection");
                return Ok(());
            }
            result => result?,
        }
        pause();
    }
    Ok(())
}
