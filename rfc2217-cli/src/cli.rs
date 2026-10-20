//! The command line: what each subcommand accepts, and how the line settings
//! map onto the library's [`PortSettings`].

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use rfc2217::{DataBits, FlowControl, Parity, PortSettings, StopBits};

use crate::target::Target;

/// RFC 2217 (Telnet COM Port Control) serial ports on the command line.
#[derive(Debug, Parser)]
#[command(name = "rfc2217", version, propagate_version = true)]
pub(crate) struct Cli {
    /// More diagnostics on stderr: -v the bridge's events, -vv protocol
    /// debug logs, -vvv a trace of every exchange.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub(crate) verbose: u8,
    /// Only errors on stderr.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub(crate) quiet: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Open a terminal session to a remote serial port. With a terminal on
    /// stdin it is interactive (Ctrl-] then ? for the command keys); with a
    /// pipe or file it copies the bytes both ways, unchanged.
    Connect(ConnectArgs),
    /// Expose a remote serial port as a local pseudo-terminal (Linux and
    /// macOS). Its path is printed on stdout once the bridge has answered.
    Pty(PtyArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ConnectArgs {
    /// host[:port], [v6addr][:port] or rfc2217://host[:port] (port 2217 by
    /// default).
    pub(crate) target: Target,
    #[command(flatten)]
    pub(crate) line: LineArgs,
    #[command(flatten)]
    pub(crate) session: SessionArgs,
    /// The escape key for the command keys, as ^X, or `none`. Only read from
    /// a terminal.
    #[arg(long, value_name = "^X|none", default_value = "^]", value_parser = parse_escape)]
    pub(crate) escape: Escape,
    /// Interactive only: show typed characters locally.
    #[arg(long)]
    pub(crate) echo: bool,
    /// Interactive only: what the Enter key sends.
    #[arg(long, value_name = "KEY", default_value = "cr")]
    pub(crate) enter: Enter,
    /// Interactive only: show each received LF as CR LF.
    #[arg(long)]
    pub(crate) lf_crlf: bool,
    /// After the end of piped input, keep copying the port's output until it
    /// has been quiet this long (seconds).
    #[arg(long, value_name = "SECS", default_value = "1", value_parser = parse_secs)]
    pub(crate) linger: Duration,
}

#[derive(Debug, Args)]
pub(crate) struct PtyArgs {
    /// host[:port], [v6addr][:port] or rfc2217://host[:port] (port 2217 by
    /// default).
    pub(crate) target: Target,
    /// Also make the pty reachable at this path, a symlink removed on exit.
    #[arg(long, value_name = "PATH")]
    pub(crate) link: Option<PathBuf>,
    #[command(flatten)]
    pub(crate) line: LineArgs,
    #[command(flatten)]
    pub(crate) session: SessionArgs,
}

/// How the session starts and when negotiation counts as failed.
#[derive(Debug, Args)]
pub(crate) struct SessionArgs {
    /// Give up on the bridge's answers after this many seconds.
    #[arg(long, value_name = "SECS", default_value = "3", value_parser = parse_secs)]
    pub(crate) timeout: Duration,
    /// Fail unless the bridge confirms every setting with the requested value.
    #[arg(long)]
    pub(crate) strict: bool,
    /// Carry on as a plain Telnet session when the server refuses RFC 2217.
    #[arg(long)]
    pub(crate) allow_telnet: bool,
    /// Set DTR once connected.
    #[arg(long, value_name = "on|off")]
    pub(crate) dtr: Option<OnOff>,
    /// Set RTS once connected.
    #[arg(long, value_name = "on|off")]
    pub(crate) rts: Option<OnOff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OnOff {
    #[value(alias = "1")]
    On,
    #[value(alias = "0")]
    Off,
}

impl OnOff {
    pub(crate) fn is_on(self) -> bool {
        self == Self::On
    }
}

/// The five line settings. The library always sends all five, so the ones
/// not given are filled in by [`LineArgs::settings`].
#[derive(Debug, Args)]
pub(crate) struct LineArgs {
    /// Baud rate, or `keep` to leave the bridge's rate as it is.
    #[arg(short, long, value_name = "RATE|keep", value_parser = parse_baud)]
    pub(crate) baud: Option<u32>,
    /// Data bits (default 8).
    #[arg(short, long, visible_alias = "databits", value_name = "5-8")]
    pub(crate) data_bits: Option<DataBitsArg>,
    /// Parity (default none).
    #[arg(long, ignore_case = true)]
    pub(crate) parity: Option<ParityArg>,
    /// Stop bits (default 1). 1.5 goes with 5 data bits on most UARTs.
    #[arg(long, visible_alias = "stopbits")]
    pub(crate) stop_bits: Option<StopBitsArg>,
    /// Flow control (default none). Many bridges do not answer dcd or dsr.
    #[arg(short, long, ignore_case = true)]
    pub(crate) flow: Option<FlowArg>,
    /// Data bits, parity and stop bits in one, e.g. 8N1, 7E1 or 5N1.5.
    #[arg(long, value_name = "8N1", conflicts_with_all = ["data_bits", "parity", "stop_bits"])]
    pub(crate) frame: Option<Frame>,
}

impl LineArgs {
    /// What `Client::new` gets: the baud rate defaults to `keep` (the RFC 2217
    /// query), everything else to 8N1 without flow control.
    pub(crate) fn settings(&self) -> PortSettings {
        let d = PortSettings::default();
        let (data_bits, parity, stop_bits) = match self.frame {
            Some(Frame(data_bits, parity, stop_bits)) => (data_bits, parity, stop_bits),
            None => (
                self.data_bits.map_or(d.data_bits, Into::into),
                self.parity.map_or(d.parity, Into::into),
                self.stop_bits.map_or(d.stop_bits, Into::into),
            ),
        };
        PortSettings {
            baud_rate: self.baud.unwrap_or(0),
            data_bits,
            parity,
            stop_bits,
            flow_control: self.flow.map_or(d.flow_control, Into::into),
        }
    }

    /// Whether any of the four settings that cannot be kept was given. When
    /// none was, the session says once that it sends 8N1 without flow control.
    pub(crate) fn framing_given(&self) -> bool {
        self.frame.is_some()
            || self.data_bits.is_some()
            || self.parity.is_some()
            || self.stop_bits.is_some()
            || self.flow.is_some()
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum DataBitsArg {
    #[value(name = "5")]
    Five,
    #[value(name = "6")]
    Six,
    #[value(name = "7")]
    Seven,
    #[value(name = "8")]
    Eight,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ParityArg {
    #[value(alias = "n")]
    None,
    #[value(alias = "o")]
    Odd,
    #[value(alias = "e")]
    Even,
    #[value(alias = "m")]
    Mark,
    #[value(alias = "s")]
    Space,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum StopBitsArg {
    #[value(name = "1")]
    One,
    #[value(name = "1.5")]
    OnePointFive,
    #[value(name = "2")]
    Two,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum FlowArg {
    #[value(alias = "n")]
    None,
    #[value(aliases = ["soft", "x"])]
    Xonxoff,
    #[value(aliases = ["hard", "h"])]
    Rtscts,
    Dcd,
    Dsr,
}

impl From<DataBitsArg> for DataBits {
    fn from(v: DataBitsArg) -> Self {
        match v {
            DataBitsArg::Five => Self::Five,
            DataBitsArg::Six => Self::Six,
            DataBitsArg::Seven => Self::Seven,
            DataBitsArg::Eight => Self::Eight,
        }
    }
}

impl From<ParityArg> for Parity {
    fn from(v: ParityArg) -> Self {
        match v {
            ParityArg::None => Self::None,
            ParityArg::Odd => Self::Odd,
            ParityArg::Even => Self::Even,
            ParityArg::Mark => Self::Mark,
            ParityArg::Space => Self::Space,
        }
    }
}

impl From<StopBitsArg> for StopBits {
    fn from(v: StopBitsArg) -> Self {
        match v {
            StopBitsArg::One => Self::One,
            StopBitsArg::OnePointFive => Self::OnePointFive,
            StopBitsArg::Two => Self::Two,
        }
    }
}

impl From<FlowArg> for FlowControl {
    fn from(v: FlowArg) -> Self {
        match v {
            FlowArg::None => Self::None,
            FlowArg::Xonxoff => Self::XonXoff,
            FlowArg::Rtscts => Self::RtsCts,
            FlowArg::Dcd => Self::Dcd,
            FlowArg::Dsr => Self::Dsr,
        }
    }
}

/// `--frame`: data bits, a parity letter and stop bits, as in 8N1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame(DataBits, Parity, StopBits);

impl FromStr for Frame {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.to_ascii_uppercase();
        let mut chars = s.chars();
        let data_bits = match chars.next() {
            Some('5') => DataBits::Five,
            Some('6') => DataBits::Six,
            Some('7') => DataBits::Seven,
            Some('8') => DataBits::Eight,
            _ => return Err("expected data bits 5-8 first, as in 8N1".into()),
        };
        let parity = match chars.next() {
            Some('N') => Parity::None,
            Some('O') => Parity::Odd,
            Some('E') => Parity::Even,
            Some('M') => Parity::Mark,
            Some('S') => Parity::Space,
            _ => return Err("expected a parity letter (N, O, E, M or S) second, as in 8N1".into()),
        };
        let stop_bits = match chars.as_str() {
            "1" => StopBits::One,
            "1.5" => StopBits::OnePointFive,
            "2" => StopBits::Two,
            _ => return Err("expected stop bits 1, 1.5 or 2 last, as in 8N1".into()),
        };
        Ok(Self(data_bits, parity, stop_bits))
    }
}

/// `--enter`: the bytes the Enter key (CR in raw mode) sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Enter {
    /// CR, as typed.
    Cr,
    /// LF instead of CR.
    Lf,
    /// CR LF.
    Crlf,
}

impl Enter {
    // Only the interactive terminal, which is Unix-only, reads keys.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Cr => b"\r",
            Self::Lf => b"\n",
            Self::Crlf => b"\r\n",
        }
    }
}

/// `--escape`: a control character, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Escape(pub(crate) Option<u8>);

impl Escape {
    /// How the key is written in messages, e.g. `^]`.
    // Only the interactive terminal, which is Unix-only, shows it.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn caret(byte: u8) -> String {
        format!("^{}", char::from(byte ^ 0x40))
    }
}

fn parse_escape(s: &str) -> Result<Escape, String> {
    match s.as_bytes() {
        b"none" => Ok(Escape(None)),
        [b'^', c @ (b'@'..=b'_' | b'a'..=b'z')] => Ok(Escape(Some(c.to_ascii_uppercase() ^ 0x40))),
        _ => Err("expected ^ and a key from @ A-Z [ \\ ] ^ _ (e.g. ^] or ^A), or `none`".into()),
    }
}

fn parse_baud(s: &str) -> Result<u32, String> {
    match s {
        "keep" => Ok(0),
        _ => match s.parse::<u32>() {
            Ok(0) => {
                Err("0 is not a baud rate; use `keep` to leave the bridge's rate as it is".into())
            }
            Ok(rate) => Ok(rate),
            Err(e) => Err(format!("{e}; expected a baud rate or `keep`")),
        },
    }
}

fn parse_secs(s: &str) -> Result<Duration, String> {
    s.parse::<f64>()
        .ok()
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        // Not `from_secs_f64`, which panics beyond u64::MAX seconds.
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .ok_or_else(|| format!("`{s}` is not a number of seconds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("rfc2217").chain(args.iter().copied()))
    }

    fn connect_settings(args: &[&str]) -> PortSettings {
        match parse(&[&["connect", "host"], args].concat())
            .unwrap()
            .command
        {
            Command::Connect(c) => c.line.settings(),
            Command::Pty(_) => unreachable!(),
        }
    }

    #[test]
    fn verify_cli() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_keep_the_baud_rate_and_send_8n1() {
        let s = connect_settings(&[]);
        assert_eq!(
            s,
            PortSettings {
                baud_rate: 0,
                ..PortSettings::default()
            }
        );
    }

    #[test]
    fn line_settings() {
        let s = connect_settings(&[
            "-b",
            "115200",
            "--databits",
            "7",
            "--parity",
            "E",
            "--stopbits",
            "2",
            "-f",
            "hard",
        ]);
        assert_eq!(
            s,
            PortSettings {
                baud_rate: 115_200,
                data_bits: DataBits::Seven,
                parity: Parity::Even,
                stop_bits: StopBits::Two,
                flow_control: FlowControl::RtsCts,
            }
        );
        let s = connect_settings(&["--frame", "5n1.5"]);
        assert_eq!(
            (s.data_bits, s.parity, s.stop_bits),
            (DataBits::Five, Parity::None, StopBits::OnePointFive)
        );
    }

    #[test]
    fn rejected_values() {
        assert!(parse(&["connect", "host", "-b", "0"]).is_err());
        assert!(parse(&["connect", "host", "--frame", "8N1", "--parity", "odd"]).is_err());
        assert!(parse(&["connect", "host", "--frame", "9N1"]).is_err());
        assert!(parse(&["connect", "host", "--escape", "x"]).is_err());
        assert!(parse(&["connect", "host", "--timeout", "-1"]).is_err());
    }

    #[test]
    fn escape_keys() {
        assert_eq!(parse_escape("^]"), Ok(Escape(Some(0x1d))));
        assert_eq!(parse_escape("^a"), Ok(Escape(Some(0x01))));
        assert_eq!(parse_escape("none"), Ok(Escape(None)));
        assert_eq!(Escape::caret(0x1d), "^]");
    }
}
