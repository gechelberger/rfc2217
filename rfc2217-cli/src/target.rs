//! Where to connect: `host[:port]`, `[v6addr][:port]` or
//! `rfc2217://host[:port]`, and the TCP connection itself.

use std::fmt;
use std::io;
use std::net::{Ipv6Addr, TcpStream, ToSocketAddrs};
use std::str::FromStr;
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};

/// The port RFC 2217 bridges conventionally listen on.
pub(crate) const DEFAULT_PORT: u16 = 2217;

/// How long one connection attempt (one resolved address) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A bridge that stops answering is noticed after about a minute: 30 s idle,
/// then 3 probes 10 s apart.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(not(windows))]
const KEEPALIVE_RETRIES: u32 = 3;

/// A host and port, resolved when connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    host: String,
    port: u16,
}

impl FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let rest = match s.split_once("://") {
            // URL schemes are case-insensitive (pyserial writes them in
            // lower case).
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("rfc2217") => rest,
            Some((scheme, _)) => {
                return Err(format!(
                    "`{scheme}://` is not supported: give host:port or rfc2217://host:port"
                ));
            }
            None => s,
        };
        if let Some((_, options)) = rest.split_once('?') {
            return Err(format!("URL options (`?{options}`) are not supported"));
        }
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        // [v6addr] or [v6addr]:port
        if let Some(bracketed) = rest.strip_prefix('[') {
            let (host, after) = bracketed
                .split_once(']')
                .ok_or("missing `]` after an IPv6 address")?;
            let port = match after {
                "" => DEFAULT_PORT,
                _ => parse_port(
                    after
                        .strip_prefix(':')
                        .ok_or("expected `:port` after `]`")?,
                )?,
            };
            return Self::new(host, port);
        }
        // A bare IPv6 address has two or more colons and no port; anything
        // else with that many is a mistake, not an address to look up.
        if rest.matches(':').count() > 1 {
            let address = rest
                .split_once('%')
                .map_or(rest, |(address, _zone)| address);
            if address.parse::<Ipv6Addr>().is_err() {
                return Err(format!(
                    "`{rest}` is not an IPv6 address; with a port, put the address in brackets: \
                     [address]:port"
                ));
            }
            return Self::new(rest, DEFAULT_PORT);
        }
        match rest.rsplit_once(':') {
            Some((host, port)) => Self::new(host, parse_port(port)?),
            None => Self::new(rest, DEFAULT_PORT),
        }
    }
}

fn parse_port(s: &str) -> Result<u16, String> {
    match s.parse() {
        Ok(0) | Err(_) => Err(format!("`{s}` is not a TCP port")),
        Ok(port) => Ok(port),
    }
}

impl Target {
    fn new(host: &str, port: u16) -> Result<Self, String> {
        if host.is_empty() {
            return Err("missing host".into());
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    /// Connects to the first resolved address that answers, with
    /// `TCP_NODELAY` and keepalive set.
    pub(crate) fn connect(&self) -> io::Result<TcpStream> {
        let mut last = None;
        for addr in (self.host.as_str(), self.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    configure(&stream)?;
                    return Ok(stream);
                }
                Err(e) => {
                    tracing::debug!(target: crate::ui::LOG, "{addr}: {e}");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the host name resolved to no address",
            )
        }))
    }
}

fn configure(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    #[cfg(not(windows))]
    let keepalive = keepalive.with_retries(KEEPALIVE_RETRIES);
    SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(s: &str) -> (String, u16) {
        let t: Target = s.parse().unwrap();
        (t.host, t.port)
    }

    #[test]
    fn forms() {
        assert_eq!(target("bridge"), ("bridge".into(), 2217));
        assert_eq!(target("bridge:4001"), ("bridge".into(), 4001));
        assert_eq!(target("192.168.1.50"), ("192.168.1.50".into(), 2217));
        assert_eq!(target("[::1]:4001"), ("::1".into(), 4001));
        assert_eq!(target("[::1]"), ("::1".into(), 2217));
        assert_eq!(target("fe80::1"), ("fe80::1".into(), 2217));
        assert_eq!(target("rfc2217://bridge:4001/"), ("bridge".into(), 4001));
        assert_eq!(target("RFC2217://bridge:4001"), ("bridge".into(), 4001));
        assert_eq!(target("fe80::1%eth0"), ("fe80::1%eth0".into(), 2217));
    }

    #[test]
    fn rejected() {
        for s in [
            "",
            ":2217",
            "bridge:",
            "bridge:0",
            "bridge:http",
            "[::1",
            "[::1]x",
            "rfc2217://h?logging=debug",
            "telnet://bridge:23",
            "socket://bridge:4001",
            "bridge::4001",
            "fe80::1:4001:x",
        ] {
            assert!(s.parse::<Target>().is_err(), "{s}");
        }
    }

    #[test]
    fn display() {
        assert_eq!("[::1]".parse::<Target>().unwrap().to_string(), "[::1]:2217");
        assert_eq!(
            "bridge".parse::<Target>().unwrap().to_string(),
            "bridge:2217"
        );
    }
}
