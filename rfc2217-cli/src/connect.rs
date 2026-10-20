//! `rfc2217 connect`: a terminal session to a remote serial port.
//!
//! With a terminal on stdin the session is interactive (Unix only for now):
//! the terminal is in raw mode, every key goes to the port as typed, and an
//! escape key leads to the command keys. Otherwise stdin is a pipe or a
//! file, and the session copies the bytes both ways unchanged (0xFF
//! included: the library escapes IAC). At the end of piped input it keeps
//! copying the port's output until that has been quiet for `--linger`,
//! instead of closing at once: bridges close the connection when they read
//! its end, and the device's reply would be lost (finding cn-7).

use std::io::{self, IsTerminal};
use std::time::Duration;

use crate::Exit;
use crate::cli::ConnectArgs;
use crate::session::{self, InputEnd, Local, LocalIo, Setup};
use crate::ui;

pub(crate) fn run(args: ConnectArgs) -> Exit {
    let interactive = io::stdin().is_terminal();
    // Windows consoles strip Ctrl-Z from reads and reject output that is
    // not UTF-8 (finding cn-6): a terminal session needs its own console
    // code there.
    #[cfg(not(unix))]
    if interactive {
        ui::error(
            "interactive sessions are not supported on Windows yet; pipe the input in instead \
             (e.g. `type commands.txt | rfc2217 connect HOST`)",
        );
        return Exit::Usage;
    }
    let stream = match args.target.connect() {
        Ok(stream) => stream,
        Err(e) => {
            ui::error(format_args!("cannot connect to {}: {e}", args.target));
            return Exit::Connect;
        }
    };
    let setup = Setup {
        target: &args.target,
        line: &args.line,
        args: &args.session,
    };
    #[cfg(unix)]
    if interactive {
        return terminal::run(&args, setup, stream);
    }
    ui::verbose(format_args!("connected to {}", args.target));
    let io = LocalIo {
        reader: Box::new(io::stdin()),
        writer: Box::new(io::stdout()),
        input: "standard input",
        output: "standard output",
        throttle: true,
        drain: None,
    };
    session::run(
        setup,
        stream,
        io,
        &mut Pipe {
            linger: args.linger,
        },
    )
}

/// A pipe or a file on stdin: byte-transparent both ways.
#[derive(Debug)]
struct Pipe {
    linger: Duration,
}

impl Local for Pipe {
    fn input_ended(&mut self) -> InputEnd {
        InputEnd::Linger(self.linger)
    }

    fn remote_closed(&self) -> Exit {
        Exit::Ok
    }
}

/// An interactive session on stdin's terminal.
#[cfg(unix)]
mod terminal {
    use std::io;
    use std::mem;
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    use rfc2217::{Client, ComPortState, ModemState, Purge};

    use crate::Exit;
    use crate::cli::{ConnectArgs, Enter, Escape, OnOff};
    use crate::session::{self, Ctl, InputEnd, Line, Local, LocalIo, Setup};
    use crate::term::RawMode;
    use crate::ui;

    /// How long `b` holds the BREAK. RFC 2217 has no duration: the CLI
    /// times the BREAK-OFF itself (finding cn-9).
    const BREAK: Duration = Duration::from_millis(250);

    /// The rates `u` and `n` step through.
    const RATES: [u32; 13] = [
        300, 600, 1200, 2400, 4800, 9600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800,
        921_600,
    ];

    pub(super) fn run(args: &ConnectArgs, setup: Setup<'_>, stream: TcpStream) -> Exit {
        let raw = match RawMode::enter() {
            Ok(raw) => raw,
            Err(e) => {
                ui::error(format_args!("cannot put the terminal into raw mode: {e}"));
                return Exit::Io;
            }
        };
        let target = setup.target;
        match args.escape.0 {
            Some(key) => ui::notice(format_args!(
                "connected to {target}; the escape key is {}, then ? for help",
                Escape::caret(key)
            )),
            None => ui::notice(format_args!(
                "connected to {target}; no escape key (--escape none)"
            )),
        }
        let mut terminal = Terminal {
            raw,
            escape: args.escape.0,
            enter: args.enter,
            echo: args.echo,
            lf_crlf: args.lf_crlf,
            command: false,
            dtr: setup.args.dtr.map(OnOff::is_on),
            rts: setup.args.rts.map(OnOff::is_on),
            break_off: None,
        };
        let io = LocalIo {
            reader: Box::new(io::stdin()),
            writer: Box::new(io::stdout()),
            input: "the terminal",
            output: "standard output",
            throttle: false,
            drain: None,
        };
        // `terminal` owns the raw mode: dropping it on the way out restores
        // the terminal (and so does the cleanup registry, on every other way).
        session::run(setup, stream, io, &mut terminal)
    }

    #[derive(Debug)]
    struct Terminal {
        raw: RawMode,
        escape: Option<u8>,
        enter: Enter,
        echo: bool,
        lf_crlf: bool,
        /// The escape key was typed: the next key is a command.
        command: bool,
        /// DTR and RTS as last set here, `None` before that: RFC 2217 0.1
        /// cannot ask the bridge, so the toggles start from "on", the state
        /// most bridges open the port with.
        dtr: Option<bool>,
        rts: Option<bool>,
        /// When the BREAK that `b` started ends.
        break_off: Option<Instant>,
    }

    impl Local for Terminal {
        fn input(&mut self, bytes: &[u8], ctl: &mut Ctl) -> Option<Exit> {
            let (mut data, mut echo) = (Vec::with_capacity(bytes.len()), Vec::new());
            for &byte in bytes {
                if mem::take(&mut self.command) {
                    if Some(byte) != self.escape {
                        // A command acts after the keys typed before it.
                        self.forward(&mut data, &mut echo, ctl);
                        if let Some(exit) = self.command(byte, ctl) {
                            return Some(exit);
                        }
                        continue;
                    }
                    // The escape key twice: it goes to the port once.
                } else if Some(byte) == self.escape {
                    self.command = true;
                    continue;
                }
                if byte == b'\r' {
                    data.extend_from_slice(self.enter.bytes());
                } else {
                    data.push(byte);
                }
                if self.echo {
                    // Enter shown as a new line, whatever it sends.
                    match byte {
                        b'\r' => echo.extend_from_slice(b"\r\n"),
                        _ => echo.push(byte),
                    }
                }
            }
            self.forward(&mut data, &mut echo, ctl);
            None
        }

        /// The terminal hung up (a read in raw mode returns 0 only then):
        /// nobody is left to see a linger.
        fn input_ended(&mut self) -> InputEnd {
            InputEnd::Exit(Exit::Ok)
        }

        fn output(&mut self, payload: Vec<u8>) -> Vec<u8> {
            ui::shown(&payload);
            if !self.lf_crlf {
                return payload;
            }
            let mut shown = Vec::with_capacity(payload.len() + payload.len() / 8);
            for byte in payload {
                if byte == b'\n' {
                    shown.push(b'\r');
                }
                shown.push(byte);
            }
            shown
        }

        fn settled(&mut self, ctl: &mut Ctl) {
            let client = ctl.client();
            if client.is_ready() {
                ui::notice(format_args!(
                    "port settings: {}",
                    Line::from(client.reported_settings())
                ));
            }
        }

        fn next_tick(&self) -> Option<Instant> {
            self.break_off
        }

        fn tick(&mut self, now: Instant, ctl: &mut Ctl) -> Option<Exit> {
            if self.break_off.is_some_and(|at| at <= now) {
                self.end_break(ctl);
            }
            None
        }

        fn remote_closed(&self) -> Exit {
            Exit::Ok
        }
    }

    impl Terminal {
        fn forward(&mut self, data: &mut Vec<u8>, echo: &mut Vec<u8>, ctl: &mut Ctl) {
            ui::shown(echo);
            ctl.show(mem::take(echo));
            ctl.send(data);
            data.clear();
        }

        /// The key after the escape key.
        fn command(&mut self, key: u8, ctl: &mut Ctl) -> Option<Exit> {
            match key {
                b'q' | b'.' => {
                    // Never leave the remote line in BREAK.
                    if self.break_off.is_some() {
                        self.end_break(ctl);
                    }
                    ui::notice("closing the connection");
                    return Some(Exit::Ok);
                }
                b'b' => self.start_break(ctl),
                b'd' => {
                    let on = !self.dtr.unwrap_or(true);
                    if usable(ctl.client(), "DTR") {
                        ctl.client().set_dtr(on);
                        self.dtr = Some(on);
                        ui::notice(format_args!("DTR {}", on_off(on)));
                    }
                }
                b'r' => {
                    let on = !self.rts.unwrap_or(true);
                    if usable(ctl.client(), "RTS") {
                        ctl.client().set_rts(on);
                        self.rts = Some(on);
                        ui::notice(format_args!("RTS {}", on_off(on)));
                    }
                }
                b'p' => {
                    if usable(ctl.client(), "purge") {
                        ctl.client().purge(Purge::Both);
                        ui::notice("asked the bridge to purge both buffers");
                    }
                }
                b'u' => step_baud(ctl, Step::Up),
                b'n' => step_baud(ctl, Step::Down),
                b's' => ui::notice(status(ctl.client(), self.dtr, self.rts)),
                b'e' => {
                    self.echo = !self.echo;
                    ui::notice(format_args!("local echo {}", on_off(self.echo)));
                }
                b'z' => return self.suspend(),
                b'?' | b'h' => ui::notice(help(self.escape_name())),
                _ => ui::notice(format_args!(
                    "{} is not a command key ({} ? for help)",
                    key_name(key),
                    self.escape_name()
                )),
            }
            None
        }

        fn escape_name(&self) -> String {
            self.escape.map(Escape::caret).unwrap_or_default()
        }

        fn start_break(&mut self, ctl: &mut Ctl) {
            if self.break_off.is_some() {
                ui::notice("a BREAK is in progress already");
            } else if ctl.client().com_port_state() == ComPortState::Negotiating {
                // The alloc layer would hold the BREAK-ON and the BREAK-OFF
                // timed here, and write them back to back once the bridge
                // answers: a BREAK of no length.
                ui::notice("no BREAK yet: the bridge has not answered the RFC 2217 offer");
            } else if usable(ctl.client(), "BREAK") {
                ctl.client().set_break(true);
                self.break_off = Some(Instant::now() + BREAK);
                ui::notice(format_args!("BREAK for {} ms", BREAK.as_millis()));
            }
        }

        fn end_break(&mut self, ctl: &mut Ctl) {
            self.break_off = None;
            ctl.client().set_break(false);
        }

        /// `z`: the shell gets the terminal back until it continues us.
        fn suspend(&mut self) -> Option<Exit> {
            ui::notice("suspended (fg resumes the session)");
            match self.raw.suspend() {
                Ok(()) => {
                    ui::notice("resumed");
                    None
                }
                // A session on a cooked terminal would echo twice, and
                // Ctrl-C would kill it: end it instead.
                Err(e) => {
                    ui::error(format_args!(
                        "cannot put the terminal back into raw mode: {e}"
                    ));
                    Some(Exit::Io)
                }
            }
        }
    }

    /// Whether a command can reach the bridge: not while it refuses RFC 2217
    /// (the alloc layer would drop the command silently, finding cn-9).
    /// While negotiating, the alloc layer holds it and sends it after the
    /// settings.
    fn usable(client: &Client, what: &str) -> bool {
        let refused = client.com_port_state() == ComPortState::Refused;
        if refused {
            ui::notice(format_args!("no {what}: the bridge does not do RFC 2217"));
        }
        !refused
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        Up,
        Down,
    }

    /// `u` and `n`: the next standard rate above or below the current one,
    /// which is the requested rate, or the reported one for `keep`.
    fn step_baud(ctl: &mut Ctl, step: Step) {
        let client = ctl.client();
        if !usable(client, "baud rate change") {
            return;
        }
        let mut settings = client.requested_settings();
        let current = match settings.baud_rate {
            0 => client.reported_settings().baud_rate,
            rate => Some(rate),
        };
        let Some(current) = current else {
            ui::notice("the bridge has not reported its baud rate yet");
            return;
        };
        let Some(next) = next_rate(current, step) else {
            let end = if step == Step::Up {
                "highest"
            } else {
                "lowest"
            };
            ui::notice(format_args!(
                "{current} baud: there is no standard rate {end} than that"
            ));
            return;
        };
        settings.baud_rate = next;
        ui::notice(format_args!("asking for {next} baud"));
        ctl.set_port_settings(settings);
    }

    fn next_rate(current: u32, step: Step) -> Option<u32> {
        match step {
            Step::Up => RATES.into_iter().find(|&rate| rate > current),
            Step::Down => RATES.into_iter().rev().find(|&rate| rate < current),
        }
    }

    /// `s`: what the client knows, and DTR and RTS as last set here. One
    /// message of several lines, CR LF apart: the terminal is raw.
    fn status(client: &Client, dtr: Option<bool>, rts: Option<bool>) -> String {
        let state = match client.com_port_state() {
            ComPortState::Negotiating => "RFC 2217 negotiating",
            ComPortState::Active => "RFC 2217 active",
            ComPortState::Refused => "RFC 2217 refused (a plain Telnet session)",
        };
        let readiness = match client.readiness() {
            Ok(()) => "every setting confirmed".to_owned(),
            Err(e) => e.to_string(),
        };
        let modem = client.modem_state().map_or_else(
            || "not reported".to_owned(),
            |m| {
                format!(
                    "CD {}, RI {}, DSR {}, CTS {}",
                    on_off(m.contains(ModemState::CD)),
                    on_off(m.contains(ModemState::RI)),
                    on_off(m.contains(ModemState::DSR)),
                    on_off(m.contains(ModemState::CTS)),
                )
            },
        );
        let flow = if client.flow_suspended() {
            "suspended by the bridge (input held)"
        } else {
            "running"
        };
        let set = |line: Option<bool>| line.map_or("not set", on_off);
        format!(
            "{state}: {readiness}\r\n\
             requested: {}\r\n\
             reported: {}\r\n\
             modem lines: {modem}\r\n\
             data flow: {flow}\r\n\
             DTR {}, RTS {} (as last set here: the bridge cannot be asked)",
            Line::from(client.requested_settings()),
            Line::from(client.reported_settings()),
            set(dtr),
            set(rts),
        )
    }

    fn help(escape: String) -> String {
        format!(
            "command keys, typed after {escape}:\r\n  \
             q or .  quit\r\n  \
             b       send a BREAK ({} ms)\r\n  \
             d / r   toggle DTR / RTS\r\n  \
             p       purge the bridge's buffers\r\n  \
             u / n   next higher / lower baud rate\r\n  \
             s       status\r\n  \
             e       local echo on / off\r\n  \
             z       suspend (fg resumes)\r\n  \
             ?       this help\r\n  \
             {escape}      send {escape} itself",
            BREAK.as_millis()
        )
    }

    fn key_name(key: u8) -> String {
        match key {
            b' ' => "space".to_owned(),
            0x21..=0x7E => format!("'{}'", char::from(key)),
            0x00..=0x1F | 0x7F => Escape::caret(key),
            _ => format!("{key:#04x}"),
        }
    }

    fn on_off(on: bool) -> &'static str {
        if on { "on" } else { "off" }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn baud_steps() {
            assert_eq!(next_rate(9600, Step::Up), Some(19_200));
            assert_eq!(next_rate(9600, Step::Down), Some(4800));
            // A rate between two standard ones steps to its neighbours.
            assert_eq!(next_rate(100_000, Step::Up), Some(115_200));
            assert_eq!(next_rate(100_000, Step::Down), Some(57_600));
            assert_eq!(next_rate(921_600, Step::Up), None);
            assert_eq!(next_rate(300, Step::Down), None);
        }

        #[test]
        fn key_names() {
            assert_eq!(key_name(b'x'), "'x'");
            assert_eq!(key_name(0x03), "^C");
            assert_eq!(key_name(0x7F), "^?");
            assert_eq!(key_name(0xE9), "0xe9");
        }
    }
}
