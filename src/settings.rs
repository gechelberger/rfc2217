//! Serial line parameters and their RFC 2217 encodings.
//!
//! The value enums are exhaustive on purpose: RFC 2217 defines a closed set
//! of values for each, so a `match` over them needs no wildcard arm.

use core::fmt;

/// Data bits per character (`SET-DATASIZE`). Exhaustive: RFC 2217 defines
/// exactly these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataBits {
    /// 5 bits.
    Five,
    /// 6 bits.
    Six,
    /// 7 bits.
    Seven,
    /// 8 bits.
    Eight,
}

impl DataBits {
    /// Wire code (the bit count itself).
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Five => 5,
            Self::Six => 6,
            Self::Seven => 7,
            Self::Eight => 8,
        }
    }

    /// Decode a wire code.
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            5 => Self::Five,
            6 => Self::Six,
            7 => Self::Seven,
            8 => Self::Eight,
            _ => return None,
        })
    }
}

/// Parity (`SET-PARITY`). Exhaustive: RFC 2217 defines exactly these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Parity {
    /// No parity bit.
    None,
    /// Odd parity.
    Odd,
    /// Even parity.
    Even,
    /// Parity bit always 1.
    Mark,
    /// Parity bit always 0.
    Space,
}

impl Parity {
    /// Wire code.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::None => 1,
            Self::Odd => 2,
            Self::Even => 3,
            Self::Mark => 4,
            Self::Space => 5,
        }
    }

    /// Decode a wire code.
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::None,
            2 => Self::Odd,
            3 => Self::Even,
            4 => Self::Mark,
            5 => Self::Space,
            _ => return None,
        })
    }
}

/// Stop bits (`SET-STOPSIZE`). Exhaustive: RFC 2217 defines exactly these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopBits {
    /// One stop bit.
    One,
    /// Two stop bits.
    Two,
    /// One and a half stop bits.
    OnePointFive,
}

impl StopBits {
    /// Wire code.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::OnePointFive => 3,
        }
    }

    /// Decode a wire code.
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::One,
            2 => Self::Two,
            3 => Self::OnePointFive,
            _ => return None,
        })
    }
}

/// Outbound (or both directions) flow control: the `SET-CONTROL` values 1,
/// 2, 3, 17 and 19. Inbound flow control (values 13 to 16 and 18) is not
/// modelled in 0.1: the client never sets it, and the [`Server`] answers
/// inbound requests with the inbound value its outbound setting implies.
///
/// [`Server`]: crate::Server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlowControl {
    /// No flow control.
    None,
    /// XON / XOFF software flow control.
    XonXoff,
    /// RTS / CTS hardware flow control.
    RtsCts,
    /// DCD flow control.
    Dcd,
    /// DSR flow control.
    Dsr,
}

impl FlowControl {
    /// Wire code.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::None => 1,
            Self::XonXoff => 2,
            Self::RtsCts => 3,
            Self::Dcd => 17,
            Self::Dsr => 19,
        }
    }

    /// Decode a wire code.
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::None,
            2 => Self::XonXoff,
            3 => Self::RtsCts,
            17 => Self::Dcd,
            19 => Self::Dsr,
            _ => return None,
        })
    }
}

/// Which buffer a `PURGE-DATA` request clears, seen from the bridge.
/// Exhaustive: RFC 2217 defines exactly these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purge {
    /// Data received from the serial port but not yet sent to the client.
    Receive,
    /// Data received from the client but not yet written to the serial port.
    Transmit,
    /// Both.
    Both,
}

impl Purge {
    /// Wire code.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Receive => 1,
            Self::Transmit => 2,
            Self::Both => 3,
        }
    }

    /// Decode a wire code.
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Receive,
            2 => Self::Transmit,
            3 => Self::Both,
            _ => return None,
        })
    }
}

/// Complete configuration of the remote serial port: the five values one
/// RFC 2217 settings round carries.
///
/// The struct is exhaustive on purpose: RFC 2217's record is closed, so
/// struct literals (with `..PortSettings::default()` for the rest) and
/// destructuring work outside the crate. Client options that are not port
/// settings are meant to come through new constructors, not new fields.
///
/// ```
/// use rfc2217::{DataBits, FlowControl, Parity, PortSettings, StopBits};
///
/// // Every field ...
/// let modbus = PortSettings {
///     baud_rate: 19_200,
///     data_bits: DataBits::Eight,
///     parity: Parity::Even,
///     stop_bits: StopBits::One,
///     flow_control: FlowControl::None,
/// };
/// // ... or only those that differ from 9600 8N1 without flow control.
/// assert_eq!(modbus, PortSettings { baud_rate: 19_200, parity: Parity::Even, ..Default::default() });
///
/// // Baud rate 0 asks the bridge for its current rate instead of setting one.
/// let keep_rate = PortSettings { baud_rate: 0, ..Default::default() };
/// # let _ = keep_rate;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortSettings {
    /// Baud rate. 0 is the RFC 2217 query value: the client asks the bridge
    /// for its current rate instead of setting one, and any rate the bridge
    /// reports confirms it (see [`Client::reported_settings`]).
    ///
    /// [`Client::reported_settings`]: crate::Client::reported_settings
    pub baud_rate: u32,
    /// Data bits.
    pub data_bits: DataBits,
    /// Parity.
    pub parity: Parity,
    /// Stop bits.
    pub stop_bits: StopBits,
    /// Outbound (or both directions) flow control: `SET-CONTROL` values 1,
    /// 2, 3, 17 and 19. Inbound flow control is not modelled in 0.1.
    pub flow_control: FlowControl,
}

impl PortSettings {
    /// The wire value of `setting`: the baud rate itself (0 = query), or the
    /// setting's RFC 2217 code.
    pub(crate) fn wire(&self, setting: Setting) -> u32 {
        match setting {
            Setting::BaudRate => self.baud_rate,
            Setting::DataBits => self.data_bits.code().into(),
            Setting::Parity => self.parity.code().into(),
            Setting::StopBits => self.stop_bits.code().into(),
            Setting::FlowControl => self.flow_control.code().into(),
        }
    }
}

impl Default for PortSettings {
    /// 9600 baud, 8 data bits, no parity, 1 stop bit, no flow control.
    fn default() -> Self {
        Self {
            baud_rate: 9600,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
        }
    }
}

/// One optional value per [`PortSettings`] field. In 0.1 it is the snapshot
/// of the values the bridge reported ([`Client::reported_settings`]): `None`
/// for a setting the bridge has not reported a decodable value for. It is
/// also the type a future query/adopt-current request will use (`None` =
/// query).
///
/// [`Client::reported_settings`]: crate::Client::reported_settings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PartialSettings {
    /// Baud rate.
    pub baud_rate: Option<u32>,
    /// Data bits.
    pub data_bits: Option<DataBits>,
    /// Parity.
    pub parity: Option<Parity>,
    /// Stop bits.
    pub stop_bits: Option<StopBits>,
    /// Flow control.
    pub flow_control: Option<FlowControl>,
}

impl PartialSettings {
    /// All five fields as a [`PortSettings`], if every one is present.
    pub const fn complete(&self) -> Option<PortSettings> {
        match *self {
            Self {
                baud_rate: Some(baud_rate),
                data_bits: Some(data_bits),
                parity: Some(parity),
                stop_bits: Some(stop_bits),
                flow_control: Some(flow_control),
            } => Some(PortSettings {
                baud_rate,
                data_bits,
                parity,
                stop_bits,
                flow_control,
            }),
            _ => None,
        }
    }

    /// The wire value of `setting` (as [`PortSettings::wire`]), if present.
    pub(crate) fn wire(&self, setting: Setting) -> Option<u32> {
        match setting {
            Setting::BaudRate => self.baud_rate,
            Setting::DataBits => self.data_bits.map(|v| v.code().into()),
            Setting::Parity => self.parity.map(|v| v.code().into()),
            Setting::StopBits => self.stop_bits.map(|v| v.code().into()),
            Setting::FlowControl => self.flow_control.map(|v| v.code().into()),
        }
    }
}

/// Names one [`PortSettings`] field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Setting {
    /// [`PortSettings::baud_rate`] (`SET-BAUDRATE`).
    BaudRate,
    /// [`PortSettings::data_bits`] (`SET-DATASIZE`).
    DataBits,
    /// [`PortSettings::parity`] (`SET-PARITY`).
    Parity,
    /// [`PortSettings::stop_bits`] (`SET-STOPSIZE`).
    StopBits,
    /// [`PortSettings::flow_control`] (`SET-CONTROL` with a flow control value).
    FlowControl,
}

impl Setting {
    /// Every setting, in RFC order (the order of the settings round).
    pub(crate) const ALL: [Self; 5] = [
        Self::BaudRate,
        Self::DataBits,
        Self::Parity,
        Self::StopBits,
        Self::FlowControl,
    ];

    /// This setting's bit in a [`SettingSet`].
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// Lower-case names for messages: `"baud rate"`, `"data bits"`, `"parity"`,
/// `"stop bits"`, `"flow control"`.
impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BaudRate => "baud rate",
            Self::DataBits => "data bits",
            Self::Parity => "parity",
            Self::StopBits => "stop bits",
            Self::FlowControl => "flow control",
        })
    }
}

/// A set of [`Setting`]s: `Copy`, no allocation. Iterates in RFC order:
/// baud rate, data bits, parity, stop bits, flow control. Build one with
/// `collect()`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SettingSet(u8);

impl SettingSet {
    /// `true` if the set holds no setting.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `true` if `setting` is in the set.
    pub const fn contains(self, setting: Setting) -> bool {
        self.0 & setting.bit() != 0
    }

    /// The settings in the set, in RFC order.
    pub fn iter(self) -> impl Iterator<Item = Setting> {
        Setting::ALL
            .into_iter()
            .filter(move |&setting| self.contains(setting))
    }
}

impl FromIterator<Setting> for SettingSet {
    fn from_iter<I: IntoIterator<Item = Setting>>(iter: I) -> Self {
        let mut bits = 0;
        for setting in iter {
            bits |= setting.bit();
        }
        Self(bits)
    }
}

/// The variant names in RFC order: `{DataBits, Parity}`.
impl fmt::Debug for SettingSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// The settings' [`Display`](fmt::Display) names in RFC order, separated by
/// commas: `"data bits, parity"`. The empty set prints nothing.
impl fmt::Display for SettingSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, setting) in self.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            fmt::Display::fmt(&setting, f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `code` and `from_code` agree with `table` both ways, and every byte
    /// not in `table` decodes to `None`.
    fn check<T: Copy + PartialEq + core::fmt::Debug>(
        table: &[(T, u8)],
        code: fn(T) -> u8,
        from_code: fn(u8) -> Option<T>,
    ) {
        for &(value, wire) in table {
            assert_eq!(code(value), wire, "{value:?}");
            assert_eq!(from_code(wire), Some(value), "{wire}");
        }
        for wire in 0..=u8::MAX {
            if table.iter().all(|&(_, w)| w != wire) {
                assert_eq!(from_code(wire), None, "{wire}");
            }
        }
    }

    #[test]
    fn wire_codes_match_rfc2217() {
        use DataBits as D;
        use FlowControl as F;
        use Parity as P;
        use StopBits as S;

        // Exhaustive matches: a new variant does not compile until it is
        // listed here, and so in its table below.
        let _ = |d: D| match d {
            D::Five | D::Six | D::Seven | D::Eight => {}
        };
        let _ = |p: P| match p {
            P::None | P::Odd | P::Even | P::Mark | P::Space => {}
        };
        let _ = |s: S| match s {
            S::One | S::Two | S::OnePointFive => {}
        };
        let _ = |f: F| match f {
            F::None | F::XonXoff | F::RtsCts | F::Dcd | F::Dsr => {}
        };
        let _ = |purge: Purge| match purge {
            Purge::Receive | Purge::Transmit | Purge::Both => {}
        };

        check(
            &[(D::Five, 5), (D::Six, 6), (D::Seven, 7), (D::Eight, 8)],
            D::code,
            D::from_code,
        );
        check(
            &[
                (P::None, 1),
                (P::Odd, 2),
                (P::Even, 3),
                (P::Mark, 4),
                (P::Space, 5),
            ],
            P::code,
            P::from_code,
        );
        check(
            &[(S::One, 1), (S::Two, 2), (S::OnePointFive, 3)],
            S::code,
            S::from_code,
        );
        check(
            &[
                (F::None, 1),
                (F::XonXoff, 2),
                (F::RtsCts, 3),
                (F::Dcd, 17),
                (F::Dsr, 19),
            ],
            F::code,
            F::from_code,
        );
        check(
            &[(Purge::Receive, 1), (Purge::Transmit, 2), (Purge::Both, 3)],
            Purge::code,
            Purge::from_code,
        );

        // Spelled out: 0 is a query, not a value; 13-16 and 18 are inbound
        // flow control codes, which `FlowControl` does not model.
        assert_eq!(D::from_code(0), None);
        assert_eq!(P::from_code(0), None);
        assert_eq!(S::from_code(0), None);
        assert_eq!(Purge::from_code(0), None);
        for wire in [0, 13, 14, 15, 16, 18] {
            assert_eq!(F::from_code(wire), None, "{wire}");
        }
    }

    #[test]
    fn setting_display_names() {
        let names: Vec<String> = Setting::ALL.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            names,
            [
                "baud rate",
                "data bits",
                "parity",
                "stop bits",
                "flow control"
            ]
        );
        // Exhaustive: a new setting does not compile until it is named here.
        // It must then be added to `ALL` (and to the names above) by hand:
        // the readiness sets iterate `ALL` and would silently skip it.
        let _ = |s: Setting| match s {
            Setting::BaudRate
            | Setting::DataBits
            | Setting::Parity
            | Setting::StopBits
            | Setting::FlowControl => {}
        };
    }

    #[test]
    fn setting_set_iter_contains_display_debug() {
        let empty = SettingSet::default();
        assert!(empty.is_empty());
        assert_eq!(empty.iter().count(), 0);
        assert_eq!(empty.to_string(), "");
        assert_eq!(format!("{empty:?}"), "{}");

        // Collected in any order, with repeats; iterated in RFC order.
        let set: SettingSet = [Setting::Parity, Setting::DataBits, Setting::Parity]
            .into_iter()
            .collect();
        assert!(!set.is_empty());
        assert!(set.contains(Setting::DataBits) && set.contains(Setting::Parity));
        for absent in [Setting::BaudRate, Setting::StopBits, Setting::FlowControl] {
            assert!(!set.contains(absent), "{absent:?}");
        }
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            [Setting::DataBits, Setting::Parity]
        );
        assert_eq!(set.to_string(), "data bits, parity");
        assert_eq!(format!("{set:?}"), "{DataBits, Parity}");
        assert_eq!(
            set,
            [Setting::DataBits, Setting::Parity].into_iter().collect()
        );

        let all: SettingSet = Setting::ALL.into_iter().rev().collect();
        assert_eq!(all.iter().collect::<Vec<_>>(), Setting::ALL);
        assert_eq!(
            all.to_string(),
            "baud rate, data bits, parity, stop bits, flow control"
        );
        assert_eq!(
            format!("{all:?}"),
            "{BaudRate, DataBits, Parity, StopBits, FlowControl}"
        );

        // Each setting alone.
        for setting in Setting::ALL {
            let one: SettingSet = core::iter::once(setting).collect();
            assert_eq!(one.iter().collect::<Vec<_>>(), [setting]);
            assert_eq!(one.to_string(), setting.to_string());
            assert_eq!(format!("{one:?}"), format!("{{{setting:?}}}"));
        }
    }

    #[test]
    fn partial_settings_complete() {
        let full = PortSettings {
            baud_rate: 115_200,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::RtsCts,
        };
        let reported = PartialSettings {
            baud_rate: Some(full.baud_rate),
            data_bits: Some(full.data_bits),
            parity: Some(full.parity),
            stop_bits: Some(full.stop_bits),
            flow_control: Some(full.flow_control),
        };
        assert_eq!(reported.complete(), Some(full));

        assert_eq!(PartialSettings::default().complete(), None);
        let missing = [
            PartialSettings {
                baud_rate: None,
                ..reported
            },
            PartialSettings {
                data_bits: None,
                ..reported
            },
            PartialSettings {
                parity: None,
                ..reported
            },
            PartialSettings {
                stop_bits: None,
                ..reported
            },
            PartialSettings {
                flow_control: None,
                ..reported
            },
        ];
        for partial in missing {
            assert_eq!(partial.complete(), None, "{partial:?}");
        }

        // Usable in constants.
        const NONE: Option<PortSettings> = PartialSettings {
            baud_rate: Some(9600),
            data_bits: None,
            parity: None,
            stop_bits: None,
            flow_control: None,
        }
        .complete();
        assert_eq!(NONE, None);
    }
}
