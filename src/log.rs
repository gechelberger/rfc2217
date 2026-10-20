//! Private logging macros. With the `tracing` feature they forward to
//! `tracing::debug!`/`tracing::trace!`; without it they type-check their
//! arguments and emit nothing. Call sites use plain format-string syntax only.
//! Logs are not part of the API contract.

/// Something unusual the peer did that the crate tolerated or ignored.
macro_rules! debug {
    ($($arg:tt)*) => {{
        #[cfg(feature = "tracing")]
        ::tracing::debug!($($arg)*);
        #[cfg(not(feature = "tracing"))]
        if false {
            let _ = ::core::format_args!($($arg)*);
        }
    }};
}

/// Routine protocol traffic (decoded events and requests).
macro_rules! trace {
    ($($arg:tt)*) => {{
        #[cfg(feature = "tracing")]
        ::tracing::trace!($($arg)*);
        #[cfg(not(feature = "tracing"))]
        if false {
            let _ = ::core::format_args!($($arg)*);
        }
    }};
}

pub(crate) use debug;
pub(crate) use trace;
