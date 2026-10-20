//! The alloc layer (feature `alloc`): the `Vec`-based methods, written on
//! top of the core.
//!
//! These methods never push back. Writes and commands go through the core
//! `try_*` calls; when the fixed output buffer lacks room, its bytes move to
//! the instance's [`Spill`] and the call is retried. The receive loops move
//! every event from the core's slot to the spill's queue. Client calls made
//! while COM-PORT-OPTION is negotiating wait in the spill's held queue and
//! are released at the byte that ends the negotiation. The spill is created
//! by the first method here that needs it, so an instance driven only
//! through the core never touches the heap. The core reads the spill first
//! (`output`, `consume_output`, `has_output`, `poll_event`), because it
//! holds the older output and events.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;

use crate::client::{Client, ComPortState, Event};
use crate::io::{CommandError, Io, Progress};
use crate::lines::{LineState, ModemState};
use crate::log::debug;
use crate::server::{Server, ServerEvent};
use crate::settings::{PortSettings, Purge};

/// A client call made while COM-PORT-OPTION was negotiating.
#[derive(Debug)]
enum Held {
    /// Serial data as given (consecutive writes are merged).
    Data(Vec<u8>),
    Dtr(bool),
    Rts(bool),
    Break(bool),
    Purge(Purge),
    Signature,
}

/// The heap overflow of one instance. Everything here is older than what
/// the core's fixed buffer and event slot hold.
pub(crate) struct Spill<E> {
    /// Output moved out of the fixed buffer.
    out: Vec<u8>,
    /// Bytes at the front of `out` already consumed; never more than the
    /// pending bytes behind them.
    out_read: usize,
    /// Events moved out of the slot, oldest first.
    pub(crate) events: VecDeque<E>,
    /// Client calls held while negotiating, in call order.
    held: VecDeque<Held>,
}

impl<E> Default for Spill<E> {
    fn default() -> Self {
        Self {
            out: Vec::new(),
            out_read: 0,
            events: VecDeque::new(),
            held: VecDeque::new(),
        }
    }
}

/// A summary: a backlog can be large, and its bytes say little.
impl<E> fmt::Debug for Spill<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spill")
            .field("pending_bytes", &self.pending_out().len())
            .field("events", &self.events.len())
            .field("held_calls", &self.held.len())
            .finish()
    }
}

impl<E> Spill<E> {
    /// Spilled output not consumed yet.
    pub(crate) fn pending_out(&self) -> &[u8] {
        &self.out[self.out_read..]
    }

    /// Remove the first `n` bytes of [`pending_out`](Self::pending_out).
    ///
    /// # Panics
    /// If `n` exceeds them.
    pub(crate) fn consume_out(&mut self, n: usize) {
        assert!(
            n <= self.pending_out().len(),
            "consume_output past the end of output()"
        );
        self.out_read += n;
        // Drop the consumed prefix once it is as long as the pending bytes:
        // each copy moves no more bytes than were consumed since the last one
        // (amortized O(1) per byte), and `out` stays within twice what is
        // pending, however much output passes through without `take_output`.
        if self.out_read >= self.out.len() - self.out_read {
            self.out.drain(..self.out_read);
            self.out_read = 0;
        }
    }
}

impl<E> Io<E> {
    fn spill_mut(&mut self) -> &mut Spill<E> {
        self.spill.get_or_insert_with(Box::default)
    }

    /// Move the fixed buffer's bytes behind the spilled ones, emptying the
    /// fixed buffer. Returns `false` if it was empty.
    fn spill_output(&mut self) -> bool {
        let n = self.out.len();
        if n == 0 {
            return false;
        }
        let Self { out, spill, .. } = self;
        let spill = spill.get_or_insert_with(Box::default);
        spill.out.extend_from_slice(out.as_slice());
        out.consume(n);
        true
    }

    /// Move the slot's event behind the queued ones. Returns `false` if the
    /// slot was empty.
    fn spill_event(&mut self) -> bool {
        match self.event.take() {
            Some(event) => {
                self.spill_mut().events.push_back(event);
                true
            }
            None => false,
        }
    }

    /// Remove and return all pending output: the spill's, then the fixed
    /// buffer's.
    fn take_output(&mut self) -> Vec<u8> {
        let mut bytes = match &mut self.spill {
            Some(spill) => {
                let mut bytes = core::mem::take(&mut spill.out);
                bytes.drain(..core::mem::take(&mut spill.out_read));
                bytes
            }
            None => Vec::new(),
        };
        bytes.extend_from_slice(self.out.as_slice());
        self.out.consume(self.out.len());
        bytes
    }

    fn has_held(&self) -> bool {
        self.spill
            .as_ref()
            .is_some_and(|spill| !spill.held.is_empty())
    }
}

/// What the shared receive loops need from a half.
trait Core {
    type Event;
    fn step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress;
    fn io(&mut self) -> &mut Io<Self::Event>;
}

impl Core for Client {
    type Event = Event;

    fn step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress {
        self.receive_into(input, payload)
    }

    fn io(&mut self) -> &mut Io<Event> {
        &mut self.io
    }
}

impl Core for Server {
    type Event = ServerEvent;

    fn step(&mut self, input: &[u8], payload: &mut [u8]) -> Progress {
        self.receive_into(input, payload)
    }

    fn io(&mut self) -> &mut Io<ServerEvent> {
        &mut self.io
    }
}

/// After a receive step that consumed nothing: move the output to the
/// spill. With the output empty, the event slot empty and room for payload,
/// the next step consumes at least one byte.
fn make_room<E>(io: &mut Io<E>) {
    assert!(
        io.spill_output(),
        "receive_into consumed nothing with the output empty"
    );
}

/// `receive`: all of `bytes`, as one loop over `receive_into`.
fn receive<C: Core>(half: &mut C, bytes: &[u8]) -> Vec<u8> {
    // Payload never exceeds the input it came from.
    let mut payload = alloc::vec![0; bytes.len()];
    let mut len = 0;
    let mut rest = bytes;
    // An event a core call left in the slot is older than this call's.
    half.io().spill_event();
    while !rest.is_empty() {
        let p = half.step(rest, &mut payload[len..]);
        rest = &rest[p.consumed..];
        len += p.payload_len;
        half.io().spill_event();
        if p.consumed == 0 {
            make_room(half.io());
        }
    }
    payload.truncate(len);
    payload
}

/// `receive_until_event`: like [`receive`], but stops right after the first
/// byte that produced an event; returns the bytes consumed.
fn receive_until_event<C: Core>(half: &mut C, input: &[u8], payload: &mut Vec<u8>) -> usize {
    half.io().spill_event();
    let start = payload.len();
    payload.resize(start + input.len(), 0);
    let mut len = start;
    let mut used = 0;
    while used < input.len() {
        let p = half.step(&input[used..], &mut payload[len..]);
        used += p.consumed;
        len += p.payload_len;
        if half.io().spill_event() {
            break;
        }
        if p.consumed == 0 {
            make_room(half.io());
        }
    }
    payload.truncate(len);
    used
}

/// Run a core call, moving the fixed output to the spill and retrying
/// whenever the call reports [`CommandError::OutputFull`]. Other errors are
/// returned.
fn with_room<C: Core, T>(
    half: &mut C,
    mut call: impl FnMut(&mut C) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    loop {
        match call(half) {
            Err(CommandError::OutputFull) => assert!(
                half.io().spill_output(),
                "a call found no room in an empty output buffer"
            ),
            result => return result,
        }
    }
}

/// The alloc layer of the client: the core's calls without push-back, plus
/// an ordered queue of the calls made while COM-PORT-OPTION is negotiating.
/// Output, events and held calls are kept on the heap until drained, and the
/// heap buffer for output keeps its largest size until
/// [`take_output`](Client::take_output) hands the bytes out. Pick this layer
/// or the core for an instance: mixing them is defined (bytes leave in call
/// order, events in the order they happened) but not recommended.
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
impl Client {
    /// Process all of `bytes`; returns the serial payload they carried. A
    /// loop over [`receive_into`](Self::receive_into) that never stops for
    /// room: events are queued for [`poll_event`](Self::poll_event) and
    /// output for [`take_output`](Self::take_output), and memory grows until
    /// they are drained.
    ///
    /// Chunk-split invariant: the payload, the events in order, the output
    /// and the state depend only on the bytes received and on where your
    /// own calls were made, never on how the input was split into `receive`
    /// calls or on when output was taken. They are what `receive_into`
    /// yields for the same bytes and calls, except that the calls this layer
    /// holds while COM-PORT-OPTION is negotiating are written at the byte
    /// that activates it (the core refuses them instead). Events are queued,
    /// so their position in the payload is lost within one call: use
    /// [`receive_until_event`](Self::receive_until_event) to keep it.
    #[must_use = "the returned bytes are serial data from the bridge"]
    pub fn receive(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.check_held();
        receive(self, bytes)
    }

    /// Process `input` up to and including the first byte that produces an
    /// event, appending its payload to `payload`; returns the number of
    /// bytes consumed (at least one for non-empty input). Consumes all of
    /// `input` if no byte produces an event, and returns 0 for empty input.
    /// Never stops for room, like [`receive`](Self::receive); the event is
    /// queued for [`poll_event`](Self::poll_event).
    ///
    /// The payload appended by this call precedes the event and the payload
    /// of later calls follows it: the event's position is exact, as with
    /// [`receive_into`](Self::receive_into), which this is a loop over. A
    /// loop over this method, passing `&input[consumed..]` again, yields the
    /// same payload, events, output and state as `receive`, and is chunk-split
    /// invariant in the same way.
    ///
    /// ```
    /// use rfc2217::{Client, Event, PortSettings, Purge};
    ///
    /// let mut client = Client::new(PortSettings::default());
    /// let _ = client.take_output();
    /// // The bridge accepts COM-PORT-OPTION, sends "AB", reports a purge of
    /// // its receive buffer, then sends "C".
    /// let input = [0xFF, 0xFD, 0x2C, b'A', b'B', 0xFF, 0xFA, 0x2C, 0x70, 0x01, 0xFF, 0xF0, b'C'];
    /// let (mut payload, mut seen) = (Vec::new(), Vec::new());
    /// let mut rest = &input[..];
    /// while !rest.is_empty() {
    ///     let n = client.receive_until_event(rest, &mut payload);
    ///     rest = &rest[n..];
    ///     while let Some(event) = client.poll_event() {
    ///         seen.push((payload.len(), event)); // the event's position in the payload
    ///     }
    /// }
    /// assert_eq!(payload, b"ABC");
    /// assert_eq!(seen, [(0, Event::ComPortActive), (2, Event::Purged(Purge::Receive))]);
    /// ```
    #[must_use = "bytes after the returned count were not processed"]
    pub fn receive_until_event(&mut self, input: &[u8], payload: &mut Vec<u8>) -> usize {
        self.check_held();
        receive_until_event(self, input, payload)
    }

    /// Remove and return all bytes to write to the socket now (after `new`,
    /// every receive call and every call below): the `Vec` form of
    /// [`output`](Self::output) and [`consume_output`](Self::consume_output).
    /// The heap backlog goes out with it, so the memory the output took is
    /// handed to you. Calls held while COM-PORT-OPTION is negotiating are not
    /// output yet.
    #[must_use = "these bytes must be written to the socket"]
    pub fn take_output(&mut self) -> Vec<u8> {
        self.check_held();
        self.io.take_output()
    }

    /// Escape and queue serial data: [`try_write_data`](Self::try_write_data)
    /// without push-back. While COM-PORT-OPTION is negotiating the data is
    /// held, in call order with the commands, and written right after the
    /// settings round, or when the peer refuses the option; after that it is
    /// written at once, also while the option is refused. A peer that never
    /// answers (a raw TCP port) keeps it held, and held memory grows with
    /// every call. Output that does not fit the fixed buffer waits on the
    /// heap until drained.
    pub fn write_data(&mut self, data: &[u8]) {
        self.check_held();
        if data.is_empty() {
            return;
        }
        if self.com_port_state() == ComPortState::Negotiating {
            let held = &mut self.io.spill_mut().held;
            match held.back_mut() {
                Some(Held::Data(bytes)) => bytes.extend_from_slice(data),
                _ => held.push_back(Held::Data(data.to_vec())),
            }
        } else {
            self.write_all(data);
        }
    }

    /// Replace the requested settings:
    /// [`try_set_port_settings`](Self::try_set_port_settings) without
    /// push-back, with the same known limits of the reply accounting. All
    /// five are sent now if COM-PORT-OPTION is active, else once it becomes
    /// active. Never held: the next settings round reads the request. Output
    /// that does not fit the fixed buffer waits on the heap until drained.
    pub fn set_port_settings(&mut self, settings: PortSettings) {
        self.check_held();
        let result = with_room(self, |client| client.try_set_port_settings(settings));
        debug_assert_eq!(result, Ok(()), "only a lack of room can refuse it");
    }

    /// Set the DTR line: [`try_set_dtr`](Self::try_set_dtr) without
    /// push-back. Held while COM-PORT-OPTION is negotiating and sent after
    /// the settings round in call order; dropped (debug log) while it is
    /// refused, and when the peer refuses it while the command is held.
    /// Output that does not fit the fixed buffer, and held calls, wait on
    /// the heap until drained. Replies ([`Event::Dtr`]) may never arrive, or
    /// arrive as a flow control value (sredird), as `try_set_dtr` describes.
    pub fn set_dtr(&mut self, on: bool) {
        self.command(Held::Dtr(on));
    }

    /// Set the RTS line: [`try_set_rts`](Self::try_set_rts) without
    /// push-back. Held, dropped and kept on the heap as
    /// [`set_dtr`](Self::set_dtr); replies ([`Event::Rts`]) may never arrive.
    pub fn set_rts(&mut self, on: bool) {
        self.command(Held::Rts(on));
    }

    /// Set or clear a break condition: [`try_set_break`](Self::try_set_break)
    /// without push-back. Held, dropped and kept on the heap as
    /// [`set_dtr`](Self::set_dtr); replies ([`Event::Break`]) may never
    /// arrive.
    pub fn set_break(&mut self, on: bool) {
        self.command(Held::Break(on));
    }

    /// Ask the bridge to discard buffered data, answered by
    /// [`Event::Purged`]: [`try_purge`](Self::try_purge) without push-back.
    /// Held, dropped and kept on the heap as [`set_dtr`](Self::set_dtr).
    pub fn purge(&mut self, purge: Purge) {
        self.command(Held::Purge(purge));
    }

    /// Ask for the server's signature, answered by [`Event::Signature`]:
    /// [`try_request_signature`](Self::try_request_signature) without
    /// push-back. Held, dropped and kept on the heap as
    /// [`set_dtr`](Self::set_dtr); counted as outstanding once it is written,
    /// not while it is held.
    pub fn request_signature(&mut self) {
        self.command(Held::Signature);
    }

    /// Write the calls held while COM-PORT-OPTION was negotiating, in call
    /// order: called by the core at the byte that ends the negotiation, when
    /// entering Active after the settings round, or when entering Refused
    /// before the event (held commands are then dropped). Nothing is held
    /// after that, so a later withdrawal or re-activation releases nothing.
    /// The released bytes go to the spill: they are the caller's calls, not
    /// replies to that byte, so they must not use its receive reserve.
    pub(crate) fn release_held(&mut self) {
        let held = match &mut self.io.spill {
            Some(spill) if !spill.held.is_empty() => core::mem::take(&mut spill.held),
            _ => return,
        };
        for call in held {
            self.run(call);
        }
        self.io.spill_output();
    }

    fn command(&mut self, call: Held) {
        self.check_held();
        if self.com_port_state() == ComPortState::Negotiating {
            self.io.spill_mut().held.push_back(call);
        } else {
            self.run(call);
        }
    }

    /// Make one call through the core, spilling for room. A command the core
    /// refuses (COM-PORT-OPTION refused) is dropped.
    fn run(&mut self, call: Held) {
        let result = match &call {
            Held::Data(data) => {
                self.write_all(data);
                return;
            }
            &Held::Dtr(on) => with_room(self, |client| client.try_set_dtr(on)),
            &Held::Rts(on) => with_room(self, |client| client.try_set_rts(on)),
            &Held::Break(on) => with_room(self, |client| client.try_set_break(on)),
            &Held::Purge(purge) => with_room(self, |client| client.try_purge(purge)),
            Held::Signature => with_room(self, Client::try_request_signature),
        };
        if let Err(error) = result {
            debug!("dropping {call:?}: {error}");
        }
    }

    /// Write all of `data` (not while negotiating), spilling for room.
    fn write_all(&mut self, data: &[u8]) {
        let mut rest = data;
        while !rest.is_empty() {
            match with_room(self, |client| client.try_write_data(rest)) {
                Ok(n) => rest = &rest[n..],
                Err(error) => unreachable!("data refused outside negotiation: {error}"),
            }
        }
    }

    /// Every transition out of Negotiating releases the held calls.
    fn check_held(&self) {
        debug_assert!(
            !self.io.has_held() || self.com_port_state() == ComPortState::Negotiating,
            "calls held while COM-PORT-OPTION is not negotiating"
        );
    }
}

/// The alloc layer of the server: the core's calls without push-back.
/// Output and events are kept on the heap until drained, and the heap buffer
/// for output keeps its largest size until
/// [`take_output`](Server::take_output) hands the bytes out.
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
impl Server {
    /// Process all of `bytes`; returns the serial payload they carried. A
    /// loop over [`receive_into`](Self::receive_into) that never stops for
    /// room: events are queued for [`poll_event`](Self::poll_event) and
    /// output for [`take_output`](Self::take_output), and memory grows until
    /// they are drained.
    ///
    /// Chunk-split invariant: the payload, the events in order, the output
    /// and the state depend only on the bytes received and on where your
    /// own calls were made, never on how the input was split into `receive`
    /// calls or on when output was taken. They are what `receive_into`
    /// yields for the same bytes and calls. Events are queued, so their
    /// position in the payload is lost within one call: use
    /// [`receive_until_event`](Self::receive_until_event) to keep it.
    #[must_use = "the returned bytes are serial data from the client"]
    pub fn receive(&mut self, bytes: &[u8]) -> Vec<u8> {
        receive(self, bytes)
    }

    /// Process `input` up to and including the first byte that produces an
    /// event, appending its payload to `payload`; returns the number of
    /// bytes consumed (at least one for non-empty input). Consumes all of
    /// `input` if no byte produces an event, and returns 0 for empty input.
    /// Never stops for room, like [`receive`](Self::receive); the event is
    /// queued for [`poll_event`](Self::poll_event).
    ///
    /// The payload appended by this call precedes the event and the payload
    /// of later calls follows it, so a bridge can apply `SettingsChanged`,
    /// `Purge` and `Break` at their exact position in the data, as with
    /// [`receive_into`](Self::receive_into), which this is a loop over. A
    /// loop over this method, passing `&input[consumed..]` again, yields the
    /// same payload, events, output and state as `receive`, and is chunk-split
    /// invariant in the same way.
    #[must_use = "bytes after the returned count were not processed"]
    pub fn receive_until_event(&mut self, input: &[u8], payload: &mut Vec<u8>) -> usize {
        receive_until_event(self, input, payload)
    }

    /// Remove and return all bytes to write to the socket now (after `new`,
    /// every receive call and every call below): the `Vec` form of
    /// [`output`](Self::output) and [`consume_output`](Self::consume_output).
    /// The heap backlog goes out with it, so the memory the output took is
    /// handed to you.
    #[must_use = "these bytes must be written to the socket"]
    pub fn take_output(&mut self) -> Vec<u8> {
        self.io.take_output()
    }

    /// Escape and queue serial data for the client:
    /// [`try_write_data`](Self::try_write_data) without push-back. Never held
    /// or gated. Output that does not fit the fixed buffer waits on the heap
    /// until drained.
    pub fn write_data(&mut self, data: &[u8]) {
        let mut rest = data;
        while !rest.is_empty() {
            match with_room(self, |server| server.try_write_data(rest)) {
                Ok(n) => rest = &rest[n..],
                Err(error) => unreachable!("server data refused: {error}"),
            }
        }
    }

    /// Record the levels of the port's modem lines and notify the client of
    /// what changed: [`try_set_modem_state`](Self::try_set_modem_state)
    /// without push-back. While COM-PORT-OPTION is not active the levels are
    /// stored and sent when the client enables it; pyserial's client needs
    /// them. Output that does not fit the fixed buffer waits on the heap
    /// until drained.
    pub fn set_modem_state(&mut self, state: ModemState) {
        let result = with_room(self, |server| server.try_set_modem_state(state));
        debug_assert_eq!(result, Ok(()), "only a lack of room can refuse it");
    }

    /// Send `state` through the client's line-state mask:
    /// [`try_notify_line_state`](Self::try_notify_line_state) without
    /// push-back. Dropped (debug log) while COM-PORT-OPTION is not active.
    /// Output that does not fit the fixed buffer waits on the heap until
    /// drained.
    pub fn notify_line_state(&mut self, state: LineState) {
        let result = with_room(self, |server| server.try_notify_line_state(state));
        dropped(format_args!("NOTIFY-LINESTATE {state:?}"), result);
    }

    /// Ask the client to pause sending:
    /// [`try_suspend_flow`](Self::try_suspend_flow) without push-back.
    /// Advisory, and ignored by pyserial's client (it expects code 8, not
    /// 108). Dropped (debug log) while COM-PORT-OPTION is not active. Output
    /// that does not fit the fixed buffer waits on the heap until drained.
    pub fn suspend_flow(&mut self) {
        let result = with_room(self, Server::try_suspend_flow);
        dropped(format_args!("FLOWCONTROL-SUSPEND"), result);
    }

    /// Allow the client to resume sending:
    /// [`try_resume_flow`](Self::try_resume_flow) without push-back.
    /// Dropped (debug log) while COM-PORT-OPTION is not active. Output that
    /// does not fit the fixed buffer waits on the heap until drained.
    pub fn resume_flow(&mut self) {
        let result = with_room(self, Server::try_resume_flow);
        dropped(format_args!("FLOWCONTROL-RESUME"), result);
    }
}

/// A server notification the core refused, because the client has not
/// enabled COM-PORT-OPTION, is dropped.
fn dropped(call: fmt::Arguments<'_>, result: Result<(), CommandError>) {
    if let Err(error) = result {
        debug_assert_eq!(
            error,
            CommandError::Inactive,
            "only an inactive option can refuse it"
        );
        debug!("dropping {call}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telnet::{DO, IAC, OPT_COM_PORT};

    /// `{:?}` on an alloc-layer instance summarises its spill instead of
    /// printing every spilled byte.
    #[test]
    fn spill_debug_is_a_summary() {
        let mut client = Client::new(PortSettings::default());
        client.set_dtr(false);
        client.write_data(&[0x41; 1000]);
        let _ = client.receive(&[0xFF, 0xFA, 44, 107, 0x30, 0xFF, 0xF0]);
        let spill = client.io.spill.as_deref().expect("calls were held");
        assert_eq!(
            format!("{spill:?}"),
            "Spill { pending_bytes: 0, events: 1, held_calls: 2 }"
        );
        // Released at activation: the opening, the round, the DTR command
        // and the data are all spilled output now.
        let _ = client.receive(&[IAC, DO, OPT_COM_PORT]);
        let summary = format!(
            "Spill {{ pending_bytes: {}, events: 2, held_calls: 0 }}",
            18 + 38 + 7 + 1000
        );
        let debug = format!("{client:?}");
        assert!(debug.contains(&summary), "{debug}");
        assert!(!debug.contains("65, 65"), "spilled bytes printed: {debug}");
    }

    /// Alloc writes spilling while the socket takes the output through the
    /// core in parts: the spill drops what was consumed, so it holds at most
    /// twice its pending bytes however many have passed through, and the
    /// bytes still leave in order.
    #[test]
    fn spill_frees_consumed_output_while_drained_in_parts() {
        let mut client = Client::new(PortSettings::default());
        let _ = client.take_output();
        let _ = client.receive(&[IAC, DO, OPT_COM_PORT]);
        let _ = client.take_output();
        assert_eq!(client.com_port_state(), ComPortState::Active);

        let mut written = Vec::new();
        let mut sent = Vec::new();
        let mut pieces = [1, 50, 3, 20, 7].iter().cycle();
        let mut max_kept = 0;
        for i in 0..4000_usize {
            let chunk = [(i % 200) as u8; 50];
            client.write_data(&chunk);
            written.extend_from_slice(&chunk);
            // The first 20 writes leave a backlog of about 1000 bytes; then
            // the socket takes 50 bytes per write, in pieces, keeping it there.
            let mut quota = if i < 20 { 0 } else { 50 };
            while quota > 0 {
                let out = client.output();
                let n = out.len().min(quota).min(*pieces.next().unwrap());
                sent.extend_from_slice(&out[..n]);
                client.consume_output(n);
                quota -= n;
            }
            if let Some(spill) = client.io.spill.as_deref() {
                let pending = spill.pending_out().len();
                assert!(
                    spill.out.len() <= 2 * pending,
                    "{} bytes kept for {pending} pending after {i} writes",
                    spill.out.len()
                );
                max_kept = max_kept.max(spill.out.len());
            }
        }
        assert!(max_kept > 0, "the spill was never used");
        assert!(sent.len() > 50 * max_kept, "too little passed through");
        sent.extend_from_slice(&client.take_output());
        assert_eq!(sent, written);
    }
}
