# Changelog

<!-- Releasing, and the one-time setup for 0.1.0 (published by hand): see the
header of .github/workflows/release-cli.yml. -->

All notable changes to rfc2217-cli, the `rfc2217` command, are documented in
this file. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): while it is 0.x, a
minor release may change the command line and a patch release never does.
Changes to exit codes, to defaults and to what is sent to the bridge are always
listed. The library has its own changelog.

## [Unreleased]

## [0.1.0] - Unreleased

The first release, built on rfc2217 0.1.

### Added

- `rfc2217 connect TARGET`: a session with a remote serial port through an
  RFC 2217 bridge.
  - Interactive when stdin is a terminal (Linux and macOS): raw mode, and an
    escape key (Ctrl-] by default, `--escape ^X|none`) followed by `q` or `.`
    quit, `b` a 250 ms BREAK, `d` and `r` toggle DTR and RTS, `p` purge both
    of the bridge's buffers, `u` and `n` the next higher or lower standard
    baud rate, `s` status, `e` local echo, `z` suspend, `?` help, and the
    escape key again to send it. `--echo`, `--enter cr|lf|crlf` and
    `--lf-crlf` for local echo and line ends.
  - Piped when stdin is not a terminal (every platform): the bytes are copied
    both ways unchanged, and after the end of the input the port's output is
    copied until it has been quiet for `--linger` seconds (default 1).
- `rfc2217 pty TARGET` (Linux and macOS): the remote port as a local
  pseudo-terminal, optionally reachable through a `--link PATH` symlink that is
  removed on exit. Its path is printed on stdout once the bridge has answered.
  Changes that the program using the pty makes to the baud rate, stop bits and
  flow control are picked up by polling about every 50 ms and sent to the
  bridge; with `--baud keep` the pty shows the bridge's rate. On Linux, the
  port's data is dropped while no program has the pty open.
- Line settings for both: `-b`/`--baud RATE|keep`, `-d`/`--data-bits 5-8`,
  `--parity none|odd|even|mark|space`, `--stop-bits 1|1.5|2`,
  `-f`/`--flow none|xonxoff|rtscts|dcd|dsr`, and `--frame` (`8N1`, `7E1`,
  `5N1.5`) for the first three in one.
- `--dtr on|off` and `--rts on|off`, set once connected.
- Negotiation checks: `--timeout SECS` (default 3), `--strict` and
  `--allow-telnet`.
- Targets `host`, `host:port`, `[v6addr]:port`, a bare IPv6 address and
  `rfc2217://host:port`, port 2217 by default. Every resolved address is
  tried; TCP keepalive notices a bridge that disappears.
- Exit codes: 0 for a normal end, 1 an I/O error, 2 a usage error, 3 could not
  resolve or connect, 4 negotiation failed, 128+N stopped by signal N.
- `-v`, `-vv` and `-vvv` for the bridge's events, the library's protocol debug
  logs and a trace of every exchange; `-q` for errors only. Messages and logs
  go to stderr; stdout carries only the port's data (`connect`) or the pty's
  path (`pty`).

### Behaviour worth knowing

- `--baud` defaults to `keep`, the RFC 2217 query: the bridge's baud rate is
  left as it is. Data bits, parity, stop bits and flow control are always sent,
  8N1 without flow control unless given, because the library can query only
  the baud rate. A notice says so when none of them was given.
- A server that refuses RFC 2217 fails the session (exit 4) unless
  `--allow-telnet`. No answer by the deadline fails it too: that is most
  likely the bridge's raw TCP port. Other values than the requested ones, or
  settings still unconfirmed at the deadline, give a warning, and fail only
  with `--strict`.
- Local input is held until the negotiation has settled; the port's output is
  shown at once.
- The bridge closing the connection ends `connect` with exit 0 and `pty` with
  exit 1.

### Known limitations

- `pty`: data bits and parity come from the command line only (on Linux the
  pty driver forces 8 data bits and no parity); a pty has no DTR, RTS, BREAK
  or modem lines, so tools that reset a board through them do not work; it is
  not paced by the baud rate. On macOS it has not run on a Mac yet.
- Windows: `connect` with piped or redirected input only, and no `pty`.
- No `serve` (sharing a local serial port) and no reconnecting yet.

[Unreleased]: https://github.com/gechelberger/rfc2217/compare/rfc2217-cli-v0.1.0...HEAD
[0.1.0]: https://github.com/gechelberger/rfc2217/releases/tag/rfc2217-cli-v0.1.0
