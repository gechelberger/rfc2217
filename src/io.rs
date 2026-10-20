//! The core's fixed storage shared by both halves (the output buffer and the
//! event slot), and the small public types of the core I/O API.

use core::fmt;

/// What one `receive_into` call did. Pass `&input[consumed..]` again later.
///
/// `receive_into` stops before a byte it cannot complete (the payload buffer
/// is full, the output buffer lacks room for the replies that byte may cause,
/// or an event has not been polled yet) and right after a byte that produced
/// an event. Bytes after `consumed` were not looked at. With the output
/// written out and the events polled, a call with non-empty input consumes
/// at least one byte, so `consumed == 0` tells you to do one of those first.
/// Fields may be added in later releases (a stop reason, for example).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[must_use = "input after `consumed` was not processed; `payload_len` payload bytes were written"]
pub struct Progress {
    /// Input bytes processed. The rest of the input was not looked at.
    pub consumed: usize,
    /// Serial payload bytes written to the start of the payload buffer.
    pub payload_len: usize,
}

/// Why a `try_*` call wrote nothing and changed nothing. Retry as the
/// variant says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CommandError {
    /// The fixed output buffer lacks room for the whole call: write out
    /// `output()`, `consume_output(n)`, then retry.
    OutputFull,
    /// Client only: COM-PORT-OPTION is still being negotiated, and the
    /// settings round must go first. Retry after `Event::ComPortActive` (or
    /// give up at your deadline).
    Negotiating,
    /// COM-PORT-OPTION is not active: the client is Refused (the peer
    /// declined or withdrew it), or the server's client has not enabled it.
    /// No command or notification can be sent until it is; data still can.
    Inactive,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OutputFull => "output buffer full: write out pending output, then retry",
            Self::Negotiating => {
                "COM-PORT-OPTION is still being negotiated: retry once it is active"
            }
            Self::Inactive => {
                "COM-PORT-OPTION is not active (refused, withdrawn, or not enabled by the peer)"
            }
        })
    }
}

impl core::error::Error for CommandError {}

/// Bytes the output buffer of either half holds.
pub(crate) const OUTPUT_CAPACITY: usize = 256;

/// Linear output buffer: the pending bytes are always one contiguous slice.
/// Bytes are encoded when they are queued and never change afterwards.
pub(crate) struct OutBuf {
    buf: [u8; OUTPUT_CAPACITY],
    /// First pending byte.
    start: u16,
    /// One past the last pending byte.
    end: u16,
}

impl OutBuf {
    pub(crate) const fn new() -> Self {
        Self {
            buf: [0; OUTPUT_CAPACITY],
            start: 0,
            end: 0,
        }
    }

    /// The pending bytes, oldest first.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf[usize::from(self.start)..usize::from(self.end)]
    }

    /// Number of pending bytes.
    pub(crate) fn len(&self) -> usize {
        usize::from(self.end - self.start)
    }

    /// Bytes that can still be pushed.
    pub(crate) fn free(&self) -> usize {
        OUTPUT_CAPACITY - self.len()
    }

    /// Remove the first `n` pending bytes.
    ///
    /// # Panics
    /// If `n` exceeds the pending bytes.
    pub(crate) fn consume(&mut self, n: usize) {
        assert!(n <= self.len(), "consume_output past the end of output()");
        // `n <= len <= OUTPUT_CAPACITY`, so it fits.
        self.start += n as u16;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }

    /// Append `bytes`, moving the pending bytes to the front first if they
    /// would not fit behind the last one. Callers check `free()` against a
    /// reserve or budget first, so running out of room here is a crate bug.
    ///
    /// # Panics
    /// If `bytes` does not fit (also in release builds: nothing is ever
    /// dropped silently).
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        assert!(
            bytes.len() <= self.free(),
            "output buffer overflow: {} bytes pushed with {} free",
            bytes.len(),
            self.free()
        );
        let mut end = usize::from(self.end);
        if end + bytes.len() > OUTPUT_CAPACITY {
            let start = usize::from(self.start);
            self.buf.copy_within(start..end, 0);
            end -= start;
            self.start = 0;
        }
        self.buf[end..end + bytes.len()].copy_from_slice(bytes);
        // At most OUTPUT_CAPACITY, which fits.
        self.end = (end + bytes.len()) as u16;
    }
}

impl fmt::Debug for OutBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} of {OUTPUT_CAPACITY} bytes pending", self.len())
    }
}

/// The output buffer and the event slot of one half, plus (with the `alloc`
/// feature) the alloc layer's heap overflow. Only the alloc-layer methods
/// ever create the spill; the core reads it first because it holds the
/// older output and events.
#[derive(Debug)]
pub(crate) struct Io<E> {
    pub(crate) out: OutBuf,
    /// The event the last processed byte produced, until it is polled.
    pub(crate) event: Option<E>,
    #[cfg(feature = "alloc")]
    pub(crate) spill: Option<alloc::boxed::Box<crate::buffered::Spill<E>>>,
}

impl<E> Io<E> {
    pub(crate) const fn new() -> Self {
        Self {
            out: OutBuf::new(),
            event: None,
            #[cfg(feature = "alloc")]
            spill: None,
        }
    }

    /// The oldest pending output: the spill's bytes while it has any, then
    /// the fixed buffer's.
    pub(crate) fn output(&self) -> &[u8] {
        #[cfg(feature = "alloc")]
        if let Some(spill) = &self.spill {
            if !spill.pending_out().is_empty() {
                return spill.pending_out();
            }
        }
        self.out.as_slice()
    }

    /// Remove the first `n` bytes of [`output`](Self::output).
    ///
    /// # Panics
    /// If `n > output().len()`.
    pub(crate) fn consume_output(&mut self, n: usize) {
        #[cfg(feature = "alloc")]
        if let Some(spill) = &mut self.spill {
            if !spill.pending_out().is_empty() {
                spill.consume_out(n);
                return;
            }
        }
        self.out.consume(n);
    }

    pub(crate) fn has_output(&self) -> bool {
        !self.output().is_empty()
    }

    /// The oldest event: the spill's queued events first, then the slot.
    pub(crate) fn poll_event(&mut self) -> Option<E> {
        #[cfg(feature = "alloc")]
        if let Some(spill) = &mut self.spill {
            if let Some(event) = spill.events.pop_front() {
                return Some(event);
            }
        }
        self.event.take()
    }

    /// An event is waiting in the slot: no input byte may be processed.
    pub(crate) fn event_pending(&self) -> bool {
        self.event.is_some()
    }

    /// Put the event of the byte being processed in the slot.
    ///
    /// # Panics
    /// If the slot is occupied (also in release builds): input is never
    /// processed while it is, and a byte produces at most one event, so
    /// this is a crate bug.
    pub(crate) fn put_event(&mut self, event: E) {
        assert!(self.event.is_none(), "event slot overwritten");
        self.event = Some(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "output buffer overflow")]
    fn out_buf_overflow_panics() {
        let mut out = OutBuf::new();
        out.push(&[1; 200]);
        out.consume(10);
        out.push(&[2; 66]);
        assert_eq!(out.free(), 0);
        out.push(&[3]);
    }

    #[test]
    #[should_panic(expected = "event slot overwritten")]
    fn event_slot_overwrite_panics() {
        let mut io = Io::<u8>::new();
        io.put_event(1);
        io.put_event(2);
    }

    #[test]
    fn out_buf_compacts_and_lends_the_oldest_bytes() {
        let mut out = OutBuf::new();
        assert!(out.as_slice().is_empty());
        assert_eq!(format!("{out:?}"), "0 of 256 bytes pending");

        out.push(&[1; 250]);
        out.consume(200);
        assert_eq!(out.as_slice(), &[1; 50]);
        assert_eq!(out.free(), 206);

        // 100 bytes do not fit behind the last pending byte: the pending
        // bytes move to the front, oldest first, and nothing is lost.
        let fresh: Vec<u8> = (0..100).collect();
        out.push(&fresh);
        assert_eq!(out.len(), 150);
        assert_eq!(&out.as_slice()[..50], &[1; 50]);
        assert_eq!(&out.as_slice()[50..], &fresh[..]);
        assert_eq!(format!("{out:?}"), "150 of 256 bytes pending");

        // The buffer can be filled completely and lent in pieces.
        out.push(&[9; 106]);
        assert_eq!(out.free(), 0);
        out.consume(1);
        assert_eq!(out.as_slice()[0], 1);
        out.consume(49);
        assert_eq!(out.as_slice()[0], 0);
        out.consume(out.len());
        assert!(out.as_slice().is_empty());
        assert_eq!(out.free(), OUTPUT_CAPACITY);
    }
}
