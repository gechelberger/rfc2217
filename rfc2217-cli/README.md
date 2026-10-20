# rfc2217-cli

[RFC 2217](https://www.rfc-editor.org/rfc/rfc2217) (Telnet COM Port Control)
on the command line. Installs one binary, `rfc2217`, built on the
[`rfc2217`](https://crates.io/crates/rfc2217) library crate.

Serial-to-Ethernet bridges such as ser2net, Moxa NPort, Lantronix and Digi
usually offer a serial port in two ways: a raw TCP port that carries only
bytes, and an RFC 2217 Telnet port that also lets the client set the baud rate,
data bits, parity, stop bits and flow control on the far side, and drive DTR,
RTS and BREAK. `rfc2217` talks to the second kind:

- **`rfc2217 connect`** is a serial terminal for the remote port: interactive
  when stdin is a terminal, with keys for BREAK, DTR, RTS, purge, baud rate and
  status, and a byte-exact pipe when it is not.
- **`rfc2217 pty`** makes the remote port a local pseudo-terminal, for
  programs that only take a device path. A pty is not a serial port: read
  [what it cannot carry](#what-a-pty-cannot-carry) before relying on it.

Both check what the bridge did with the request: they fail (exit code 4) when
it does not speak RFC 2217, and say which settings it did not confirm, failing
on that too with `--strict`. The library's client is tested against ser2net
4.6.4 and 4.3.11, sredird 2.2.2 and pyserial 3.5.

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| `connect`, interactive | yes | yes | not yet |
| `connect`, piped or redirected input | yes | yes | yes |
| `pty` | yes | yes | no |

## Install

```sh
cargo install --locked rfc2217-cli
```

`--locked` builds with the dependency versions this release was built and
tested with (the package ships its `Cargo.lock`). It needs Rust 1.85 or newer.
The command is `rfc2217`; `cargo uninstall rfc2217-cli` removes it. There are
no prebuilt binaries yet.

## Quick start

```sh
# The bridge's current baud rate, 8N1, no flow control. Ctrl-] then q quits.
rfc2217 connect 192.168.1.50

# 115200 baud on port 4001; 9600 baud 7E1; RTS/CTS flow control.
rfc2217 connect bridge.local:4001 -b 115200
rfc2217 connect bridge.local -b 9600 --frame 7E1
rfc2217 connect rfc2217://bridge.local:2217 -b 115200 --flow rtscts

# A script: send one command, keep the reply, fail unless 9600 8N1 was confirmed.
printf '*IDN?\r\n' | rfc2217 connect -q bridge.local -b 9600 --strict > reply.txt

# A device path for a program that needs one (two terminals).
rfc2217 pty bridge.local -b 115200 --link ~/ttyRFC0
picocom -b 115200 ~/ttyRFC0
```

The target is `host`, `host:port`, `[v6addr]:port`, a bare IPv6 address, or
pyserial's `rfc2217://host:port` (without `?options`). The port defaults
to 2217. Every address the host name resolves to is tried in turn, for up to
5 s each. TCP keepalive is on, so a bridge that disappears is noticed (after
about a minute of silence on Linux and macOS).

## Line settings

The session starts by sending the serial port's settings to the bridge:

| Option | Values | Default |
| --- | --- | --- |
| `-b`, `--baud` | a rate, or `keep` | `keep` |
| `-d`, `--data-bits` (or `--databits`) | `5`, `6`, `7`, `8` | `8` |
| `--parity` | `none`, `odd`, `even`, `mark`, `space` (or `n`, `o`, `e`, `m`, `s`; any case) | `none` |
| `--stop-bits` (or `--stopbits`) | `1`, `1.5`, `2` | `1` |
| `-f`, `--flow` | `none`, `xonxoff`, `rtscts`, `dcd`, `dsr` (or `n`; `x`, `soft`; `h`, `hard`) | `none` |
| `--frame` | data bits, parity and stop bits in one: `8N1`, `7E1`, `5N1.5` | |

- **The baud rate is kept unless you give one.** `keep` sends the RFC 2217
  query: the bridge leaves its rate as it is and reports it.
- **The other four are always sent**, 8N1 without flow control unless given.
  RFC 2217 could ask the bridge for them too, but the library can query only
  the baud rate so far, so there is no `keep` for them. When you give none of
  the four, `rfc2217` says once that it sends 8N1 without flow control. A
  device on 7E1 needs `--frame 7E1` (or `-d 7 --parity even`) every time.
  Bridges that follow RFC 2217 return to their configured settings when the
  session ends.
- `--frame` cannot be combined with `--data-bits`, `--parity` or
  `--stop-bits`. 1.5 stop bits goes with 5 data bits on most UARTs.
- `--flow` is the flow control of the bridge's serial port.

Bridges differ in what they answer (the library's
[known bridge quirks](https://github.com/gechelberger/rfc2217#known-bridge-quirks)
has the details):

- ser2net 4.6.4 never answers 1.5 stop bits or DCD/DSR flow control: the
  setting stays unconfirmed.
- ser2net 4.3.11 does not answer 1.5 stop bits, and answers DCD/DSR flow
  control with another value.
- sredird 2.2.2 on a pty confirms 8N for a 7E request, and answers rates and
  flow control it does not support with what it set instead (9600 for 250000
  baud, none for DCD/DSR).
- pyserial's PortManager does not answer DCD/DSR flow control.

An unconfirmed setting or another value is a warning, not an error, unless you
pass `--strict`: see below.

## Negotiation and exit codes

After connecting, `rfc2217` waits up to `--timeout` seconds (default 3) for the
bridge to take up RFC 2217 and to answer every setting. The bridge's output is
shown at once, also during the wait; what you type or pipe in is held until the
outcome is settled, and sent only if the session goes on.

| What the bridge did | Result |
| --- | --- |
| Refused RFC 2217 (a plain Telnet server) | Exit 4. With `--allow-telnet`: a warning, and the session carries data only, as plain Telnet |
| No answer to the RFC 2217 offer by the deadline | Exit 4: most likely this is the bridge's raw TCP port, not its RFC 2217 port |
| Applied other values than the requested ones | A warning showing the requested and reported settings, and the session goes on; exit 4 with `--strict` |
| Left some settings unanswered at the deadline | A warning, and the session goes on; exit 4 with `--strict` |

| Exit code | Meaning |
| --- | --- |
| 0 | You quit, the input ended (after `--linger`), or the bridge closed the connection (`connect`) |
| 1 | An I/O error during the session; for `pty`, also the bridge closing the connection |
| 2 | A usage error, including interactive `connect` and `pty` on Windows |
| 3 | The host could not be resolved, or did not accept the connection |
| 4 | Negotiation failed, as in the table above |
| 128 + N | Stopped by signal N (143 for SIGTERM) |

Scripts can rely on these codes: they change only in a minor release.

## connect

`rfc2217 connect TARGET` copies the port's data to stdout and your input to
the port. Messages and diagnostics go to stderr, so stdout carries nothing but
the port's data.

### Interactive

With a terminal on stdin (Linux and macOS), the terminal is put in raw mode,
so every key goes to the device, Ctrl-C and Ctrl-Z included. Commands start
with the escape key, Ctrl-] unless `--escape` names another (`^A`, `^T`, ...)
or `none`; the notice at the start of the session names it.

| Keys | Action |
| --- | --- |
| Ctrl-] `q` or Ctrl-] `.` | Quit |
| Ctrl-] `b` | Send a BREAK of 250 ms (RFC 2217 has no duration: `rfc2217` turns it off again) |
| Ctrl-] `d`, Ctrl-] `r` | Toggle DTR, RTS |
| Ctrl-] `p` | Purge the bridge's receive and transmit buffers |
| Ctrl-] `u`, Ctrl-] `n` | Next higher, next lower standard baud rate |
| Ctrl-] `s` | Status |
| Ctrl-] `e` | Local echo on or off |
| Ctrl-] `z` | Suspend: the terminal is restored, and raw again after `fg` |
| Ctrl-] `?` | Help |
| Ctrl-] Ctrl-] | Send Ctrl-] itself |

- `u` and `n` step from the requested baud rate, or from the rate the bridge
  reported when it is `keep`.
- The status shows whether RFC 2217 is active, whether the settings are
  confirmed, the requested and reported settings, the modem lines (CTS, DSR,
  RI, CD) as the bridge last reported them, and whether the bridge asked to
  suspend the data flow. DTR and RTS are shown as `rfc2217` last set them:
  the bridge cannot be asked for them.
- `--echo` turns local echo on from the start (the bridge never echoes; the
  device may). `--enter cr|lf|crlf` sets what the Enter key sends (CR by
  default), and `--lf-crlf` shows each received LF as CR LF, for devices that
  end lines with LF alone.

### Piped

When stdin is not a terminal (a pipe or a file), there is no raw mode, no
escape key and no line-end mapping: the bytes go both ways unchanged, `0xFF`
included. At the end of the input, `rfc2217` keeps copying the port's output
until it has been quiet for `--linger` seconds (default 1), then closes the
connection and exits with 0.

```sh
# Upload a file at 115200 baud and capture what the device answers.
rfc2217 connect -q bridge.local -b 115200 --linger 2 < firmware.hex > log.txt
echo "exit code $?"

# Pass only if the bridge confirmed 19200 7E1 within 5 s.
printf 'STATUS\r' | rfc2217 connect bridge.local -b 19200 --frame 7E1 --strict --timeout 5
```

On Windows, `connect` works in this mode only. With a console on stdin it
exits with code 2 and says that interactive sessions are not supported on
Windows yet; pipe the input instead:

```text
type commands.txt | rfc2217 connect bridge.local -b 9600 > reply.txt
```

## pty

`rfc2217 pty TARGET` (Linux and macOS) creates a pseudo-terminal, connects to
the bridge and copies the data between the two, until the bridge closes the
connection (exit code 1) or `rfc2217` is stopped. A program opens the pty as
if it were a serial port.

`--link PATH` also makes the pty reachable at PATH, through a symlink that is
removed when `rfc2217` exits. Use a path you can write, such as `~/ttyRFC0` or
`/tmp/ttyRFC0`, never `/dev`: that needs root, and a pty made by root cannot be
opened by other users. Port lists (IDE port pickers, pyserial's `list_ports`) show neither the
pty nor the link, so give programs the path. If `rfc2217` is killed with
SIGKILL, the link stays behind: remove it, because pty numbers are reused and
a stale link soon points at an unrelated terminal.

The line settings on the command line are sent when the session starts, as for
`connect`, and the negotiation is checked the same way. After that,
`rfc2217` polls the pty's settings about every 50 ms, and when the program
changes the baud rate, the stop bits (1 or 2) or the flow control (RTS/CTS or
XON/XOFF), the new settings go to the bridge. `--dtr` and `--rts` set the
remote lines once connected.

### What a pty cannot carry

- **Data bits and parity come from the command line only** (`--data-bits`,
  `--parity` or `--frame`). On Linux the pty driver forces 8 data bits and no
  parity whenever a program changes the settings, so what the program asks for
  never reaches `rfc2217`; depending on its C library and on what else the
  same call changes, the program's request is dropped silently or fails with
  `EINVAL`. 1.5 stop bits and DCD/DSR flow control cannot be expressed through
  a pty at all.
- **No DTR, RTS, BREAK or modem lines.** A pty has none: the program cannot
  set DTR or RTS, send a BREAK, or read CTS, DSR, CD and RI. So tools that
  reset a board through DTR and RTS do not work through the pty: esptool,
  `avrdude -c arduino`, `stm32flash -i`. Tools built on pyserial, esptool
  included, can talk to the bridge directly instead, with
  `--port rfc2217://bridge.local:2217`; for esptool's automatic reset over a
  network, see esptool's documentation on remote serial ports.
- **Changes take effect when seen.** A settings change reaches the bridge up
  to about 50 ms after the program makes it, not at its place in the data, and
  a change undone within one interval may not be seen at all. Bytes written
  right after a baud rate change can go out at the old rate.
- **No pacing.** A pty is not paced by the baud rate: `write` and `tcdrain`
  return long before the bytes are on the wire, so protocols that rely on
  timing (RS-485 direction switching, Modbus RTU frame gaps) do not work.

Programs that only exchange data and set the baud rate, stop bits or flow
control, such as picocom, minicom, screen, or most of your own code, work
unchanged.

There is no `pty` on Windows. There, com0com (a virtual null-modem pair) with
hub4com's com2tcp-rfc2217 gives programs a COM port bridged to RFC 2217, and
pyserial-based tools can use an `rfc2217://host:port` URL.

## Options

Global, for both subcommands:

- `-v`, `-vv`, `-vvv`: more diagnostics on stderr: the bridge's events, then
  the library's protocol debug logs, then a trace of every exchange.
- `-q`: errors only.

Both `connect` and `pty` take the line settings above (`-b`, `-d`,
`--parity`, `--stop-bits`, `-f`, `--frame`) and:

- `--timeout SECS`: how long to wait for the bridge's answers (default 3).
- `--strict`: fail (exit 4) unless the bridge confirms every setting with the
  requested value.
- `--allow-telnet`: carry on as a plain Telnet session when the server refuses
  RFC 2217.
- `--dtr on|off`, `--rts on|off`: set DTR and RTS once connected.

`connect` also takes:

- `--escape ^X|none`: the escape key (default `^]`), read only from a
  terminal.
- `--echo`, `--enter cr|lf|crlf`, `--lf-crlf`: local echo and line ends,
  interactive sessions only.
- `--linger SECS`: after the end of piped input, how long the port's output
  must be quiet before the session ends (default 1).

`pty` also takes `--link PATH`.

`rfc2217 help`, `rfc2217 connect --help` and `rfc2217 pty --help` list them
all.

## Compared with

- pyserial's miniterm (`python3 -m serial.tools.miniterm rfc2217://host:2217 115200`)
  is the closest to `connect`, and also always sends all five settings.
  `rfc2217` is a single binary without Python, keeps the baud rate by default,
  and tells unconfirmed, refused and mismatched settings apart.
- ser2net is the other side: a bridge that both subcommands can talk to.
- socat can give a pty for a TCP port, but over the raw TCP port: no settings
  reach the serial port.
- ttynvt (Linux, needs CUSE and root) presents a real serial device and also
  forwards data bits, parity, DTR and RTS, which a pty cannot carry.

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
