//! A blocking RFC 2217 client on the alloc layer: the README's quick start as
//! a command-line tool. It connects, configures the remote port, prints what
//! the bridge reports, sends a line of text and prints whatever comes back
//! for two seconds. Serial data goes to stdout, everything else to stderr.
//!
//! Usage: `blocking_client <host:port> [baud] [text]`
//!
//! Against the simulated bridge (`examples/sim_server.rs`):
//!
//! ```text
//! cargo run --example sim_server
//! cargo run --example blocking_client -- 127.0.0.1:2217 115200 hello
//! ```

use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rfc2217::{Client, NotReady, PortSettings};

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
            eprintln!("blocking_client: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: blocking_client <host:port> [baud] [text]");
    ExitCode::from(2)
}

fn run(addr: &str, baud_rate: u32, text: Option<&str>) -> io::Result<()> {
    let addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no address to connect to"))?;
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    sock.set_nodelay(true)?;
    let mut client = Client::new(PortSettings {
        baud_rate,
        ..PortSettings::default()
    });
    let mut buf = [0u8; 4096];

    // Negotiate: fail fast when waiting cannot help, and report why at the
    // deadline.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        sock.write_all(&client.take_output())?;
        match client.readiness() {
            Ok(()) => break,
            Err(e @ (NotReady::Refused | NotReady::Mismatched(_))) => {
                return Err(io::Error::other(e));
            }
            Err(e) if Instant::now() >= deadline => {
                return Err(io::Error::new(ErrorKind::TimedOut, e));
            }
            Err(_) => {}
        }
        if !receive(&mut sock, &mut client, &mut buf)? {
            return Err(ErrorKind::UnexpectedEof.into());
        }
    }
    eprintln!("ready: {:?}", client.reported_settings());

    if let Some(text) = text {
        client.write_data(text.as_bytes());
        client.write_data(b"\r\n");
    }
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        sock.write_all(&client.take_output())?;
        if !receive(&mut sock, &mut client, &mut buf)? {
            eprintln!("the bridge closed the connection");
            break;
        }
    }
    sock.write_all(&client.take_output())?;
    Ok(())
}

/// One read (or read timeout): serial data to stdout, events to stderr.
/// Returns `false` at the end of the stream.
fn receive(sock: &mut TcpStream, client: &mut Client, buf: &mut [u8]) -> io::Result<bool> {
    match sock.read(buf) {
        Ok(0) => return Ok(false),
        Ok(n) => {
            // Payload may arrive while negotiating, too.
            let data = client.receive(&buf[..n]);
            let mut stdout = io::stdout().lock();
            stdout.write_all(&data)?;
            stdout.flush()?;
        }
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        Err(e) => return Err(e),
    }
    while let Some(event) = client.poll_event() {
        eprintln!("event: {event:?}");
    }
    Ok(true)
}
