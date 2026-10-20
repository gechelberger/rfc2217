//! [`ModemState`] and [`LineState`]: the RFC 2217 modem and line state bytes.

use core::fmt;
use core::ops::{BitAnd, BitOr, BitOrAssign};

/// A newtype over one RFC 2217 state byte: the flag constants (from the high
/// bit down), `from_bits`/`bits`/`contains`/`is_empty`, `|`, `|=`, `&`,
/// `From<u8>` both ways, and a `Debug` that names the flags that are set.
macro_rules! state_byte {
    (
        $(#[$doc:meta])*
        $name:ident {
            $($(#[$flag_doc:meta])* $flag:ident = $bit:literal,)*
        }
    ) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name(u8);

        impl $name {
            $($(#[$flag_doc])* pub const $flag: Self = Self($bit);)*

            /// The value with exactly these bits. Every bit pattern is valid.
            pub const fn from_bits(bits: u8) -> Self {
                Self(bits)
            }

            /// The byte as it is sent on the wire.
            pub const fn bits(self) -> u8 {
                self.0
            }

            /// `true` if every bit of `other` is set in `self`.
            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }

            /// `true` if no bit is set.
            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }
        }

        impl BitOr for $name {
            type Output = Self;

            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }

        impl BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl BitAnd for $name {
            type Output = Self;

            fn bitand(self, rhs: Self) -> Self {
                Self(self.0 & rhs.0)
            }
        }

        impl From<u8> for $name {
            fn from(bits: u8) -> Self {
                Self(bits)
            }
        }

        impl From<$name> for u8 {
            fn from(state: $name) -> u8 {
                state.0
            }
        }

        /// The names of the flags that are set, from the high bit down:
        #[doc = concat!("`", stringify!($name), "(A | B)`, or `", stringify!($name), "(empty)`.")]
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "("))?;
                let mut first = true;
                for (name, bit) in [$((stringify!($flag), $bit)),*] {
                    if self.0 & bit != 0 {
                        if !first {
                            f.write_str(" | ")?;
                        }
                        f.write_str(name)?;
                        first = false;
                    }
                }
                if first {
                    f.write_str("empty")?;
                }
                f.write_str(")")
            }
        }
    };
}

state_byte! {
    /// The modem state byte of RFC 2217 (`NOTIFY-MODEMSTATE`,
    /// `SET-MODEMSTATE-MASK`): the levels of the four modem input lines in
    /// the high nibble, and what changed since the previous notification in
    /// the low nibble. Every bit pattern is valid and round-trips.
    ///
    /// ```
    /// use rfc2217::ModemState;
    ///
    /// let state = ModemState::CTS | ModemState::DSR | ModemState::DELTA_CTS;
    /// assert_eq!(state.bits(), 0x31);
    /// assert!(state.cts() && state.dsr() && !state.cd());
    /// assert!(state.contains(ModemState::DELTA_CTS));
    /// assert_eq!(format!("{state:?}"), "ModemState(DSR | CTS | DELTA_CTS)");
    /// ```
    ModemState {
        /// `0x80`: carrier detect (receive line signal detect) is on.
        CD = 0x80,
        /// `0x40`: ring indicator is on.
        RI = 0x40,
        /// `0x20`: data set ready is on.
        DSR = 0x20,
        /// `0x10`: clear to send is on.
        CTS = 0x10,
        /// `0x08`: carrier detect changed.
        DELTA_CD = 0x08,
        /// `0x04`: ring indicator went from on to off.
        TRAILING_EDGE_RI = 0x04,
        /// `0x02`: data set ready changed.
        DELTA_DSR = 0x02,
        /// `0x01`: clear to send changed.
        DELTA_CTS = 0x01,
    }
}

impl ModemState {
    /// Carrier detect is on (the level, not the change flag).
    pub const fn cd(self) -> bool {
        self.contains(Self::CD)
    }

    /// Ring indicator is on (the level, not the change flag).
    pub const fn ri(self) -> bool {
        self.contains(Self::RI)
    }

    /// Data set ready is on (the level, not the change flag).
    pub const fn dsr(self) -> bool {
        self.contains(Self::DSR)
    }

    /// Clear to send is on (the level, not the change flag).
    pub const fn cts(self) -> bool {
        self.contains(Self::CTS)
    }
}

state_byte! {
    /// The line state byte of RFC 2217 (`NOTIFY-LINESTATE`,
    /// `SET-LINESTATE-MASK`): the serial port's error and status flags.
    /// Every bit pattern is valid and round-trips.
    LineState {
        /// `0x80`: time-out error.
        TIMEOUT_ERROR = 0x80,
        /// `0x40`: transfer shift register empty.
        SHIFT_REGISTER_EMPTY = 0x40,
        /// `0x20`: transfer holding register empty.
        HOLDING_REGISTER_EMPTY = 0x20,
        /// `0x10`: break detected.
        BREAK_DETECT = 0x10,
        /// `0x08`: framing error.
        FRAMING_ERROR = 0x08,
        /// `0x04`: parity error.
        PARITY_ERROR = 0x04,
        /// `0x02`: overrun error.
        OVERRUN_ERROR = 0x02,
        /// `0x01`: data ready.
        DATA_READY = 0x01,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modem_state_debug_lists_flags() {
        let debug = |bits| format!("{:?}", ModemState::from_bits(bits));
        assert_eq!(debug(0x00), "ModemState(empty)");
        assert_eq!(format!("{:?}", ModemState::default()), "ModemState(empty)");
        assert_eq!(debug(0xB0), "ModemState(CD | DSR | CTS)");
        assert_eq!(debug(0x31), "ModemState(DSR | CTS | DELTA_CTS)");
        assert_eq!(debug(0x04), "ModemState(TRAILING_EDGE_RI)");
        assert_eq!(
            debug(0xFF),
            "ModemState(CD | RI | DSR | CTS | DELTA_CD | TRAILING_EDGE_RI | DELTA_DSR | DELTA_CTS)"
        );
    }

    #[test]
    fn line_state_debug_lists_flags() {
        let debug = |bits| format!("{:?}", LineState::from_bits(bits));
        assert_eq!(debug(0x00), "LineState(empty)");
        assert_eq!(format!("{:?}", LineState::default()), "LineState(empty)");
        assert_eq!(
            debug(0x61),
            "LineState(SHIFT_REGISTER_EMPTY | HOLDING_REGISTER_EMPTY | DATA_READY)"
        );
        assert_eq!(
            debug(0x1C),
            "LineState(BREAK_DETECT | FRAMING_ERROR | PARITY_ERROR)"
        );
        assert_eq!(
            debug(0xFF),
            "LineState(TIMEOUT_ERROR | SHIFT_REGISTER_EMPTY | HOLDING_REGISTER_EMPTY | \
             BREAK_DETECT | FRAMING_ERROR | PARITY_ERROR | OVERRUN_ERROR | DATA_READY)"
        );
    }

    #[test]
    fn bits_round_trip_for_every_value() {
        for bits in 0..=u8::MAX {
            let modem = ModemState::from_bits(bits);
            assert_eq!(modem.bits(), bits);
            assert_eq!(u8::from(modem), bits);
            assert_eq!(ModemState::from(bits), modem);
            assert_eq!(modem.is_empty(), bits == 0);

            let line = LineState::from_bits(bits);
            assert_eq!(line.bits(), bits);
            assert_eq!(u8::from(line), bits);
            assert_eq!(LineState::from(bits), line);
            assert_eq!(line.is_empty(), bits == 0);
        }

        // Each flag is its own bit, from 0x80 down.
        let modem = [
            ModemState::CD,
            ModemState::RI,
            ModemState::DSR,
            ModemState::CTS,
            ModemState::DELTA_CD,
            ModemState::TRAILING_EDGE_RI,
            ModemState::DELTA_DSR,
            ModemState::DELTA_CTS,
        ];
        let line = [
            LineState::TIMEOUT_ERROR,
            LineState::SHIFT_REGISTER_EMPTY,
            LineState::HOLDING_REGISTER_EMPTY,
            LineState::BREAK_DETECT,
            LineState::FRAMING_ERROR,
            LineState::PARITY_ERROR,
            LineState::OVERRUN_ERROR,
            LineState::DATA_READY,
        ];
        for (i, (m, l)) in modem.into_iter().zip(line).enumerate() {
            assert_eq!(m.bits(), 0x80 >> i);
            assert_eq!(l.bits(), 0x80 >> i);
        }

        // The operators work on the bits.
        let mut state = ModemState::CTS | ModemState::DSR;
        assert_eq!(state.bits(), 0x30);
        assert!(state.contains(ModemState::CTS));
        assert!(state.contains(ModemState::CTS | ModemState::DSR));
        assert!(!state.contains(ModemState::CTS | ModemState::CD));
        assert!(state.contains(ModemState::default()));
        state |= ModemState::DELTA_CTS;
        assert_eq!(state.bits(), 0x31);
        assert_eq!((state & ModemState::from_bits(0x0F)).bits(), 0x01);
        assert!((state & ModemState::RI).is_empty());

        let line = LineState::BREAK_DETECT | LineState::DATA_READY;
        assert_eq!((line & LineState::from_bits(0xF0)), LineState::BREAK_DETECT);
    }

    #[test]
    fn modem_level_accessors() {
        let levels = |state: ModemState| (state.cd(), state.ri(), state.dsr(), state.cts());
        assert_eq!(levels(ModemState::CD), (true, false, false, false));
        assert_eq!(levels(ModemState::RI), (false, true, false, false));
        assert_eq!(levels(ModemState::DSR), (false, false, true, false));
        assert_eq!(levels(ModemState::CTS), (false, false, false, true));
        assert_eq!(
            levels(ModemState::from_bits(0xF0)),
            (true, true, true, true)
        );
        // The change flags are not levels.
        assert_eq!(
            levels(ModemState::from_bits(0x0F)),
            (false, false, false, false)
        );
        assert_eq!(
            levels(ModemState::from_bits(0x31)),
            (false, false, true, true)
        );
    }
}
