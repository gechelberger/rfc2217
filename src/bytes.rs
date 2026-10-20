//! [`InlineBytes`]: a short byte string stored inline, with its original length.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

/// Up to [`CAPACITY`](Self::CAPACITY) bytes stored inline, plus the length
/// the value had on the wire. Never allocates; `Copy`.
///
/// It carries the payload of `Event::Signature`, `Event::Unknown` and
/// `ServerEvent::ClientSignature`, and stores the signature given to
/// `Server::with_signature`. The first `CAPACITY` bytes are kept; a longer
/// value is truncated, and [`original_len`](Self::original_len) and
/// [`is_truncated`](Self::is_truncated) say so, so truncation is never
/// silent. Dereferences to `[u8]`. The bytes are raw (`IAC IAC` unescaped)
/// and never decoded as text.
///
/// ```
/// use rfc2217::{Event, InlineBytes};
///
/// // A signature, as the client reports it.
/// let event = Event::Signature(InlineBytes::from("ser2net 4.6.4"));
/// if let Event::Signature(signature) = &event {
///     assert!(signature.starts_with(b"ser2net")); // through `Deref<Target = [u8]>`
///     assert!(!signature.is_truncated());
/// }
///
/// // A longer value keeps its first 64 bytes and says how long it was.
/// let long = InlineBytes::new(&[b'x'; 100]);
/// assert_eq!(long.len(), InlineBytes::CAPACITY);
/// assert_eq!(long.original_len(), 100);
/// assert!(long.is_truncated());
/// assert!(long != [b'x'; 64][..]); // a truncated value equals no slice
/// ```
#[derive(Clone, Copy)]
pub struct InlineBytes {
    /// Length on the wire, saturating at `u16::MAX`. The kept length is
    /// `min(original_len, CAPACITY)`.
    original_len: u16,
    buf: [u8; InlineBytes::CAPACITY],
}

impl InlineBytes {
    /// Bytes kept inline. Every signature observed from a real bridge is at
    /// most 24 bytes long.
    pub const CAPACITY: usize = 64;

    /// The empty value (also [`Default`]).
    pub const EMPTY: Self = Self {
        original_len: 0,
        buf: [0; Self::CAPACITY],
    };

    /// Keeps the first [`CAPACITY`](Self::CAPACITY) bytes of `bytes`;
    /// [`original_len`](Self::original_len) is `bytes.len()` (saturating at
    /// `u16::MAX`).
    pub fn new(bytes: &[u8]) -> Self {
        Self::from_prefix(bytes, bytes.len())
    }

    /// A value `original_len` bytes long of which `prefix` holds at least the
    /// first `min(original_len, CAPACITY)` bytes (more are ignored).
    pub(crate) fn from_prefix(prefix: &[u8], original_len: usize) -> Self {
        let original_len = u16::try_from(original_len).unwrap_or(u16::MAX);
        let kept = usize::from(original_len).min(Self::CAPACITY);
        debug_assert!(prefix.len() >= kept, "the prefix lacks kept bytes");
        let kept = kept.min(prefix.len());
        let mut buf = [0; Self::CAPACITY];
        buf[..kept].copy_from_slice(&prefix[..kept]);
        Self { original_len, buf }
    }

    /// The kept bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.original_len().min(Self::CAPACITY)]
    }

    /// Length of the value on the wire (after Telnet unescaping).
    pub fn original_len(&self) -> usize {
        usize::from(self.original_len)
    }

    /// `original_len() > CAPACITY`: the bytes after the first
    /// [`CAPACITY`](Self::CAPACITY) were not kept.
    pub fn is_truncated(&self) -> bool {
        self.original_len() > Self::CAPACITY
    }
}

impl Default for InlineBytes {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Deref for InlineBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for InlineBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<&[u8]> for InlineBytes {
    fn from(bytes: &[u8]) -> Self {
        Self::new(bytes)
    }
}

impl<const N: usize> From<&[u8; N]> for InlineBytes {
    fn from(bytes: &[u8; N]) -> Self {
        Self::new(bytes)
    }
}

impl From<&str> for InlineBytes {
    fn from(text: &str) -> Self {
        Self::new(text.as_bytes())
    }
}

/// Equal when the kept bytes and the original lengths are equal.
impl PartialEq for InlineBytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes() && self.original_len == other.original_len
    }
}

impl Eq for InlineBytes {}

impl Hash for InlineBytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
        self.original_len.hash(state);
    }
}

/// Compares the whole value: a truncated value equals no slice.
impl PartialEq<[u8]> for InlineBytes {
    fn eq(&self, other: &[u8]) -> bool {
        !self.is_truncated() && self.as_bytes() == other
    }
}

/// A byte-string literal of the kept bytes, followed by the original length
/// when truncated: `b"ser2net"`, `b"xx…x"...(200 bytes)`.
impl fmt::Debug for InlineBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "b\"{}\"", self.as_bytes().escape_ascii())?;
        if self.is_truncated() {
            write!(f, "...({} bytes)", self.original_len())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;

    fn hash(value: &InlineBytes) -> u64 {
        let mut h = DefaultHasher::new();
        value.hash(&mut h);
        h.finish()
    }

    #[test]
    fn inline_bytes_keeps_64_and_the_length() {
        let long = InlineBytes::new(&[b'x'; 100]);
        assert_eq!(long.as_bytes(), &[b'x'; 64]);
        assert_eq!((long.len(), long.original_len()), (64, 100));
        assert!(long.is_truncated());

        let full = InlineBytes::new(&[b'y'; 64]);
        assert_eq!((full.len(), full.original_len()), (64, 64));
        assert!(!full.is_truncated());

        let short = InlineBytes::from("ser2net");
        assert_eq!(&*short, b"ser2net");
        assert_eq!(short.original_len(), 7);
        assert!(!short.is_truncated());

        assert!(InlineBytes::EMPTY.is_empty());
        assert_eq!(InlineBytes::default(), InlineBytes::EMPTY);
        assert_eq!(InlineBytes::new(b""), InlineBytes::EMPTY);

        // A parser body: 66 bytes kept of a 202-byte body.
        let body = InlineBytes::from_prefix(&[7; 66], 202);
        assert_eq!((body.len(), body.original_len()), (64, 202));

        // The original length saturates.
        let huge = InlineBytes::new(&[0; 70_000]);
        assert_eq!(
            (huge.len(), huge.original_len()),
            (64, usize::from(u16::MAX))
        );
    }

    #[test]
    fn inline_bytes_debug() {
        assert_eq!(
            format!("{:?}", InlineBytes::from("ser2net")),
            r#"b"ser2net""#
        );
        assert_eq!(format!("{:?}", InlineBytes::EMPTY), r#"b"""#);
        assert_eq!(
            format!("{:?}", InlineBytes::new(&[0xFF, b'"', b'\r'])),
            r#"b"\xff\"\r""#
        );
        assert_eq!(
            format!("{:?}", InlineBytes::new(&[b'x'; 200])),
            format!("b\"{}\"...(200 bytes)", "x".repeat(64))
        );
    }

    #[test]
    fn inline_bytes_eq_hash_and_slice_eq() {
        let a = InlineBytes::from(b"abc");
        let b = InlineBytes::from(&b"abc"[..]);
        assert_eq!(a, b);
        assert_eq!(hash(&a), hash(&b));
        assert_ne!(a, InlineBytes::from("abd"));

        // Same kept bytes, different original length: different values.
        let truncated = InlineBytes::new(&[b'x'; 65]);
        let exact = InlineBytes::new(&[b'x'; 64]);
        assert_eq!(truncated.as_bytes(), exact.as_bytes());
        assert_ne!(truncated, exact);
        assert_ne!(hash(&truncated), hash(&exact));

        // Slice equality compares the whole value.
        assert!(a == b"abc"[..]);
        assert!(a != b"ab"[..]);
        assert!(exact == [b'x'; 64][..]);
        assert!(truncated != [b'x'; 64][..]);
    }
}
