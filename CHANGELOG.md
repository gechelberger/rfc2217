# Changelog

All notable changes to this crate are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html): while
it is 0.x, a minor release may break the API and a patch release never does.
Changes to wire behaviour and to the fixed capacities are always listed.

## [Unreleased]

## [0.1.0] - 2026-10-20

The first release.

### Added

- `Client`: the client side of RFC 2217 (Telnet COM Port Control) as a sans-IO
  state machine: Telnet option negotiation, IAC escaping, the settings round,
  DTR, RTS, BREAK, purge and signature commands, and every server reply and
  notification decoded into an `Event`.
- `Server`: the bridge side, for simulators, tests and bridges: negotiation,
  every client request applied to a port model and acknowledged, modem-state
  and line-state notifications, flow-control suspend and resume, and the
  requests reported as `ServerEvent`s.
- An allocation-free `no_std` core with fixed memory and backpressure:
  `receive_into`, `output` and `consume_output`, `has_output`, `poll_event`,
  `try_write_data`, and fallible `try_*` calls that return a `CommandError` and
  change nothing on error. Each instance has a 256-byte output buffer and one
  event slot (`Client` 520 bytes, `Server` 496); input stops instead of losing
  anything.
- The `alloc` convenience layer (a default feature): `receive`,
  `receive_until_event`, `take_output`, `write_data` and infallible commands on
  the same types, which never push back.
- The readiness model: `is_ready()`, `readiness()` with the `NotReady` reasons,
  `pending_settings()`, `mismatched_settings()`, `reported_settings()` (a
  `PartialSettings`) and `requested_settings()`, with `Setting` and
  `SettingSet`.
- Typed modem and line state: `ModemState` and `LineState` with named flags,
  and `Client::modem_state()`.
- `InlineBytes` for signatures and undecodable replies: 64 bytes kept, plus the
  original length.
- Replays of real sessions with ser2net 4.6.4 and 4.3.11, sredird 2.2.2 and
  pyserial 3.5 (PortManager, and its client against `Server`).
- Examples: `blocking_client` (alloc layer), `core_client` (core) and
  `sim_server` (a simulated bridge on the core).

### Behaviour worth knowing

- Readiness is strict: `is_ready()` is true only while COM-PORT-OPTION is
  active and the bridge has answered the latest request for every setting with
  exactly the requested value, and `readiness()` says why not (`Negotiating`,
  `Refused`, `Mismatched`, `Pending`). It may never become true, so apply a
  deadline. A baud rate of 0 is the RFC 2217 query and is answered by any rate.
- Core calls are fallible and atomic. While COM-PORT-OPTION is negotiating they
  return `CommandError::Negotiating` (only `try_set_port_settings` succeeds, by
  replacing the request); while it is refused, commands return
  `CommandError::Inactive` and data flows; `CommandError::OutputFull` asks you
  to write out the output first. The alloc layer holds data and commands while
  negotiating and writes them in call order right after the settings round; on
  refusal, held data is written and held commands are dropped.
- `com_port_state()` follows only the answer to the client's
  `WILL COM-PORT-OPTION`. `Event::ComPortActive` and `Event::ComPortRefused`
  come on every transition, a withdrawal and a re-activation included. A
  withdrawal clears the settings accounting, `reported_settings()`,
  `modem_state()`, `flow_suspended()` and outstanding signature requests; a
  re-activation sends a fresh settings round.
- `Event::FlowSuspended` and `FlowResumed` come only when the state changes.
  Flow control is advisory: writes are never refused or buffered because of it.
- An empty SIGNATURE that the client did not ask for is the server asking for
  the client's: no event, and no answer in 0.1.
- `Server::new` queues the server's opening (`WILL`/`DO` BINARY and
  SUPPRESS-GO-AHEAD; 12 bytes). COM-PORT-OPTION is left to the client: the
  server accepts the client's `WILL` with `DO` but never sends
  `DO COM-PORT-OPTION` itself, because that trips a race in pyserial 3.5's
  client (it then never sends its `WILL`; up to one open in five failed).
  RFC 2217 lets either side start, so a client that waits for the server's
  `DO` does not activate; no such client is known. The server refuses
  `DO COM-PORT-OPTION` with `WONT`, ignores COM-PORT-OPTION requests until the
  client's `WILL` is accepted, and reports `ServerEvent::ComPortActive` on every
  activation.
- The server acknowledges every request automatically with the value in effect
  (auto-acknowledgement only), which suits simulators, tests and bridges whose
  port accepts every value. `ServerEvent::SettingsChanged` comes once per
  request that changed one of the five settings. DTR, RTS and BREAK requests
  produce `ServerEvent::Dtr`, `Rts` and `Break` every time, also when nothing
  changed; the server has no accessor for them, so a bridge must handle the
  events. Inbound flow-control requests are answered with the value the
  outbound setting implies; wrong-length requests and the client's
  FLOWCONTROL-SUSPEND/RESUME get no answer.
- The server's modem state comes from the application (`try_set_modem_state`,
  `set_modem_state`); the server computes the change bits and never invents a
  state. The client's masks apply, with their RFC defaults: no line-state bits
  until the client sets a line-state mask, every modem-state bit. The modem
  state is sent on activation, on a mask change, on pyserial's poll and on the
  first call; a state supplied before activation is stored and reported then.
  RFC 2217 has no change bit for RI turning on, so a rising RI alone is not
  notified; the next notification or poll shows it. Line-state, suspend and
  resume calls return `CommandError::Inactive` before activation; the alloc
  layer drops them with a debug log.
- The crate is `no_std` and has no dependencies by default. The default `alloc`
  feature needs a global allocator in the final binary, so firmware, and every
  crate in its dependency graph, uses `default-features = false`. `tracing` is
  optional and off by default.
- Both halves are 8-bit transparent whatever the BINARY option's state (no NVT
  translation). A subnegotiation is given up after 1024 bytes and the rest
  arrives as data; an option the peer enables more than 8 times is refused
  from then on.

### Known limitations

- ESP-Link is not supported: the client's opening derails its parser.
- The server cannot refuse or adjust a requested value: there are no deferred
  acknowledgements yet.
- The client cannot set line-state or modem-state masks, query DTR, RTS, BREAK
  or flow control without setting them, or suspend the server's data.
- Inbound flow control is not modelled: `FlowControl` is outbound only.
- Signatures and undecodable replies keep 64 bytes, plus their original length.
  The client does not answer a server's signature request.
- The capacities are fixed (a 256-byte output buffer and one event slot per
  instance); there are no size parameters.

### Changed from the unpublished pre-release code

- The `telnet` module, and `code()`/`from_code()` on the value enums, are
  private.
- `Client::settings()` is now `requested_settings()`. `settings_acknowledged()`
  and `unacknowledged_settings()` are gone: use `readiness()` and
  `pending_settings()`.
- `Event::Signature`, `Event::Unknown` and `ServerEvent::ClientSignature` carry
  `InlineBytes`; `Event::LineState` and `Event::ModemState` carry `LineState`
  and `ModemState`. `Server::with_signature` takes `impl AsRef<[u8]>`.
- `Server::notify_modem_state(u8)` is replaced by `try_set_modem_state` and
  `set_modem_state(ModemState)`; `notify_line_state` takes a `LineState`.
- `receive`, `take_output` and `poll_event` are `#[must_use]`.
- `tracing` is an optional feature, off by default.

[Unreleased]: https://github.com/gechelberger/rfc2217/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/gechelberger/rfc2217/releases/tag/v0.1.0
